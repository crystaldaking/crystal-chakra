//! KMP and mixed JVM call queries avoid kotlin-lsp's incomplete hierarchy renderer.
//! Syntax identifies call expressions; LSP references and unique definitions
//! confirm their binding. Nothing is promoted from a name match alone.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::Provenance;
use chakra_domain::state::ProviderState;
use chakra_domain::symbol::Language;
use chakra_engine::{PreciseQueryRequest, PreciseQueryResult, PreciseRelation};
use chakra_language_index::calls::{CallSyntax, Callable};
use chakra_provider_worker::{QueryChannel, QueryOutcome, WorkerError, convert};
use lsp_types::{GotoDefinitionResponse, InitializeResult, Location, OneOf, Position, Range};
use serde_json::json;

const MAX_REFERENCES: usize = 2048;
const MAX_DOCUMENTS: usize = 32;
const MAX_CALLS: usize = 2048;

pub(crate) fn verify_capabilities(result: &InitializeResult) -> Result<(), WorkerError> {
    if matches!(
        result.capabilities.definition_provider,
        Some(OneOf::Left(true)) | Some(OneOf::Right(_))
    ) && matches!(
        result.capabilities.references_provider,
        Some(OneOf::Left(true)) | Some(OneOf::Right(_))
    ) {
        Ok(())
    } else {
        Err(WorkerError::Unsupported(
            "Kotlin definitions and references".to_owned(),
        ))
    }
}

pub(crate) fn query(
    channel: &mut dyn QueryChannel,
    request: &PreciseQueryRequest,
    deadline: Instant,
) -> Result<QueryOutcome, WorkerError> {
    let mut work = Query {
        channel,
        request,
        deadline,
        syntax: BTreeMap::new(),
    };
    let path = request.symbol.declaration.file();
    let document = request
        .workspace
        .document(path)
        .ok_or(WorkerError::InvalidPosition)?;
    let position = convert::find_symbol_position(
        &document.source,
        &request.symbol.name,
        &request.symbol.declaration,
    )?;
    let target_position = convert::lsp_to_chakra_position(&document.source, position)
        .ok_or(WorkerError::InvalidPosition)?;
    let target = work
        .syntax(path)?
        .callables
        .iter()
        .find(|callable| contains_position(&callable.identifier, target_position))
        .cloned()
        .ok_or(WorkerError::InvalidPosition)?;
    // A successfully imported project must resolve its own declaration.
    // Empty definitions during import are readiness, not a precise empty graph.
    let definitions = work.definitions(path, position)?;
    if definitions.is_empty() {
        return Ok(QueryOutcome {
            result: PreciseQueryResult::unavailable(
                request.workspace.revision,
                ProviderState::CatchingUp,
            ),
            may_improve_when_ready: true,
        });
    }
    if definitions.len() != 1 || !work.location_matches(&definitions[0], &target.identifier) {
        return Ok(QueryOutcome::ready(PreciseQueryResult::unavailable(
            request.workspace.revision,
            ProviderState::Degraded,
        )));
    }
    let mut result = PreciseQueryResult {
        revision: request.workspace.revision,
        state: ProviderState::Ready,
        fallback_cause: None,
        incoming: Vec::new(),
        outgoing: Vec::new(),
        incoming_truncated: false,
        outgoing_truncated: false,
    };
    if request.directions.incoming {
        let uri = convert::path_to_uri(&request.workspace.repository_root, path)?;
        let value = work.channel.request(
            "textDocument/references",
            &json!({
                "textDocument": {"uri": uri}, "position": position,
                "context": {"includeDeclaration": false}
            }),
            deadline,
        )?;
        let references =
            serde_json::from_value::<Option<Vec<Location>>>(value)?.unwrap_or_default();
        result.incoming_truncated = references.len() > MAX_REFERENCES;
        let mut incoming = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for reference in references.into_iter().take(MAX_REFERENCES) {
            let Some(reference_path) =
                convert::uri_to_path(&request.workspace.repository_root, &reference.uri)
            else {
                continue;
            };
            if request
                .workspace
                .document(&reference_path)
                .is_none_or(|document| {
                    !matches!(document.language, Language::Kotlin | Language::Java)
                })
            {
                continue;
            }
            if !work.syntax.contains_key(&reference_path) && work.syntax.len() == MAX_DOCUMENTS {
                result.incoming_truncated = true;
                continue;
            }
            work.channel.open_document(&reference_path, deadline)?;
            let reference_range = work
                .range(&reference_path, reference.range)
                .ok_or(WorkerError::InvalidPosition)?;
            if !seen.insert((
                reference_path.clone(),
                reference_range.start(),
                reference_range.end(),
            )) {
                continue;
            }
            let java_reference = request
                .workspace
                .document(&reference_path)
                .is_some_and(|document| document.language == Language::Java);
            let syntax = work.syntax(&reference_path)?;
            let Some(call) = syntax
                .calls
                .iter()
                .find(|call| {
                    if java_reference {
                        contains(&reference_range, &call.callee)
                            && contains(&call.selector, &reference_range)
                    } else {
                        contains(&call.callee, &reference_range)
                    }
                })
                .cloned()
            else {
                // Imports and callable values (e.g. ::target) are not calls.
                result.incoming_truncated |= syntax
                    .unsupported_calls
                    .iter()
                    .any(|range| contains(range, &reference_range));
                continue;
            };
            let Some(caller) = call
                .caller
                .and_then(|index| syntax.callables.get(index))
                .cloned()
            else {
                result.incoming_truncated = true;
                continue;
            };
            if !java_reference {
                let definitions = work.definitions(&reference_path, reference.range.start)?;
                if definitions.len() != 1
                    || !work.location_matches(&definitions[0], &target.identifier)
                {
                    result.incoming_truncated = true;
                    continue;
                }
            }
            // The pinned server resolves Java references to this verified Kotlin
            // target (including overloads), but has no Java definition handler.
            // Its semantic reference establishes the edge; syntax only identifies
            // the actual invocation and its owner, excluding method values.
            add_relation(&mut incoming, caller, call.expression);
        }
        result.incoming = bounded(incoming, request.limit, &mut result.incoming_truncated);
    }
    if request.directions.outgoing {
        let syntax = work.syntax(path)?;
        let target_index = syntax
            .callables
            .iter()
            .position(|callable| callable.identifier == target.identifier)
            .ok_or(WorkerError::InvalidPosition)?;
        let calls: Vec<_> = syntax
            .calls
            .iter()
            .filter(|call| call.caller == Some(target_index))
            .cloned()
            .collect();
        result.outgoing_truncated = calls.len() > MAX_CALLS
            || syntax
                .unsupported_calls
                .iter()
                .any(|range| contains(&target.declaration, range));
        let mut outgoing = BTreeMap::new();
        for call in calls.into_iter().take(MAX_CALLS) {
            let position = convert::chakra_to_lsp_position(&document.source, call.callee.start())
                .ok_or(WorkerError::InvalidPosition)?;
            let definitions = work.definitions(path, position)?;
            if definitions.len() != 1 {
                result.outgoing_truncated = true;
                continue;
            }
            let definition = &definitions[0];
            let Some(target_path) =
                convert::uri_to_path(&request.workspace.repository_root, &definition.uri)
            else {
                continue;
            };
            if request
                .workspace
                .document(&target_path)
                .is_none_or(|document| {
                    !matches!(document.language, Language::Kotlin | Language::Java)
                })
            {
                continue;
            }
            if !work.syntax.contains_key(&target_path) && work.syntax.len() == MAX_DOCUMENTS {
                result.outgoing_truncated = true;
                continue;
            }
            let target_range = work
                .range(&target_path, definition.range)
                .ok_or(WorkerError::InvalidPosition)?;
            let syntax = work.syntax(&target_path)?;
            let mut candidates = syntax.callables.iter().filter(|callable| {
                contains(&callable.declaration, &target_range)
                    && contains(&target_range, &callable.identifier)
            });
            let callee = candidates.next().cloned();
            if candidates.next().is_some() || callee.is_none() {
                result.outgoing_truncated = true;
                continue;
            }
            if let Some(callee) = callee {
                add_relation(&mut outgoing, callee, call.expression);
            }
        }
        result.outgoing = bounded(outgoing, request.limit, &mut result.outgoing_truncated);
    }
    Ok(QueryOutcome::ready(result))
}

struct Query<'a> {
    channel: &'a mut dyn QueryChannel,
    request: &'a PreciseQueryRequest,
    deadline: Instant,
    syntax: BTreeMap<RepoRelativePath, CallSyntax>,
}

impl Query<'_> {
    fn syntax(&mut self, path: &RepoRelativePath) -> Result<&CallSyntax, WorkerError> {
        if !self.syntax.contains_key(path) {
            let document = self
                .request
                .workspace
                .document(path)
                .ok_or(WorkerError::InvalidPosition)?;
            let mut abort = None;
            let mut cancelled = || {
                if Instant::now() >= self.deadline {
                    abort = Some(WorkerError::Timeout);
                }
                if abort.is_none() {
                    abort = self.channel.wait_until(Instant::now()).err();
                }
                abort.is_some()
            };
            let syntax = match document.language {
                Language::Kotlin => chakra_language_kotlin::calls::analyze_calls(
                    path,
                    &document.source,
                    &mut cancelled,
                )
                .map_err(|_| ()),
                Language::Java => chakra_language_java::calls::analyze_calls(
                    path,
                    &document.source,
                    &mut cancelled,
                )
                .map_err(|_| ()),
                _ => return Err(WorkerError::Unsupported("call syntax language".to_owned())),
            };
            if let Some(error) = abort {
                return Err(error);
            }
            self.syntax.insert(
                path.clone(),
                syntax.map_err(|_| {
                    WorkerError::Unsupported("bounded, well-formed JVM call syntax".to_owned())
                })?,
            );
        }
        self.syntax.get(path).ok_or(WorkerError::InvalidPosition)
    }

    fn definitions(
        &mut self,
        path: &RepoRelativePath,
        position: Position,
    ) -> Result<Vec<Location>, WorkerError> {
        let uri = convert::path_to_uri(&self.request.workspace.repository_root, path)?;
        let value = self.channel.request(
            "textDocument/definition",
            &json!({"textDocument":{"uri":uri},"position":position}),
            self.deadline,
        )?;
        let response = serde_json::from_value::<Option<GotoDefinitionResponse>>(value)?;
        let locations = match response {
            Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
            Some(GotoDefinitionResponse::Array(locations)) => locations,
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => Vec::new(),
        };
        let mut unique = Vec::new();
        for location in locations {
            if !unique.contains(&location) {
                unique.push(location);
            }
            if unique.len() > 1 {
                break;
            }
        }
        Ok(unique)
    }

    fn range(&self, path: &RepoRelativePath, range: Range) -> Option<SourceRange> {
        let document = self.request.workspace.document(path)?;
        SourceRange::new(
            path.clone(),
            convert::lsp_to_chakra_position(&document.source, range.start)?,
            convert::lsp_to_chakra_position(&document.source, range.end)?,
        )
        .ok()
    }

    fn location_matches(&self, location: &Location, identifier: &SourceRange) -> bool {
        let Some(path) =
            convert::uri_to_path(&self.request.workspace.repository_root, &location.uri)
        else {
            return false;
        };
        self.range(&path, location.range)
            .is_some_and(|range| range == *identifier)
    }
}

fn contains(outer: &SourceRange, inner: &SourceRange) -> bool {
    outer.file() == inner.file() && outer.start() <= inner.start() && inner.end() <= outer.end()
}

fn contains_position(range: &SourceRange, position: TextPosition) -> bool {
    range.start() <= position && position < range.end()
}

type Relations = BTreeMap<(RepoRelativePath, TextPosition, TextPosition), PreciseRelation>;

fn add_relation(relations: &mut Relations, callable: Callable, call_site: SourceRange) {
    let key = (
        callable.identifier.file().clone(),
        callable.identifier.start(),
        callable.identifier.end(),
    );
    let relation = relations.entry(key).or_insert_with(|| PreciseRelation {
        name: callable.name,
        declaration: callable.identifier,
        occurrence_count: 0,
        call_sites: Vec::new(),
        provenance: Provenance::KotlinLsp,
    });
    relation.occurrence_count += 1;
    if relation.call_sites.len() < 3 && !relation.call_sites.contains(&call_site) {
        relation.call_sites.push(call_site);
    }
}

fn bounded(relations: Relations, limit: usize, truncated: &mut bool) -> Vec<PreciseRelation> {
    *truncated |= relations.len() > limit;
    relations.into_values().take(limit).collect()
}

#[cfg(test)]
mod tests;
