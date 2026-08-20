//! LSP-to-domain conversion. No LSP type leaves this crate (invariants 5, 6,
//! 10); UTF-16 positions and file URIs are translated against the pinned
//! syntax snapshot, and every converted fact carries `Provenance::Pyright`.

use std::path::Path;
use std::str::FromStr;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::Provenance;
use chakra_engine::{PreciseRelation, ProviderWorkspace};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Position, Range, Uri,
};
use url::Url;

use crate::worker::ProviderError;

const MAX_REPRESENTATIVE_CALL_SITES: usize = 3;

pub(crate) fn directory_uri(path: &Path) -> Result<Uri, ProviderError> {
    let url = Url::from_directory_path(path)
        .map_err(|()| ProviderError::InvalidUri(path.display().to_string()))?;
    Uri::from_str(url.as_str()).map_err(|_| ProviderError::InvalidUri(path.display().to_string()))
}

pub(crate) fn path_to_uri(root: &Path, path: &RepoRelativePath) -> Result<Uri, ProviderError> {
    let absolute = path
        .as_str()
        .split('/')
        .fold(root.to_path_buf(), |base, component| base.join(component));
    let url = Url::from_file_path(&absolute)
        .map_err(|()| ProviderError::InvalidUri(absolute.display().to_string()))?;
    Uri::from_str(url.as_str())
        .map_err(|_| ProviderError::InvalidUri(absolute.display().to_string()))
}

pub(crate) fn uri_to_path(root: &Path, uri: &Uri) -> Option<RepoRelativePath> {
    let absolute = Url::parse(uri.as_str()).ok()?.to_file_path().ok()?;
    let relative = absolute.strip_prefix(root).ok()?;
    let raw = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    RepoRelativePath::new(raw).ok()
}

fn source_line(source: &str, zero_based_line: u32) -> Option<&str> {
    source.lines().nth(usize::try_from(zero_based_line).ok()?)
}

pub(crate) fn chakra_to_lsp_position(source: &str, position: TextPosition) -> Option<Position> {
    let line = position.line().checked_sub(1)?;
    let scalar_column = usize::try_from(position.column().checked_sub(1)?).ok()?;
    let text = source_line(source, line)?;
    let prefix: String = text.chars().take(scalar_column).collect();
    if prefix.chars().count() != scalar_column {
        return None;
    }
    Some(Position::new(
        line,
        u32::try_from(prefix.encode_utf16().count()).ok()?,
    ))
}

pub(crate) fn lsp_to_chakra_position(source: &str, position: Position) -> Option<TextPosition> {
    let text = source_line(source, position.line)?;
    let target = usize::try_from(position.character).ok()?;
    let mut utf16 = 0_usize;
    let mut scalars = 0_usize;
    for character in text.chars() {
        if utf16 == target {
            break;
        }
        let next = utf16.checked_add(character.len_utf16())?;
        if next > target {
            return None;
        }
        utf16 = next;
        scalars += 1;
    }
    if utf16 != target {
        return None;
    }
    TextPosition::new(
        position.line.checked_add(1)?,
        u32::try_from(scalars).ok()?.checked_add(1)?,
    )
    .ok()
}

fn convert_range(path: RepoRelativePath, source: &str, range: Range) -> Option<SourceRange> {
    SourceRange::new(
        path,
        lsp_to_chakra_position(source, range.start)?,
        lsp_to_chakra_position(source, range.end)?,
    )
    .ok()
}

pub(crate) fn find_symbol_position(
    source: &str,
    name: &str,
    declaration: &SourceRange,
) -> Result<Position, ProviderError> {
    let start_line = declaration.start().line();
    let end_line = declaration.end().line();
    let start_index =
        usize::try_from(start_line - 1).map_err(|_| ProviderError::InvalidPosition)?;
    let line_count =
        usize::try_from(end_line - start_line + 1).map_err(|_| ProviderError::InvalidPosition)?;
    for (offset, line) in source
        .lines()
        .skip(start_index)
        .take(line_count)
        .enumerate()
    {
        let line_number = start_line
            .checked_add(u32::try_from(offset).map_err(|_| ProviderError::InvalidPosition)?)
            .ok_or(ProviderError::InvalidPosition)?;
        let minimum = if line_number == start_line {
            usize::try_from(declaration.start().column() - 1)
                .map_err(|_| ProviderError::InvalidPosition)?
        } else {
            0
        };
        for (byte, _) in line.match_indices(name) {
            let scalar = line[..byte].chars().count();
            if scalar < minimum || !identifier_boundary(line, byte, name.len()) {
                continue;
            }
            let chakra = TextPosition::new(
                line_number,
                u32::try_from(scalar + 1).map_err(|_| ProviderError::InvalidPosition)?,
            )
            .map_err(|_| ProviderError::InvalidPosition)?;
            return chakra_to_lsp_position(source, chakra).ok_or(ProviderError::InvalidPosition);
        }
    }
    chakra_to_lsp_position(source, declaration.start()).ok_or(ProviderError::InvalidPosition)
}

fn identifier_boundary(line: &str, byte: usize, length: usize) -> bool {
    let before = line[..byte].chars().next_back();
    let after = line[byte + length..].chars().next();
    !before.is_some_and(|character| character == '_' || character.is_alphanumeric())
        && !after.is_some_and(|character| character == '_' || character.is_alphanumeric())
}

pub(crate) fn item_declaration(
    item: &CallHierarchyItem,
    workspace: &ProviderWorkspace,
) -> Option<(RepoRelativePath, SourceRange)> {
    let path = uri_to_path(&workspace.repository_root, &item.uri)?;
    let document = workspace.document(&path)?;
    let range = convert_range(path.clone(), &document.source, item.selection_range)?;
    Some((path, range))
}

pub(crate) fn convert_incoming(
    calls: Vec<CallHierarchyIncomingCall>,
    workspace: &ProviderWorkspace,
    limit: usize,
    truncated: &mut bool,
) -> Vec<PreciseRelation> {
    let mut result = Vec::new();
    for call in calls {
        let Some((path, declaration)) = item_declaration(&call.from, workspace) else {
            continue;
        };
        if result.len() == limit {
            *truncated = true;
            break;
        }
        let source = workspace.document(&path);
        let occurrence_count = u64::try_from(call.from_ranges.len().max(1)).unwrap_or(u64::MAX);
        let call_sites = source.as_ref().map_or_else(Vec::new, |document| {
            call.from_ranges
                .iter()
                .filter_map(|range| convert_range(path.clone(), &document.source, *range))
                .take(MAX_REPRESENTATIVE_CALL_SITES)
                .collect()
        });
        result.push(PreciseRelation {
            name: call.from.name,
            declaration,
            occurrence_count,
            call_sites,
            provenance: Provenance::Pyright,
        });
    }
    result
}

pub(crate) fn convert_outgoing(
    calls: Vec<CallHierarchyOutgoingCall>,
    workspace: &ProviderWorkspace,
    caller_path: &RepoRelativePath,
    limit: usize,
    truncated: &mut bool,
) -> Vec<PreciseRelation> {
    let caller_source = workspace.document(caller_path);
    let mut result = Vec::new();
    for call in calls {
        let Some((_, declaration)) = item_declaration(&call.to, workspace) else {
            continue;
        };
        if result.len() == limit {
            *truncated = true;
            break;
        }
        let occurrence_count = u64::try_from(call.from_ranges.len().max(1)).unwrap_or(u64::MAX);
        let call_sites = caller_source.as_ref().map_or_else(Vec::new, |document| {
            call.from_ranges
                .iter()
                .filter_map(|range| convert_range(caller_path.clone(), &document.source, *range))
                .take(MAX_REPRESENTATIVE_CALL_SITES)
                .collect()
        });
        result.push(PreciseRelation {
            name: call.to.name,
            declaration,
            occurrence_count,
            call_sites,
            provenance: Provenance::Pyright,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::sync::Arc;

    use chakra_domain::revision::Revision;
    use chakra_engine::ProviderDocument;
    use lsp_types::SymbolKind;
    use tempfile::TempDir;

    use super::*;

    fn python_document(path: &RepoRelativePath, source: &str) -> ProviderDocument {
        ProviderDocument {
            path: path.clone(),
            source: Arc::from(source),
            language: chakra_domain::symbol::Language::Python,
        }
    }

    fn item(name: &str, uri: Uri) -> CallHierarchyItem {
        let selection = Range::new(Position::new(0, 4), Position::new(0, 10));
        CallHierarchyItem {
            name: name.to_owned(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri,
            range: selection,
            selection_range: selection,
            data: None,
        }
    }

    #[test]
    fn incoming_relations_carry_pyright_provenance_and_bounded_call_sites()
    -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let path = RepoRelativePath::new("src/index.py")?;
        let workspace = ProviderWorkspace::from_documents(
            repository.path().to_path_buf(),
            Revision(4),
            vec![python_document(&path, "def target():\n    pass\n")],
        );
        let inside = path_to_uri(repository.path(), &path)?;
        let range = Range::new(Position::new(0, 4), Position::new(0, 10));
        let calls = vec![CallHierarchyIncomingCall {
            from: item("target", inside),
            from_ranges: vec![range; 5],
        }];
        let mut truncated = false;

        let converted = convert_incoming(calls, &workspace, 10, &mut truncated);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].occurrence_count, 5);
        assert_eq!(converted[0].call_sites.len(), 3);
        assert_eq!(converted[0].provenance, Provenance::Pyright);
        assert!(!truncated);
        Ok(())
    }

    #[test]
    fn relations_outside_the_workspace_are_dropped_before_the_limit() -> Result<(), Box<dyn Error>>
    {
        let repository = TempDir::new()?;
        let path = RepoRelativePath::new("src/index.py")?;
        let workspace = ProviderWorkspace::from_documents(
            repository.path().to_path_buf(),
            Revision(4),
            vec![python_document(&path, "def inside():\n    pass\n")],
        );
        let outside_root = TempDir::new()?;
        let outside = path_to_uri(outside_root.path(), &path)?;
        let inside = path_to_uri(repository.path(), &path)?;
        let calls = vec![
            CallHierarchyIncomingCall {
                from: item("outside", outside),
                from_ranges: Vec::new(),
            },
            CallHierarchyIncomingCall {
                from: item("inside", inside),
                from_ranges: Vec::new(),
            },
        ];
        let mut truncated = false;

        let converted = convert_incoming(calls, &workspace, 1, &mut truncated);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, "inside");
        assert!(!truncated);
        Ok(())
    }

    #[test]
    fn unicode_positions_round_trip_through_lsp_utf16() -> Result<(), Box<dyn Error>> {
        let source = "const café🦀 = 1;\n";
        // Column 11 is the 🦀: the "const café" prefix is 10 UTF-16 units.
        let chakra = TextPosition::new(1, 11)?;
        let lsp = chakra_to_lsp_position(source, chakra).ok_or("position conversion failed")?;
        assert_eq!(lsp.character, 10);
        assert_eq!(lsp_to_chakra_position(source, lsp), Some(chakra));
        Ok(())
    }

    #[test]
    fn path_uri_round_trip_is_repository_scoped() -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let path = RepoRelativePath::new("src/index.py")?;
        let uri = path_to_uri(root.path(), &path)?;
        assert_eq!(uri_to_path(root.path(), &uri), Some(path));
        Ok(())
    }
}
