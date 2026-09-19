use super::*;
use chakra_domain::revision::Revision;
use chakra_engine::{
    CallHierarchyDirections, ProviderDocument, ProviderRequestPriority, ProviderSymbol,
    ProviderWorkspace,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

const SOURCE: &str = "fun target() {}\nfun other() {}\nfun caller() { val emoji = \"😀\"; target(); target(); val value = ::target }\n";

struct Channel {
    answers: VecDeque<(&'static str, Value)>,
    definitions: Vec<Position>,
    cancelled: bool,
}

impl QueryChannel for Channel {
    fn request(&mut self, method: &str, params: &Value, _: Instant) -> Result<Value, WorkerError> {
        let (expected, answer) = self
            .answers
            .pop_front()
            .ok_or_else(|| WorkerError::Unsupported("unexpected test request".into()))?;
        assert_eq!(method, expected);
        if method == "textDocument/definition" {
            self.definitions
                .push(serde_json::from_value(params["position"].clone())?);
        }
        Ok(answer)
    }

    fn wait_until(&mut self, _: Instant) -> Result<(), WorkerError> {
        if self.cancelled {
            Err(WorkerError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn open_document(&mut self, _: &RepoRelativePath, _: Instant) -> Result<(), WorkerError> {
        self.wait_until(Instant::now())
    }
}

fn fixture(
    name: &str,
    line: u32,
    incoming: bool,
) -> Result<PreciseQueryRequest, Box<dyn std::error::Error>> {
    let path = RepoRelativePath::new("src/commonMain/kotlin/Main.kt")?;
    let declaration = SourceRange::new(
        path.clone(),
        TextPosition::new(line, 5)?,
        TextPosition::new(line, 5 + name.len() as u32)?,
    )?;
    Ok(PreciseQueryRequest {
        workspace: ProviderWorkspace::from_documents(
            std::env::temp_dir().join("chakra-semantic-fixture"),
            Revision(17),
            vec![ProviderDocument {
                path,
                source: Arc::from(SOURCE),
                language: Language::Kotlin,
            }],
        ),
        symbol: ProviderSymbol {
            name: name.into(),
            declaration,
            language: Language::Kotlin,
        },
        directions: CallHierarchyDirections {
            incoming,
            outgoing: !incoming,
        },
        limit: 20,
        priority: ProviderRequestPriority::Normal,
    })
}

fn location(
    request: &PreciseQueryRequest,
    line: u32,
    start: u32,
    end: u32,
) -> Result<Value, WorkerError> {
    Ok(json!({
        "uri": convert::path_to_uri(&request.workspace.repository_root, request.symbol.declaration.file())?,
        "range": {"start":{"line":line,"character":start},"end":{"line":line,"character":end}}
    }))
}

fn channel(answers: Vec<(&'static str, Value)>) -> Channel {
    Channel {
        answers: answers.into(),
        definitions: Vec::new(),
        cancelled: false,
    }
}

fn add_java(
    request: &mut PreciseQueryRequest,
    source: &str,
) -> Result<RepoRelativePath, Box<dyn std::error::Error>> {
    let path = RepoRelativePath::new("src/main/java/Peer.java")?;
    let kotlin = request
        .workspace
        .document(request.symbol.declaration.file())
        .ok_or("missing Kotlin source")?;
    request.workspace = ProviderWorkspace::from_documents(
        request.workspace.repository_root.clone(),
        request.workspace.revision,
        vec![
            kotlin,
            ProviderDocument {
                path: path.clone(),
                source: Arc::from(source),
                language: Language::Java,
            },
        ],
    );
    Ok(path)
}

#[test]
fn java_semantic_references_identify_calls_without_inventing_overload_or_method_value_edges()
-> Result<(), Box<dyn std::error::Error>> {
    let mut request = fixture("target", 1, true)?;
    let source = "class Peer {\n void javaCaller() { String s = \"😀\"; MainKt.target(); }\n void numericCaller() { MainKt.target(1); }\n Runnable value = MainKt::target;\n}";
    let path = add_java(&mut request, source)?;
    let uri = convert::path_to_uri(&request.workspace.repository_root, &path)?;
    let java_location = |line: usize, text: &str| -> Result<Value, Box<dyn std::error::Error>> {
        let source_line = source.lines().nth(line).ok_or("missing Java line")?;
        let byte = source_line.find(text).ok_or("missing Java reference")?;
        let start = source_line[..byte].encode_utf16().count();
        Ok(
            json!({"uri":uri,"range":{"start":{"line":line,"character":start},"end":{"line":line,"character":start+text.encode_utf16().count()}}}),
        )
    };
    let call = java_location(1, "MainKt.target")?;
    let value = java_location(3, "MainKt::target")?;
    let mut channel = channel(vec![
        ("textDocument/definition", location(&request, 0, 4, 10)?),
        ("textDocument/references", json!([call, call, value])),
    ]);
    let result = query(
        &mut channel,
        &request,
        Instant::now() + Duration::from_secs(5),
    )?
    .result;
    assert_eq!(result.incoming.len(), 1, "{:?}", result.incoming);
    assert_eq!(result.incoming[0].name, "javaCaller");
    assert_eq!(result.incoming[0].declaration.file(), &path);
    assert_eq!(result.incoming[0].occurrence_count, 1);
    assert_eq!(result.incoming[0].provenance, Provenance::KotlinLsp);
    assert!(!result.incoming_truncated);
    assert!(channel.answers.is_empty());
    Ok(())
}

#[test]
fn kotlin_outgoing_definitions_can_resolve_java_declarations()
-> Result<(), Box<dyn std::error::Error>> {
    let mut request = fixture("caller", 3, false)?;
    let source = "class Peer { public static void target() {} }";
    let path = add_java(&mut request, source)?;
    let start = source.find("target").ok_or("missing target")?;
    let java = json!({"uri":convert::path_to_uri(&request.workspace.repository_root,&path)?,
        "range":{"start":{"line":0,"character":start},"end":{"line":0,"character":start+6}}});
    let mut channel = channel(vec![
        ("textDocument/definition", location(&request, 2, 4, 10)?),
        ("textDocument/definition", java.clone()),
        ("textDocument/definition", java),
    ]);
    let result = query(
        &mut channel,
        &request,
        Instant::now() + Duration::from_secs(5),
    )?
    .result;
    assert_eq!(result.outgoing.len(), 1);
    assert_eq!(result.outgoing[0].declaration.file(), &path);
    assert_eq!(result.outgoing[0].name, "target");
    assert_eq!(result.outgoing[0].occurrence_count, 2);
    assert!(!result.outgoing_truncated);
    Ok(())
}

fn call_locations(request: &PreciseQueryRequest) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let line = SOURCE.lines().nth(2).ok_or("missing fixture line")?;
    line.match_indices("target")
        .map(|(offset, _)| {
            let column = line[..offset].encode_utf16().count() as u32;
            Ok(location(request, 2, column, column + 6)?)
        })
        .collect()
}

#[test]
fn incoming_deduplicates_references_and_excludes_callable_values()
-> Result<(), Box<dyn std::error::Error>> {
    let request = fixture("target", 1, true)?;
    let target = location(&request, 0, 4, 10)?;
    let calls = call_locations(&request)?;
    let mut channel = channel(vec![
        ("textDocument/definition", target.clone()),
        (
            "textDocument/references",
            json!([calls[0], calls[0], calls[1], calls[2]]),
        ),
        ("textDocument/definition", target.clone()),
        ("textDocument/definition", target),
    ]);
    let outcome = query(
        &mut channel,
        &request,
        Instant::now() + Duration::from_secs(5),
    )?;
    assert_eq!(outcome.result.state, ProviderState::Ready);
    assert_eq!(outcome.result.revision, Revision(17));
    assert!(!outcome.result.incoming_truncated);
    assert_eq!(outcome.result.incoming.len(), 1);
    let caller = &outcome.result.incoming[0];
    assert_eq!(caller.name, "caller");
    assert_eq!(caller.provenance, Provenance::KotlinLsp);
    assert_eq!(caller.occurrence_count, 2);
    assert_eq!(caller.call_sites.len(), 2);
    assert_eq!(caller.call_sites[0].start().column(), 33);
    assert_eq!(channel.definitions[1].character, 33); // UTF-16 includes the surrogate pair.
    assert!(channel.answers.is_empty());
    Ok(())
}

#[test]
fn deferred_lambda_calls_are_incomplete_without_false_named_edges()
-> Result<(), Box<dyn std::error::Error>> {
    let source = "fun target() {}\nfun factory() = { target() }\n";
    for incoming in [true, false] {
        let mut request = if incoming {
            fixture("target", 1, true)?
        } else {
            fixture("factory", 2, false)?
        };
        request.workspace = ProviderWorkspace::from_documents(
            request.workspace.repository_root.clone(),
            Revision(17),
            vec![ProviderDocument {
                path: request.symbol.declaration.file().clone(),
                source: Arc::from(source),
                language: Language::Kotlin,
            }],
        );
        let definition = if incoming {
            location(&request, 0, 4, 10)?
        } else {
            location(&request, 1, 4, 11)?
        };
        let mut answers = vec![("textDocument/definition", definition)];
        if incoming {
            answers.push((
                "textDocument/references",
                json!([location(&request, 1, 18, 24)?]),
            ));
        }
        let mut channel = channel(answers);
        let outcome = query(
            &mut channel,
            &request,
            Instant::now() + Duration::from_secs(5),
        )?;
        assert_eq!(outcome.result.state, ProviderState::Ready);
        assert!(outcome.result.incoming.is_empty());
        assert!(outcome.result.outgoing.is_empty());
        assert_eq!(outcome.result.incoming_truncated, incoming);
        assert_eq!(outcome.result.outgoing_truncated, !incoming);
        assert!(channel.answers.is_empty());
    }
    Ok(())
}

#[test]
fn incoming_rejects_wrong_and_ambiguous_bindings() -> Result<(), Box<dyn std::error::Error>> {
    let request = fixture("target", 1, true)?;
    let target = location(&request, 0, 4, 10)?;
    let other = location(&request, 1, 4, 9)?;
    let calls = call_locations(&request)?;
    let mut channel = channel(vec![
        ("textDocument/definition", target.clone()),
        ("textDocument/references", json!([calls[0], calls[1]])),
        ("textDocument/definition", other.clone()),
        ("textDocument/definition", json!([target, other])),
    ]);
    let outcome = query(
        &mut channel,
        &request,
        Instant::now() + Duration::from_secs(5),
    )?;
    assert!(outcome.result.incoming.is_empty());
    assert!(outcome.result.incoming_truncated);
    assert!(channel.answers.is_empty());
    Ok(())
}

#[test]
fn outgoing_uses_unique_definitions_from_immutable_source() -> Result<(), Box<dyn std::error::Error>>
{
    let request = fixture("caller", 3, false)?;
    let target = location(&request, 0, 4, 10)?;
    let mut channel = channel(vec![
        ("textDocument/definition", location(&request, 2, 4, 10)?),
        ("textDocument/definition", json!([target, target])),
        ("textDocument/definition", Value::Null),
    ]);
    let outcome = query(
        &mut channel,
        &request,
        Instant::now() + Duration::from_secs(5),
    )?;
    assert_eq!(outcome.result.outgoing.len(), 1);
    assert_eq!(outcome.result.outgoing[0].name, "target");
    assert_eq!(outcome.result.outgoing[0].occurrence_count, 1);
    assert_eq!(outcome.result.outgoing[0].provenance, Provenance::KotlinLsp);
    assert!(outcome.result.outgoing_truncated);
    assert!(channel.answers.is_empty());
    Ok(())
}

#[test]
fn readiness_cancellation_and_overbroad_definition_cannot_produce_ready()
-> Result<(), Box<dyn std::error::Error>> {
    let request = fixture("target", 1, true)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut loading = channel(vec![("textDocument/definition", Value::Null)]);
    let outcome = query(&mut loading, &request, deadline)?;
    assert_eq!(outcome.result.state, ProviderState::CatchingUp);
    assert!(outcome.may_improve_when_ready);
    let mut broad = channel(vec![(
        "textDocument/definition",
        location(&request, 0, 0, 15)?,
    )]);
    assert_eq!(
        query(&mut broad, &request, deadline)?.result.state,
        ProviderState::Degraded
    );
    let mut cancelled = channel(vec![]);
    cancelled.cancelled = true;
    assert!(matches!(
        query(&mut cancelled, &request, deadline),
        Err(WorkerError::Cancelled)
    ));
    assert!(matches!(
        query(&mut channel(vec![]), &request, Instant::now()),
        Err(WorkerError::Timeout)
    ));
    Ok(())
}

#[test]
fn implicit_operator_reference_is_reported_as_incomplete() -> Result<(), Box<dyn std::error::Error>>
{
    let mut request = fixture("plus", 1, true)?;
    let source =
        "operator fun Int.plus(other: Int): Int = this\nfun caller() { val result = 1 + 2 }\n";
    let path = request.symbol.declaration.file().clone();
    request.symbol.declaration = SourceRange::new(
        path.clone(),
        TextPosition::new(1, 1)?,
        TextPosition::new(1, 44)?,
    )?;
    request.workspace = ProviderWorkspace::from_documents(
        request.workspace.repository_root.clone(),
        Revision(17),
        vec![ProviderDocument {
            path,
            source: Arc::from(source),
            language: Language::Kotlin,
        }],
    );
    let declaration = source
        .lines()
        .next()
        .ok_or("missing declaration")?
        .find("plus")
        .ok_or("missing name")? as u32;
    let operator = source
        .lines()
        .nth(1)
        .ok_or("missing caller")?
        .find('+')
        .ok_or("missing operator")? as u32;
    let mut channel = channel(vec![
        (
            "textDocument/definition",
            location(&request, 0, declaration, declaration + 4)?,
        ),
        (
            "textDocument/references",
            json!([location(&request, 1, operator, operator + 1)?]),
        ),
    ]);
    let result = query(
        &mut channel,
        &request,
        Instant::now() + Duration::from_secs(5),
    )?
    .result;
    assert_eq!(result.state, ProviderState::Ready);
    assert!(result.incoming.is_empty());
    assert!(result.incoming_truncated);
    assert!(channel.answers.is_empty());
    Ok(())
}
