#!/usr/bin/env python3
"""Probe a local language server's LSP capabilities and report them as JSON.

This is an opt-in, reproducible provider-selection probe (issue #23 /
ADR-0027), not a Chakra runtime dependency. It uses only the Python standard
library and never modifies the probed workspace. The default Chakra test
suite does not require any language server; this tool exists so provider
decisions can be re-verified on demand.

Usage:
    tools/probe_language_server.py --root /path/to/workspace -- <server command...>
    tools/probe_language_server.py --root . --require definition references callHierarchy -- gopls

Exit status is 0 when every --require capability is advertised by the server,
1 otherwise (including transport or initialization failure).
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

JsonObject = dict[str, Any]
MAX_MESSAGE_BYTES = 8 * 1024 * 1024
INITIALIZE_TIMEOUT_SECONDS = 60.0


class TransportClosed(RuntimeError):
    """Raised when the provider closes its JSON-RPC transport."""


class LspSession:
    def __init__(self, command: list[str], root: Path) -> None:
        self.process = subprocess.Popen(
            command,
            cwd=root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            start_new_session=os.name == "posix",
        )
        self._messages: queue.Queue[JsonObject] = queue.Queue(maxsize=256)
        self._next_id = 1
        self._write_lock = threading.Lock()
        threading.Thread(target=self._read_stdout, daemon=True).start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        while True:
            headers: dict[str, str] = {}
            while True:
                line = self.process.stdout.readline()
                if not line:
                    self._messages.put({"__closed__": True})
                    return
                line = line.strip()
                if not line:
                    break
                name, _, value = line.partition(b":")
                headers[name.decode("ascii", "replace").strip().lower()] = value.decode(
                    "ascii", "replace"
                ).strip()
            length = int(headers.get("content-length", "0"))
            if length <= 0 or length > MAX_MESSAGE_BYTES:
                continue
            payload = self.process.stdout.read(length)
            try:
                self._messages.put(json.loads(payload))
            except json.JSONDecodeError:
                continue

    def send(self, message: JsonObject) -> None:
        payload = json.dumps(message).encode("utf-8")
        frame = f"Content-Length: {len(payload)}\r\n\r\n".encode("ascii") + payload
        with self._write_lock:
            assert self.process.stdin is not None
            self.process.stdin.write(frame)
            self.process.stdin.flush()

    def request(self, method: str, params: JsonObject, timeout: float) -> JsonObject:
        request_id = self._next_id
        self._next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{method} timed out after {timeout}s")
            try:
                message = self._messages.get(timeout=remaining)
            except queue.Empty:
                raise TimeoutError(f"{method} timed out after {timeout}s") from None
            if message.get("__closed__"):
                raise TransportClosed(f"transport closed during {method}")
            if message.get("id") != request_id:
                # Server request or notification: answer requests with null to
                # keep the handshake moving, ignore everything else.
                if "method" in message and "id" in message:
                    self.send({"jsonrpc": "2.0", "id": message["id"], "result": None})
                continue
            if "error" in message:
                raise RuntimeError(f"{method} failed: {message['error']}")
            return message.get("result") or {}

    def notify(self, method: str, params: JsonObject) -> None:
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def shutdown(self) -> None:
        try:
            self.request("shutdown", {}, timeout=10.0)
            self.notify("exit", {})
        except (TransportClosed, TimeoutError, RuntimeError):
            pass
        try:
            self.process.wait(timeout=10.0)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=10.0)


def capability_report(capabilities: JsonObject) -> JsonObject:
    def advertised(*names: str) -> bool:
        for name in names:
            value: Any = capabilities.get(name)
            if value is True or isinstance(value, dict):
                return True
        return False

    return {
        "definition": advertised("definitionProvider"),
        "references": advertised("referencesProvider"),
        "callHierarchy": advertised("callHierarchyProvider"),
        "workspaceSymbol": advertised("workspaceSymbolProvider"),
        "documentSymbol": advertised("documentSymbolProvider"),
        "rename": advertised("renameProvider"),
        "diagnostics": advertised("diagnosticProvider"),
        "textDocumentSync": capabilities.get("textDocumentSync") is not None,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True, help="workspace root for initialize")
    parser.add_argument(
        "--require",
        nargs="*",
        default=[],
        choices=["definition", "references", "callHierarchy", "workspaceSymbol"],
        help="capabilities that must be advertised for exit status 0",
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="language server command after --",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        print("error: pass the language server command after --", file=sys.stderr)
        return 2

    root = args.root.resolve()
    session = LspSession(command, root)
    try:
        result = session.request(
            "initialize",
            {
                "processId": None,
                "rootUri": root.as_uri(),
                "capabilities": {},
                "workspaceFolders": None,
            },
            timeout=INITIALIZE_TIMEOUT_SECONDS,
        )
        session.notify("initialized", {})
        report = capability_report(result.get("capabilities") or {})
        report["serverInfo"] = result.get("serverInfo") or {}
    except (TransportClosed, TimeoutError, RuntimeError) as exc:
        print(json.dumps({"error": str(exc)}))
        session.process.kill()
        session.process.wait()
        return 1
    finally:
        if session.process.poll() is None:
            session.shutdown()

    missing = [name for name in args.require if not report.get(name)]
    report["required"] = args.require
    report["missing"] = missing
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if not missing else 1


if __name__ == "__main__":
    sys.exit(main())
