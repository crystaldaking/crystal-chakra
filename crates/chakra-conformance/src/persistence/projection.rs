//! Benchmark-local per-file fact projection (issue #38).
//!
//! This is **not** a production cache format. It is a serializable projection
//! of the per-file syntax facts a real persistence layer would have to store,
//! built only from public `chakra-engine`/`chakra-language` APIs so the
//! measurements in `docs/evaluation/v0.2.0-persistence-acceptance.md`
//! describe facts Chakra actually produces today: declarations,
//! relationships (including imports), diagnostics, and call candidates.
//!
//! Every fact list is bounded; overflow is counted in the `omitted_*` fields
//! and never silently dropped.

use std::hash::Hasher;

use serde::{Deserialize, Serialize};

use chakra_engine::SymbolGraph;

use crate::Check;

/// Format version of the benchmark projection and cache model. Bumping it
/// invalidates every model cache (compatibility-key mismatch), exactly like a
/// production index format version would.
pub const MODEL_FORMAT_VERSION: u32 = 1;

/// Per-file fact bounds for the model cache.
pub const MAX_DECLARATIONS_PER_FILE: u64 = 4_096;
pub const MAX_RELATIONSHIPS_PER_FILE: u64 = 16_384;
pub const MAX_CALL_CANDIDATES_PER_FILE: u64 = 16_384;
/// Signatures are source lines; the projection caps each one.
pub const MAX_SIGNATURE_BYTES: usize = 512;

/// One declared symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationFact {
    pub qualified_name: String,
    pub kind: String,
    pub line: u32,
    pub column: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// One typed relation originating from a symbol declared in the file.
/// Edges with kind `IMPORTS` are the file's imports; keeping one list avoids
/// duplicating the storage model for a benchmark.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RelationshipFact {
    pub kind: String,
    pub from: String,
    pub to: String,
}

/// One syntactic call candidate attributed to a caller in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallCandidateFact {
    pub caller: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    pub target_kind: String,
    pub form: String,
    pub resolution: String,
    pub line: u32,
}

/// All benchmark facts for one indexed source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFacts {
    pub path: String,
    /// Model content hash of the retained source (see [`model_hash`]).
    pub content_hash: String,
    pub byte_len: u64,
    pub diagnostic_count: u64,
    pub declarations: Vec<DeclarationFact>,
    pub relationships: Vec<RelationshipFact>,
    pub call_candidates: Vec<CallCandidateFact>,
    #[serde(default)]
    pub omitted_declarations: u64,
    #[serde(default)]
    pub omitted_relationships: u64,
    #[serde(default)]
    pub omitted_call_candidates: u64,
}

impl FileFacts {
    /// Total omitted facts across all kinds.
    pub fn omitted_facts(&self) -> u64 {
        self.omitted_declarations
            .saturating_add(self.omitted_relationships)
            .saturating_add(self.omitted_call_candidates)
    }
}

/// Deterministic model hash (SipHash-1-3 with fixed keys, as used by the
/// corpus fingerprint). A production cache would use a cryptographic hash;
/// for measuring sizes and hit ratios a fixed 64-bit hash is sufficient and
/// honest about being a model.
pub fn model_hash(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

/// Serde wire name of a small enum (`CALLS`, `resolved`, ...), so facts use
/// the same vocabulary as the domain JSON schemas.
fn wire_name(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Builds the per-file projection for every file retained in `graph`.
/// Files are emitted in `file_summaries_iter` order (sorted by path); fact
/// lists are sorted so the projection is deterministic.
pub fn build_projection(graph: &SymbolGraph) -> Check<Vec<FileFacts>> {
    let mut files = Vec::new();
    for summary in graph.file_summaries_iter() {
        let path = summary.path;
        let source = graph.file_source(&path).unwrap_or_default();
        let mut declarations = Vec::new();
        let mut relationships = Vec::new();
        let mut call_candidates = Vec::new();
        let mut omitted_declarations = 0_u64;
        let mut omitted_relationships = 0_u64;
        let mut omitted_call_candidates = 0_u64;
        for symbol in graph.symbols_in_file(&path) {
            let declaration = DeclarationFact {
                qualified_name: symbol.key.qualified_name.clone(),
                kind: wire_name(&symbol.key.kind),
                line: symbol.location.start().line(),
                column: symbol.location.start().column(),
                signature: symbol.signature.as_ref().map(|signature| {
                    signature
                        .chars()
                        .take(MAX_SIGNATURE_BYTES)
                        .collect::<String>()
                }),
            };
            if u64::try_from(declarations.len()).unwrap_or(u64::MAX) < MAX_DECLARATIONS_PER_FILE {
                declarations.push(declaration);
            } else {
                omitted_declarations += 1;
            }
            for edge in graph.outgoing_edges(symbol.id) {
                let Some(target) = graph.symbol(edge.to) else {
                    omitted_relationships += 1;
                    continue;
                };
                let relationship = RelationshipFact {
                    kind: wire_name(&edge.kind),
                    from: symbol.key.qualified_name.clone(),
                    to: target.key.qualified_name.clone(),
                };
                if u64::try_from(relationships.len()).unwrap_or(u64::MAX)
                    < MAX_RELATIONSHIPS_PER_FILE
                {
                    relationships.push(relationship);
                } else {
                    omitted_relationships += 1;
                }
            }
            for call_site in graph.call_sites_from(symbol.id) {
                let candidate = CallCandidateFact {
                    caller: symbol.key.qualified_name.clone(),
                    name: call_site.name.clone(),
                    qualifier: call_site.qualifier.clone(),
                    target_kind: wire_name(&call_site.target_kind),
                    form: wire_name(&call_site.form),
                    resolution: wire_name(&call_site.resolution),
                    line: call_site.location.start().line(),
                };
                if u64::try_from(call_candidates.len()).unwrap_or(u64::MAX)
                    < MAX_CALL_CANDIDATES_PER_FILE
                {
                    call_candidates.push(candidate);
                } else {
                    omitted_call_candidates += 1;
                }
            }
        }
        declarations.sort_by(|left, right| {
            (left.line, &left.qualified_name).cmp(&(right.line, &right.qualified_name))
        });
        relationships.sort();
        call_candidates.sort_by(|left, right| {
            (&left.caller, left.line, &left.name).cmp(&(&right.caller, right.line, &right.name))
        });
        files.push(FileFacts {
            path: path.as_str().to_owned(),
            content_hash: model_hash(source.as_bytes()),
            byte_len: u64::try_from(source.len()).unwrap_or(u64::MAX),
            diagnostic_count: graph.file_diagnostic_count(&path).unwrap_or(0),
            declarations,
            relationships,
            call_candidates,
            omitted_declarations,
            omitted_relationships,
            omitted_call_candidates,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_hash_is_deterministic_and_fixed_width() {
        assert_eq!(model_hash(b"chakra"), model_hash(b"chakra"));
        assert_eq!(model_hash(b"").len(), 16);
        assert_ne!(model_hash(b"a"), model_hash(b"b"));
    }
}
