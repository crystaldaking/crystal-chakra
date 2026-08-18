#!/usr/bin/env python3
"""Run bounded, provider-neutral LSP probes against the PHP evaluation corpus.

This is an opt-in evaluation tool, not a Chakra runtime dependency. It uses
only the Python standard library and writes no files inside the evaluated
workspace. Pass the language-server command after `--`.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse


JsonObject = dict[str, Any]
MAX_MESSAGE_BYTES = 8 * 1024 * 1024


class TransportClosed(RuntimeError):
    """Raised when the provider closes its JSON-RPC transport."""


class LspSession:
    def __init__(
        self,
        command: list[str],
        root: Path,
        request_timeout: float,
        settings: JsonObject,
    ) -> None:
        self.command = command
        self.root = root
        self.request_timeout = request_timeout
        self.settings = settings
        self.process = subprocess.Popen(
            command,
            cwd=root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=os.name == "posix",
        )
        self._messages: queue.Queue[JsonObject] = queue.Queue(maxsize=1024)
        self._next_id = 1
        self._write_lock = threading.Lock()
        self._stderr_lock = threading.Lock()
        self._stderr = bytearray()
        self.notifications: list[str] = []
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        try:
            while True:
                headers: dict[str, str] = {}
                while True:
                    line = self.process.stdout.readline()
                    if not line:
                        raise TransportClosed("provider stdout closed")
                    if line in (b"\r\n", b"\n"):
                        break
                    name, separator, value = line.decode("ascii").partition(":")
                    if not separator:
                        raise TransportClosed(f"invalid LSP header: {line!r}")
                    headers[name.lower()] = value.strip()
                length = int(headers["content-length"])
                if length < 0 or length > MAX_MESSAGE_BYTES:
                    raise TransportClosed(f"LSP message exceeds {MAX_MESSAGE_BYTES} bytes")
                payload = self.process.stdout.read(length)
                if len(payload) != length:
                    raise TransportClosed("provider closed during an LSP message")
                message = json.loads(payload)
                if isinstance(message, dict):
                    self._messages.put(message)
        except Exception as error:  # The main thread reports transport detail.
            self._messages.put({"_transport_error": str(error)})

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        while True:
            chunk = self.process.stderr.read(4096)
            if not chunk:
                return
            with self._stderr_lock:
                self._stderr.extend(chunk)
                if len(self._stderr) > 65_536:
                    del self._stderr[:-65_536]

    def stderr_tail(self) -> str:
        with self._stderr_lock:
            return bytes(self._stderr).decode("utf-8", errors="replace")

    def send(self, message: JsonObject) -> None:
        encoded = json.dumps(message, separators=(",", ":")).encode("utf-8")
        frame = f"Content-Length: {len(encoded)}\r\n\r\n".encode("ascii") + encoded
        assert self.process.stdin is not None
        with self._write_lock:
            try:
                self.process.stdin.write(frame)
                self.process.stdin.flush()
            except (BrokenPipeError, OSError) as error:
                raise TransportClosed(str(error)) from error

    def notify(self, method: str, params: JsonObject | list[Any] | None = None) -> None:
        message: JsonObject = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)

    def _answer_server_request(self, message: JsonObject) -> None:
        method = message.get("method")
        params = message.get("params") or {}
        if method == "workspace/configuration":
            items = params.get("items", []) if isinstance(params, dict) else []
            result: Any = []
            for item in items:
                value: Any = self.settings
                section = item.get("section") if isinstance(item, dict) else None
                if isinstance(section, str):
                    for segment in section.split("."):
                        if not isinstance(value, dict) or segment not in value:
                            value = {}
                            break
                        value = value[segment]
                result.append(value)
        elif method == "workspace/workspaceFolders":
            result = [{"uri": self.root.as_uri(), "name": self.root.name}]
        elif method == "workspace/applyEdit":
            result = {"applied": False, "failureReason": "read-only evaluation client"}
        else:
            result = None
        self.send({"jsonrpc": "2.0", "id": message["id"], "result": result})

    def request(
        self,
        method: str,
        params: JsonObject | list[Any] | None,
        timeout: float | None = None,
        cancel_immediately: bool = False,
    ) -> JsonObject:
        request_id = self._next_id
        self._next_id += 1
        message: JsonObject = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
        }
        if params is not None:
            message["params"] = params
        self.send(message)
        if cancel_immediately:
            self.notify("$/cancelRequest", {"id": request_id})
        deadline = time.monotonic() + (timeout or self.request_timeout)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for {method}")
            try:
                response = self._messages.get(timeout=remaining)
            except queue.Empty as error:
                raise TimeoutError(f"timed out waiting for {method}") from error
            if "_transport_error" in response:
                raise TransportClosed(str(response["_transport_error"]))
            if "method" in response and "id" in response:
                self._answer_server_request(response)
                continue
            if "method" in response:
                notification = str(response["method"])
                if len(self.notifications) < 512:
                    self.notifications.append(notification)
                continue
            if response.get("id") == request_id:
                return response

    def rss_kib(self) -> int | None:
        try:
            value = subprocess.check_output(
                ["ps", "-o", "rss=", "-p", str(self.process.pid)],
                text=True,
                timeout=2,
            ).strip()
            return int(value) if value else None
        except (OSError, ValueError, subprocess.SubprocessError):
            return None

    def kill(self) -> float:
        started = time.monotonic()
        if self.process.poll() is None:
            if os.name == "posix":
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                self.process.kill()
        self.process.wait(timeout=5)
        return (time.monotonic() - started) * 1000.0

    def shutdown(self) -> JsonObject:
        result: JsonObject = {"clean": False}
        try:
            response = self.request("shutdown", None, timeout=5)
            result["response"] = response
            self.notify("exit")
            self.process.wait(timeout=5)
            result["clean"] = self.process.returncode == 0
        except Exception as error:
            result["error"] = str(error)
            if self.process.poll() is None:
                if os.name == "posix":
                    try:
                        os.killpg(self.process.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                else:
                    self.process.terminate()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    if os.name == "posix":
                        try:
                            os.killpg(self.process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    else:
                        self.process.kill()
                    self.process.wait(timeout=2)
        result["exit_code"] = self.process.returncode
        result["stderr_tail"] = self.stderr_tail()[-4096:]
        return result


def load_json_object(raw: str | None) -> JsonObject:
    if raw is None:
        return {}
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("JSON option must contain an object")
    return value


def utf16_position(
    source: str,
    needle: str,
    symbol: str,
    occurrence: int = 1,
    symbol_occurrence: int = 1,
) -> JsonObject:
    offset = -1
    search_from = 0
    for _ in range(occurrence):
        offset = source.find(needle, search_from)
        if offset < 0:
            raise ValueError(f"could not find occurrence {occurrence} of {needle!r}")
        search_from = offset + len(needle)
    symbol_offset = -1
    symbol_search_from = 0
    for _ in range(symbol_occurrence):
        symbol_offset = needle.find(symbol, symbol_search_from)
        if symbol_offset < 0:
            raise ValueError(
                f"could not find symbol occurrence {symbol_occurrence} "
                f"of {symbol!r} inside {needle!r}"
            )
        symbol_search_from = symbol_offset + len(symbol)
    absolute = offset + symbol_offset
    before = source[:absolute]
    line = before.count("\n")
    line_start = before.rfind("\n") + 1
    character = len(source[line_start:absolute].encode("utf-16-le")) // 2
    return {"line": line, "character": character}


def text_document_position(root: Path, probe: JsonObject) -> tuple[JsonObject, str]:
    path = root / str(probe["file"])
    source = path.read_text(encoding="utf-8")
    position = utf16_position(
        source,
        str(probe["needle"]),
        str(probe["symbol"]),
        int(probe.get("occurrence", 1)),
        int(probe.get("symbol_occurrence", 1)),
    )
    return {"textDocument": {"uri": path.as_uri()}, "position": position}, source


def timed_request(
    session: LspSession,
    method: str,
    params: JsonObject | list[Any] | None,
    timeout: float | None = None,
    cancel_immediately: bool = False,
) -> JsonObject:
    started = time.monotonic()
    try:
        response = session.request(
            method,
            params,
            timeout=timeout,
            cancel_immediately=cancel_immediately,
        )
        status = "error" if "error" in response else "ok"
        error = response.get("error")
        if isinstance(error, dict) and isinstance(error.get("data"), str):
            error = dict(error)
            error["data"] = error["data"][:2048]
        return {
            "status": status,
            "latency_ms": (time.monotonic() - started) * 1000.0,
            "response_bytes": len(json.dumps(response, separators=(",", ":")).encode()),
            "result": response.get("result"),
            "error": error,
        }
    except (TimeoutError, TransportClosed) as error:
        return {
            "status": "timeout" if isinstance(error, TimeoutError) else "transport_closed",
            "latency_ms": (time.monotonic() - started) * 1000.0,
            "response_bytes": 0,
            "error": str(error),
        }


def location_uris(value: Any) -> list[str]:
    uris: list[str] = []
    if isinstance(value, dict):
        for key in ("uri", "targetUri"):
            if isinstance(value.get(key), str):
                uris.append(value[key])
        for child in value.values():
            uris.extend(location_uris(child))
    elif isinstance(value, list):
        for child in value:
            uris.extend(location_uris(child))
    return list(dict.fromkeys(uris))


def location_count(value: Any) -> int:
    if isinstance(value, dict):
        if isinstance(value.get("uri"), str) or isinstance(value.get("targetUri"), str):
            return 1
        return sum(location_count(child) for child in value.values())
    if isinstance(value, list):
        return sum(location_count(child) for child in value)
    return 0


def target_locations(value: Any) -> list[tuple[str, JsonObject]]:
    locations: list[tuple[str, JsonObject]] = []
    if isinstance(value, dict):
        if isinstance(value.get("targetUri"), str):
            target_range = value.get("targetSelectionRange") or value.get("targetRange")
            if isinstance(target_range, dict):
                locations.append((value["targetUri"], target_range))
                return locations
        if isinstance(value.get("uri"), str) and isinstance(value.get("range"), dict):
            locations.append((value["uri"], value["range"]))
            return locations
        for child in value.values():
            locations.extend(target_locations(child))
    elif isinstance(value, list):
        for child in value:
            locations.extend(target_locations(child))
    return locations


def range_contains(source_range: JsonObject, position: JsonObject) -> bool:
    start = source_range.get("start")
    end = source_range.get("end")
    if not isinstance(start, dict) or not isinstance(end, dict):
        return False
    point = (int(position["line"]), int(position["character"]))
    lower = (int(start["line"]), int(start["character"]))
    upper = (int(end["line"]), int(end["character"]))
    return lower <= point <= upper


def uri_matches(root: Path, uri: str, relative: str) -> bool:
    parsed = urlparse(uri)
    if parsed.scheme != "file":
        return False
    actual = Path(unquote(parsed.path)).resolve()
    return actual == (root / relative).resolve()


def initialize(
    session: LspSession,
    provider: str,
    initialization_options: JsonObject,
    timeout: float,
) -> JsonObject:
    params: JsonObject = {
        "processId": None,
        "clientInfo": {"name": "chakra-php-provider-evaluation", "version": "1"},
        "rootUri": session.root.as_uri(),
        "workspaceFolders": [{"uri": session.root.as_uri(), "name": session.root.name}],
        "capabilities": {
            "workspace": {
                "configuration": True,
                "workspaceFolders": True,
                "symbol": {"dynamicRegistration": False},
            },
            "textDocument": {
                "synchronization": {"dynamicRegistration": False, "didSave": True},
                "definition": {"dynamicRegistration": False, "linkSupport": True},
                "references": {"dynamicRegistration": False},
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                "callHierarchy": {"dynamicRegistration": False},
            },
            "window": {"workDoneProgress": True},
        },
        "initializationOptions": initialization_options,
        "trace": "off",
    }
    measured = timed_request(session, "initialize", params, timeout=timeout)
    if measured["status"] != "ok":
        raise RuntimeError(f"{provider} initialize failed: {measured}")
    session.notify("initialized", {})
    capabilities = (measured.get("result") or {}).get("capabilities", {})
    return {
        "latency_ms": measured["latency_ms"],
        "response_bytes": measured["response_bytes"],
        "server_info": (measured.get("result") or {}).get("serverInfo"),
        "capabilities": {
            key: capabilities.get(key)
            for key in (
                "textDocumentSync",
                "definitionProvider",
                "referencesProvider",
                "callHierarchyProvider",
                "workspaceSymbolProvider",
                "documentSymbolProvider",
            )
        },
    }


def open_documents(session: LspSession) -> dict[str, str]:
    sources: dict[str, str] = {}
    for path in sorted(session.root.rglob("*.php")):
        source = path.read_text(encoding="utf-8")
        sources[path.as_uri()] = source
        session.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": path.as_uri(),
                    "languageId": "php",
                    "version": 1,
                    "text": source,
                }
            },
        )
    return sources


def definition_probe(session: LspSession, probe: JsonObject) -> JsonObject:
    params, _ = text_document_position(session.root, probe)
    measured = timed_request(session, "textDocument/definition", params)
    result = measured.pop("result", None)
    uris = location_uris(result)
    expected = probe.get("expected_definition_file")
    if expected is None:
        correct = not uris
    else:
        expected_symbol = str(probe["expected_target"]).rsplit("::", 1)[-1]
        expected_source = (session.root / str(expected)).read_text(encoding="utf-8")
        expected_position = utf16_position(
            expected_source,
            f"function {expected_symbol}",
            expected_symbol,
        )
        correct = any(
            uri_matches(session.root, uri, str(expected))
            and range_contains(source_range, expected_position)
            for uri, source_range in target_locations(result)
        )
    return {
        "id": probe["id"],
        "expected_target": probe.get("expected_target"),
        "expected_definition_file": expected,
        "correct": correct,
        "locations": uris,
        **measured,
    }


def evaluate_session(
    session: LspSession,
    truth: JsonObject,
    provider: str,
    initialization_options: JsonObject,
    startup_timeout: float,
) -> JsonObject:
    initialized = initialize(
        session,
        provider,
        initialization_options,
        startup_timeout,
    )
    sources = open_documents(session)

    calls = truth["calls"]
    definitions = [definition_probe(session, probe) for probe in calls]
    positive = [probe for probe in definitions if probe["expected_target"] is not None]
    true_positives = sum(1 for probe in positive if probe["correct"])
    false_negatives = len(positive) - true_positives
    negatives = [probe for probe in definitions if probe["expected_target"] is None]
    false_positives = sum(1 for probe in negatives if not probe["correct"])
    precision = true_positives / max(1, true_positives + false_positives)
    recall = true_positives / max(1, true_positives + false_negatives)

    references_probe = truth["lsp_probes"]["references"]
    references_params, _ = text_document_position(session.root, references_probe)
    references_params["context"] = {"includeDeclaration": True}
    references = timed_request(session, "textDocument/references", references_params)
    reference_result = references.pop("result", None)
    reference_locations = location_uris(reference_result)
    references["location_count"] = location_count(reference_result)
    references["unique_file_count"] = len(reference_locations)

    hierarchy_probe = truth["lsp_probes"]["call_hierarchy"]
    hierarchy_params, _ = text_document_position(session.root, hierarchy_probe)
    prepared = timed_request(session, "textDocument/prepareCallHierarchy", hierarchy_params)
    prepared_items = prepared.get("result") if isinstance(prepared.get("result"), list) else []
    incoming: JsonObject | None = None
    outgoing: JsonObject | None = None
    if prepared_items:
        incoming = timed_request(
            session, "callHierarchy/incomingCalls", {"item": prepared_items[0]}
        )
        outgoing = timed_request(
            session, "callHierarchy/outgoingCalls", {"item": prepared_items[0]}
        )

    sync_probe = truth["lsp_probes"]["synchronization"]
    sync_path = session.root / str(sync_probe["file"])
    sync_uri = sync_path.as_uri()
    original = sources[sync_uri]
    changed = original.replace(
        str(sync_probe["old_needle"]), str(sync_probe["new_needle"]), 1
    )
    session.notify(
        "textDocument/didChange",
        {
            "textDocument": {"uri": sync_uri, "version": 2},
            "contentChanges": [{"text": changed}],
        },
    )
    changed_position = utf16_position(
        changed,
        str(sync_probe["new_needle"]),
        "missingAfterEdit",
    )
    changed_definition = timed_request(
        session,
        "textDocument/definition",
        {"textDocument": {"uri": sync_uri}, "position": changed_position},
    )
    changed_locations = location_uris(changed_definition.pop("result", None))
    session.notify(
        "textDocument/didChange",
        {
            "textDocument": {"uri": sync_uri, "version": 3},
            "contentChanges": [{"text": original}],
        },
    )
    restored_probe = dict(truth["lsp_probes"]["definition"])
    restored_probe["id"] = "restored-after-change"
    restored_probe["expected_target"] = "App::Services::ReportService::generate"
    restored_probe["expected_definition_file"] = "app/Services/ReportService.php"
    restored = definition_probe(session, restored_probe)

    if initialized["capabilities"].get("workspaceSymbolProvider"):
        cancellation_method = "workspace/symbol"
        cancellation_params: JsonObject = {"query": "a"}
    else:
        cancellation_method = "textDocument/definition"
        cancellation_params, _ = text_document_position(session.root, calls[0])
    cancellation = timed_request(
        session,
        cancellation_method,
        cancellation_params,
        cancel_immediately=True,
    )
    cancellation.pop("result", None)
    cancellation["method"] = cancellation_method
    cancellation["cancelled"] = (
        isinstance(cancellation.get("error"), dict)
        and cancellation["error"].get("code") == -32800
    )

    return {
        "initialize": initialized,
        "opened_documents": len(sources),
        "rss_kib": session.rss_kib(),
        "definitions": {
            "precision": precision,
            "recall": recall,
            "true_positives": true_positives,
            "false_positives": false_positives,
            "false_negatives": false_negatives,
            "cases": definitions,
        },
        "references": references,
        "call_hierarchy": {
            "prepare": prepared,
            "incoming": incoming,
            "outgoing": outgoing,
        },
        "synchronization": {
            "changed_query": changed_definition,
            "changed_location_count": len(changed_locations),
            "stale_definition_returned": bool(changed_locations),
            "restored_query_correct": restored["correct"],
        },
        "cancellation": cancellation,
        "notifications": sorted(set(session.notifications)),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--ground-truth", type=Path)
    parser.add_argument("--initialization-options")
    parser.add_argument("--settings")
    parser.add_argument("--startup-timeout", type=float, default=60.0)
    parser.add_argument("--restart-timeout", type=float, default=30.0)
    parser.add_argument("--request-timeout", type=float, default=15.0)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command and args.command[0] == "--":
        args.command = args.command[1:]
    if not args.command:
        parser.error("provider command is required after --")
    return args


def main() -> int:
    args = parse_args()
    root = args.root.resolve()
    truth_path = args.ground_truth or root / "ground-truth.json"
    truth = json.loads(truth_path.read_text(encoding="utf-8"))
    initialization_options = load_json_object(args.initialization_options)
    settings = load_json_object(args.settings)
    started = time.monotonic()
    first = LspSession(args.command, root, args.request_timeout, settings)
    try:
        evaluation = evaluate_session(
            first,
            truth,
            args.provider,
            initialization_options,
            args.startup_timeout,
        )
        crash_detection_ms = first.kill()
        crash = {
            "detected": first.process.returncode is not None,
            "exit_code": first.process.returncode,
            "detection_ms": crash_detection_ms,
            "stderr_tail": first.stderr_tail()[-4096:],
        }
    except Exception as error:
        first.kill()
        output = {
            "schema_version": 1,
            "provider": args.provider,
            "command": args.command,
            "fatal_error": str(error),
            "stderr_tail": first.stderr_tail()[-8192:],
        }
        print(json.dumps(output, indent=2, sort_keys=True))
        return 1

    second = LspSession(args.command, root, args.request_timeout, settings)
    try:
        restart_initialize = initialize(
            second,
            args.provider,
            initialization_options,
            args.restart_timeout,
        )
        open_documents(second)
        restart_probe = dict(truth["calls"][0])
        restart_definition = definition_probe(second, restart_probe)
        graceful_shutdown = second.shutdown()
    except Exception as error:
        second.kill()
        restart_initialize = {"error": str(error)}
        restart_definition = None
        graceful_shutdown = {
            "clean": False,
            "exit_code": second.process.returncode,
            "stderr_tail": second.stderr_tail()[-4096:],
        }

    output = {
        "schema_version": 1,
        "provider": args.provider,
        "command": args.command,
        "corpus": str(root),
        "elapsed_ms": (time.monotonic() - started) * 1000.0,
        "evaluation": evaluation,
        "crash": crash,
        "restart": {
            "initialize": restart_initialize,
            "definition_correct": (
                restart_definition.get("correct")
                if isinstance(restart_definition, dict)
                else False
            ),
            "definition": restart_definition,
            "shutdown": graceful_shutdown,
        },
    }
    print(json.dumps(output, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
