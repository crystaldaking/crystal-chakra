//! Terraform JSON syntax extraction (issue #86). `.tf.json`, `.tfvars.json`,
//! and `.tftest.json` files parse through the Tree-sitter JSON grammar and
//! mirror the native-HCL entity model: byte-accurate ranges, the same
//! resource/data/module/variable/output/provider/test identities, import
//! edges for `source`/`required_providers`, actionable diagnostics, and
//! configuration-reference candidates from `${...}` interpolation strings
//! where the JSON expression encoding permits them.

use std::sync::Arc;

use chakra_domain::diagnostic::{SyntaxDiagnostic, SyntaxDiagnosticCause, SyntaxDiagnosticKind};
use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::{Precision, Provenance};
use chakra_domain::symbol::{CallForm, CallTargetKind, EdgeKind, Language, SymbolKey, SymbolKind};
use tree_sitter::{Node, Parser};

use crate::parser::{
    ParseError, is_test_file, is_tfvars, module_path, scan_traversal, traversal_target,
};

use crate::parser::{CallDraft, NamedRelationDraft, ParsedFile, SymbolDraft};

const MAX_INTERPOLATIONS_PER_STRING: usize = 16;

/// Terraform JSON variants (issue #86); plain `.json` is not Terraform.
pub(crate) fn is_terraform_json_path(path: &RepoRelativePath) -> bool {
    let path = path.as_str();
    path.ends_with(".tf.json") || path.ends_with(".tfvars.json") || path.ends_with(".tftest.json")
}

/// Parses one Terraform JSON source into the shared per-file fact model.
pub(crate) fn parse_terraform_json(
    path: &RepoRelativePath,
    source: Arc<str>,
) -> Result<ParsedFile, ParseError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .map_err(|error| ParseError::Language(error.to_string()))?;
    let tree = parser
        .parse(source.as_ref(), None)
        .ok_or_else(|| ParseError::NoTree(path.clone()))?;
    let root = tree.root_node();
    let module_path = module_path(path);
    let file_source = source.clone();
    let mut extraction = JsonExtraction {
        path: path.clone(),
        source: source.as_ref(),
        symbols: Vec::new(),
        calls: Vec::new(),
        named_relations: Vec::new(),
    };
    let module = extraction.symbols.len();
    extraction.symbols.push(SymbolDraft {
        key: SymbolKey {
            language: Language::Hcl,
            qualified_name: module_path.join("::"),
            container: module_path
                .split_last()
                .and_then(|(_, parent)| (!parent.is_empty()).then(|| parent.join("::"))),
            kind: SymbolKind::Module,
            path: path.clone(),
        },
        location: range_of(path, source.as_ref(), root)?,
        signature: Some("Terraform JSON configuration file".to_owned()),
        parent: None,
    });
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "object" {
            if is_tfvars(path) {
                extraction.tfvars_object(child, module)?;
            } else {
                extraction.top_level_object(child, module)?;
            }
        } else if child.kind() == "ERROR" {
            // Diagnostics are collected separately below.
        }
    }
    let (diagnostics, diagnostic_count) = diagnostics_of(path, source.as_ref(), root)?;
    Ok(ParsedFile {
        source: file_source,
        module_path,
        symbols: extraction.symbols,
        calls: extraction.calls,
        named_relations: extraction.named_relations,
        has_errors: root.has_error(),
        diagnostics,
        diagnostic_count,
    })
}

struct JsonExtraction<'a> {
    path: RepoRelativePath,
    source: &'a str,
    symbols: Vec<SymbolDraft>,
    calls: Vec<CallDraft>,
    named_relations: Vec<NamedRelationDraft>,
}

impl JsonExtraction<'_> {
    fn text(&self, node: Node<'_>) -> &str {
        self.source
            .get(node.start_byte()..node.end_byte())
            .unwrap_or_default()
    }

    fn key_text(&self, pair: Node<'_>) -> Option<String> {
        let key = pair.child_by_field_name("key")?;
        json_string_text(self.text(key))
    }

    fn add_symbol(
        &mut self,
        parent: Option<usize>,
        container: Option<String>,
        segments: Vec<String>,
        kind: SymbolKind,
        node: Node<'_>,
        signature: Option<String>,
    ) -> Result<usize, ParseError> {
        let qualified_name = segments.join("::");
        let index = self.symbols.len();
        self.symbols.push(SymbolDraft {
            key: SymbolKey {
                language: Language::Hcl,
                qualified_name,
                container,
                kind,
                path: self.path.clone(),
            },
            location: range_of(&self.path, self.source, node)?,
            signature,
            parent,
        });
        Ok(index)
    }

    /// `.tfvars.json`: top-level pairs are variable assignments, mirroring
    /// native `attribute` with the `tfvars` container.
    fn tfvars_object(&mut self, object: Node<'_>, module: usize) -> Result<(), ParseError> {
        let mut cursor = object.walk();
        for pair in object.named_children(&mut cursor) {
            if pair.kind() != "pair" {
                continue;
            }
            let Some(name) = self.key_text(pair) else {
                continue;
            };
            let symbol = self.add_symbol(
                Some(module),
                Some("tfvars".to_owned()),
                vec!["tfvars".to_owned(), name.clone()],
                SymbolKind::Property,
                pair,
                Some("tfvars assignment (JSON)".to_owned()),
            )?;
            if let Some(value) = pair.child_by_field_name("value") {
                self.scan_interpolations(value, symbol)?;
            }
        }
        Ok(())
    }

    fn top_level_object(&mut self, object: Node<'_>, module: usize) -> Result<(), ParseError> {
        let mut cursor = object.walk();
        for pair in object.named_children(&mut cursor) {
            if pair.kind() != "pair" {
                continue;
            }
            let Some(key) = self.key_text(pair) else {
                continue;
            };
            let Some(value) = pair.child_by_field_name("value") else {
                continue;
            };
            match key.as_str() {
                "resource" | "data" => self.labeled_group(value, module, &key, 2)?,
                "module" => self.labeled_group(value, module, "module", 1)?,
                "variable" | "output" | "provider" => self.labeled_group(value, module, &key, 1)?,
                "terraform" | "locals" => {
                    let symbol = self.add_symbol(
                        Some(module),
                        Some(module_path(&self.path).join("::")),
                        vec![key.clone()],
                        SymbolKind::Module,
                        pair,
                        Some(format!("{key} block (JSON)")),
                    )?;
                    self.block_body(value, symbol, Some(key.as_str()))?;
                }
                "check" => self.labeled_group(value, module, "check", 1)?,
                "run" if is_test_file(&self.path) => self.labeled_group(value, module, "run", 1)?,
                "variables" if is_test_file(&self.path) => {
                    self.labeled_group(value, module, "var", 1)?
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// One `resource`/`data`/`module`/... group: an object keyed by (type)
    /// and then name, mirroring native block labels.
    fn labeled_group(
        &mut self,
        value: Node<'_>,
        module: usize,
        block_type: &str,
        labels: usize,
    ) -> Result<(), ParseError> {
        let mut entities = Vec::new();
        if labels == 2 {
            // resource/data: value is an object keyed by type, each type an
            // object keyed by name.
            let mut cursor = value.walk();
            for type_pair in value.named_children(&mut cursor) {
                if type_pair.kind() != "pair" {
                    continue;
                }
                let Some(type_name) = self.key_text(type_pair) else {
                    continue;
                };
                let Some(type_value) = type_pair.child_by_field_name("value") else {
                    continue;
                };
                let mut inner = type_value.walk();
                for name_pair in type_value.named_children(&mut inner) {
                    if name_pair.kind() != "pair" {
                        continue;
                    }
                    let Some(name) = self.key_text(name_pair) else {
                        continue;
                    };
                    let Some(body) = name_pair.child_by_field_name("value") else {
                        continue;
                    };
                    entities.push((
                        vec![block_type.to_owned(), type_name.clone(), name],
                        name_pair,
                        body,
                    ));
                }
            }
        } else {
            let mut cursor = value.walk();
            for name_pair in value.named_children(&mut cursor) {
                if name_pair.kind() != "pair" {
                    continue;
                }
                let Some(name) = self.key_text(name_pair) else {
                    continue;
                };
                let Some(body) = name_pair.child_by_field_name("value") else {
                    continue;
                };
                entities.push((vec![block_type.to_owned(), name], name_pair, body));
            }
        }
        self.emit_entities(entities, module, block_type)
    }

    fn emit_entities(
        &mut self,
        entities: Vec<(Vec<String>, Node<'_>, Node<'_>)>,
        module: usize,
        block_type: &str,
    ) -> Result<(), ParseError> {
        for (identity, pair, body) in entities {
            let name = identity.last().cloned().unwrap_or_default();
            let (segments, kind) = json_block_identity(block_type, &identity, &self.path);
            let signature = Some(format!("{} \"{}\" (JSON)", block_type, name));
            let container = segments.join("::");
            let symbol = self.add_symbol(
                Some(module),
                Some(container),
                segments,
                kind,
                pair,
                signature,
            )?;
            self.block_body(body, symbol, Some(block_type))?;
        }
        Ok(())
    }

    /// Attributes of one entity body: Property symbols per key, Import
    /// symbols and edges for `source`/`required_providers`, and reference
    /// candidates from `${...}` interpolation strings.
    fn block_body(
        &mut self,
        body: Node<'_>,
        caller: usize,
        block_type: Option<&str>,
    ) -> Result<(), ParseError> {
        if body.kind() != "object" {
            self.scan_interpolations(body, caller)?;
            return Ok(());
        }
        let mut cursor = body.walk();
        for pair in body.named_children(&mut cursor) {
            if pair.kind() != "pair" {
                continue;
            }
            let Some(name) = self.key_text(pair) else {
                continue;
            };
            let is_import = (name == "source" && matches!(block_type, Some("module" | "provider")))
                || block_type == Some("required_providers");
            let kind = if is_import {
                SymbolKind::Import
            } else {
                SymbolKind::Property
            };
            let caller_name = self.symbols[caller].key.qualified_name.clone();
            let attribute = self.add_symbol(
                Some(caller),
                Some(caller_name),
                vec![name.clone()],
                kind,
                pair,
                None,
            )?;
            if is_import {
                let target = self.symbols[attribute].key.qualified_name.clone();
                self.named_relations.push(NamedRelationDraft {
                    from: caller,
                    candidates: vec![target],
                    target_kinds: vec![SymbolKind::Import],
                    kind: EdgeKind::Imports,
                });
            }
            if let Some(value) = pair.child_by_field_name("value") {
                if block_type == Some("terraform") && name == "required_providers" {
                    self.block_body(value, caller, Some("required_providers"))?;
                } else {
                    self.scan_interpolations(value, attribute)?;
                }
            }
        }
        Ok(())
    }

    /// Reference candidates from `${...}` interpolations in string values,
    /// bounded per string; the JSON expression encoding only permits
    /// traversal references inside interpolation markers.
    fn scan_interpolations(&mut self, node: Node<'_>, caller: usize) -> Result<(), ParseError> {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "string" {
                let text = self.text(current).to_owned();
                let mut offset = 0_usize;
                let mut found = 0_usize;
                while found < MAX_INTERPOLATIONS_PER_STRING {
                    let Some(relative) = text[offset..].find("${") else {
                        break;
                    };
                    let start = current.start_byte() + offset + relative + 2;
                    let segments = scan_traversal(self.source, start);
                    if let Some(target) = traversal_target(&segments) {
                        self.calls.push(CallDraft {
                            promoted: false,
                            caller,
                            form: CallForm::Scoped,
                            target_kind: CallTargetKind::Configuration,
                            name: target.name,
                            qualifier: Some(target.qualifier),
                            receiver_hint: segments.first().map(|segment| segment.name.clone()),
                            location: byte_range_of(
                                &self.path,
                                self.source,
                                target.start,
                                target.end,
                            )?,
                        });
                    }
                    offset += relative + 2;
                    found += 1;
                }
            }
            let mut cursor = current.walk();
            for child in current.named_children(&mut cursor) {
                stack.push(child);
            }
        }
        Ok(())
    }
}

/// Mirrors `block_identity` for JSON-encoded entities: the identity segments
/// already contain the block type and labels.
fn json_block_identity(
    block_type: &str,
    identity: &[String],
    path: &RepoRelativePath,
) -> (Vec<String>, SymbolKind) {
    match block_type {
        "resource" | "data" => (identity.to_vec(), SymbolKind::Configuration),
        "module" => (identity.to_vec(), SymbolKind::Module),
        "variable" => {
            let mut segments = vec!["var".to_owned()];
            segments.extend(identity.iter().skip(1).cloned());
            (segments, SymbolKind::Property)
        }
        "output" => (identity.to_vec(), SymbolKind::Property),
        "provider" => (identity.to_vec(), SymbolKind::Configuration),
        "check" => (identity.to_vec(), SymbolKind::Configuration),
        "run" if is_test_file(path) => (identity.to_vec(), SymbolKind::Test),
        "variables" if is_test_file(path) => (identity.to_vec(), SymbolKind::Property),
        _ => (identity.to_vec(), SymbolKind::Configuration),
    }
}

fn json_string_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed);
    (!inner.is_empty()).then(|| inner.to_owned())
}

/// Scalar-column position for a byte offset, matching the native parser's
/// Unicode accounting.
fn position_of(
    source: &str,
    byte: usize,
    path: &RepoRelativePath,
) -> Result<TextPosition, ParseError> {
    let mut line = 1_u32;
    let mut line_start = 0_usize;
    for (index, character) in source.char_indices() {
        if index >= byte {
            break;
        }
        if character == '\n' {
            line = line.saturating_add(1);
            line_start = index + 1;
        }
    }
    let column = source
        .get(line_start..byte)
        .map_or(1, |slice| slice.chars().count() + 1);
    TextPosition::new(
        line,
        u32::try_from(column).map_err(|error| ParseError::PositionInteger {
            path: path.clone(),
            source: error,
        })?,
    )
    .map_err(|error| ParseError::Range {
        path: path.clone(),
        message: error.to_string(),
    })
}

fn range_of(
    path: &RepoRelativePath,
    source: &str,
    node: Node<'_>,
) -> Result<SourceRange, ParseError> {
    byte_range_of(path, source, node.start_byte(), node.end_byte())
}

fn byte_range_of(
    path: &RepoRelativePath,
    source: &str,
    start: usize,
    end: usize,
) -> Result<SourceRange, ParseError> {
    let start = position_of(source, start, path)?;
    let end = position_of(source, end, path)?;
    SourceRange::new(path.clone(), start, end).map_err(|error| ParseError::Range {
        path: path.clone(),
        message: error.to_string(),
    })
}

fn diagnostics_of(
    path: &RepoRelativePath,
    source: &str,
    root: Node<'_>,
) -> Result<(Vec<SyntaxDiagnostic>, u64), ParseError> {
    const MAX_DIAGNOSTICS: usize = 64;
    let mut diagnostics = Vec::new();
    let mut total = 0_u64;
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "ERROR" || node.is_missing() {
            total = total.saturating_add(1);
            if diagnostics.len() < MAX_DIAGNOSTICS {
                diagnostics.push(SyntaxDiagnostic {
                    language: Language::Hcl,
                    range: byte_range_of(path, source, node.start_byte(), node.end_byte())?,
                    kind: SyntaxDiagnosticKind::Error,
                    provenance: Provenance::TreeSitter,
                    precision: Precision::Syntax,
                    cause: SyntaxDiagnosticCause::ParseRecovery,
                    node_kind: node.kind().to_owned(),
                });
            }
        }
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    Ok((diagnostics, total))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use chakra_domain::symbol::{CallTargetKind, SymbolKind};

    use super::*;

    fn parse(path: &str, source: &str) -> Result<ParsedFile, Box<dyn Error>> {
        Ok(parse_terraform_json(
            &RepoRelativePath::new(path)?,
            Arc::from(source),
        )?)
    }

    #[test]
    fn json_entities_mirror_native_hcl_identities() -> Result<(), Box<dyn Error>> {
        let file = parse(
            "main.tf.json",
            r#"{
  "resource": {"aws_vpc": {"main": {"cidr_block": "10.0.0.0/16"}}},
  "data": {"aws_ami": {"ubuntu": {}}},
  "module": {"vpc": {"source": "terraform-aws-modules/vpc/aws"}},
  "variable": {"region": {"type": "string"}},
  "output": {"vpc_id": {"value": "${aws_vpc.main.id}"}},
  "provider": {"aws": {"region": "eu-west-1"}},
  "terraform": {},
  "locals": {"name": "demo"}
}
"#,
        )?;
        let kinds: Vec<_> = file
            .symbols
            .iter()
            .map(|symbol| (symbol.key.qualified_name.as_str(), symbol.key.kind))
            .collect();
        for (name, kind) in [
            ("resource::aws_vpc::main", SymbolKind::Configuration),
            ("data::aws_ami::ubuntu", SymbolKind::Configuration),
            ("module::vpc", SymbolKind::Module),
            ("var::region", SymbolKind::Property),
            ("output::vpc_id", SymbolKind::Property),
            ("provider::aws", SymbolKind::Configuration),
            ("terraform", SymbolKind::Module),
            ("locals", SymbolKind::Module),
        ] {
            assert!(kinds.contains(&(name, kind)), "missing {kind:?} {name}");
        }
        // The module source becomes an Import with an Imports edge.
        let import = file
            .symbols
            .iter()
            .find(|symbol| symbol.key.kind == SymbolKind::Import)
            .ok_or("missing import")?;
        assert_eq!(import.key.qualified_name, "source");
        assert!(
            file.named_relations
                .iter()
                .any(|relation| relation.kind == EdgeKind::Imports)
        );
        // The ${aws_vpc.main.id} interpolation yields a configuration
        // reference candidate with a byte-accurate range.
        let reference = file
            .calls
            .iter()
            .find(|call| call.target_kind == CallTargetKind::Configuration)
            .ok_or("missing reference candidate")?;
        assert_eq!(reference.name, "main");
        assert_eq!(reference.qualifier.as_deref(), Some("resource::aws_vpc"));
        assert_eq!(reference.location.start().line(), 6);
        Ok(())
    }

    #[test]
    fn tfvars_and_tftest_json_variants_have_native_roles() -> Result<(), Box<dyn Error>> {
        let vars = parse("prod.tfvars.json", "{\n  \"region\": \"eu-west-1\"\n}\n")?;
        assert!(
            vars.symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == "tfvars::region"
                    && symbol.key.kind == SymbolKind::Property)
        );
        let test = parse(
            "tests/flow.tftest.json",
            "{\n  \"run\": {\"apply\": {}},\n  \"variables\": {\"region\": \"x\"}\n}\n",
        )?;
        assert!(
            test.symbols
                .iter()
                .any(|symbol| symbol.key.qualified_name == "run::apply"
                    && symbol.key.kind == SymbolKind::Test)
        );
        Ok(())
    }

    #[test]
    fn malformed_json_reports_actionable_diagnostics() -> Result<(), Box<dyn Error>> {
        let file = parse("main.tf.json", "{\"resource\": {\n")?;
        assert!(file.has_errors);
        assert!(!file.diagnostics.is_empty());
        assert!(
            file.diagnostics
                .iter()
                .all(|diagnostic| diagnostic.provenance == Provenance::TreeSitter
                    && diagnostic.precision == Precision::Syntax)
        );
        Ok(())
    }
}
