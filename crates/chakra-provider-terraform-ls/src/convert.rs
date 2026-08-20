//! LSP-to-domain conversion for terraform-ls reference facts.

use std::path::Path;
use std::str::FromStr;

use chakra_domain::location::{RepoRelativePath, SourceRange, TextPosition};
use chakra_domain::provenance::Provenance;
use chakra_engine::{PreciseRelation, ProviderWorkspace};
use lsp_types::{Location, Position, Range, SymbolKind, Uri};
use url::Url;

use crate::worker::ProviderError;

const MAX_REPRESENTATIVE_CALL_SITES: usize = 3;

#[derive(Debug, Clone)]
pub(crate) struct CallerSymbol {
    pub name: String,
    pub path: RepoRelativePath,
    pub range: Range,
    pub kind: SymbolKind,
}

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

pub(crate) fn convert_range(
    path: RepoRelativePath,
    source: &str,
    range: Range,
) -> Option<SourceRange> {
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
    !before.is_some_and(|character| {
        character == '_' || character == '-' || character.is_alphanumeric()
    }) && !after.is_some_and(|character| {
        character == '_' || character == '-' || character.is_alphanumeric()
    })
}

fn contains(range: Range, position: Position) -> bool {
    range.start <= position && position < range.end
}

fn span_key(range: Range) -> (u32, u32) {
    (
        range.end.line.saturating_sub(range.start.line),
        range.end.character.saturating_sub(range.start.character),
    )
}

fn hcl_container_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::MODULE
            | SymbolKind::NAMESPACE
            | SymbolKind::CLASS
            | SymbolKind::METHOD
            | SymbolKind::FUNCTION
            | SymbolKind::OBJECT
            | SymbolKind::STRUCT
    )
}

fn hcl_symbol_name(raw: &str) -> String {
    raw.split(|character: char| !(character.is_alphanumeric() || matches!(character, '_' | '-')))
        .rfind(|part| !part.is_empty())
        .unwrap_or(raw)
        .to_owned()
}

pub(crate) fn convert_references(
    references: Vec<Location>,
    symbols: &[CallerSymbol],
    workspace: &ProviderWorkspace,
    limit: usize,
    truncated: &mut bool,
) -> Vec<PreciseRelation> {
    let mut relations: Vec<PreciseRelation> = Vec::new();
    for reference in references {
        let Some(path) = uri_to_path(&workspace.repository_root, &reference.uri) else {
            continue;
        };
        let Some(document) = workspace.document(&path) else {
            continue;
        };
        let Some(caller) = symbols
            .iter()
            .filter(|symbol| {
                symbol.path == path
                    && hcl_container_kind(symbol.kind)
                    && contains(symbol.range, reference.range.start)
            })
            .min_by_key(|symbol| span_key(symbol.range))
        else {
            continue;
        };
        let Some(declaration) = convert_range(path.clone(), &document.source, caller.range) else {
            continue;
        };
        let Some(call_site) = convert_range(path, &document.source, reference.range) else {
            continue;
        };
        if let Some(existing) = relations
            .iter_mut()
            .find(|relation| relation.declaration == declaration)
        {
            existing.occurrence_count = existing.occurrence_count.saturating_add(1);
            if existing.call_sites.len() < MAX_REPRESENTATIVE_CALL_SITES {
                existing.call_sites.push(call_site);
            }
            continue;
        }
        if relations.len() == limit {
            *truncated = true;
            continue;
        }
        relations.push(PreciseRelation {
            name: hcl_symbol_name(&caller.name),
            declaration,
            occurrence_count: 1,
            call_sites: vec![call_site],
            provenance: Provenance::TerraformLs,
        });
    }
    relations
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::sync::Arc;

    use chakra_domain::revision::Revision;
    use chakra_domain::symbol::Language;
    use chakra_engine::ProviderDocument;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn references_map_to_enclosing_hcl_blocks() -> Result<(), Box<dyn Error>> {
        let repository = TempDir::new()?;
        let path = RepoRelativePath::new("main.tf")?;
        let source: Arc<str> = Arc::from(
            "resource \"null_resource\" \"caller\" {\n  first = null_resource.target.id\n  second = null_resource.target.id\n}\n",
        );
        let workspace = ProviderWorkspace::from_documents(
            repository.path().to_path_buf(),
            Revision(4),
            vec![ProviderDocument {
                path: path.clone(),
                source,
                language: Language::Hcl,
            }],
        );
        let uri = path_to_uri(repository.path(), &path)?;
        let references = vec![
            Location::new(
                uri.clone(),
                Range::new(Position::new(1, 10), Position::new(1, 30)),
            ),
            Location::new(uri, Range::new(Position::new(2, 11), Position::new(2, 31))),
        ];
        let symbols = vec![CallerSymbol {
            name: "null_resource.caller".to_owned(),
            path,
            range: Range::new(Position::new(0, 0), Position::new(3, 1)),
            kind: SymbolKind::OBJECT,
        }];
        let mut truncated = false;
        let relations = convert_references(references, &symbols, &workspace, 10, &mut truncated);
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].name, "caller");
        assert_eq!(relations[0].occurrence_count, 2);
        assert_eq!(relations[0].call_sites.len(), 2);
        assert_eq!(relations[0].provenance, Provenance::TerraformLs);
        assert!(!truncated);
        Ok(())
    }

    #[test]
    fn unicode_positions_round_trip_through_lsp_utf16() -> Result<(), Box<dyn Error>> {
        let source = "café🦀() { true; }\n";
        let chakra = TextPosition::new(1, 5)?;
        let lsp = chakra_to_lsp_position(source, chakra).ok_or("position conversion failed")?;
        assert_eq!(lsp.character, 4);
        assert_eq!(lsp_to_chakra_position(source, lsp), Some(chakra));
        Ok(())
    }

    #[test]
    fn lsp_symbol_ranges_exclude_the_end_position() {
        let range = Range::new(Position::new(2, 0), Position::new(4, 1));
        assert!(contains(range, Position::new(2, 0)));
        assert!(contains(range, Position::new(4, 0)));
        assert!(!contains(range, Position::new(4, 1)));
    }
}
