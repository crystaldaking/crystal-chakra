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

fn item_declaration(
    item: &CallHierarchyItem,
    workspace: &ProviderWorkspace,
) -> Option<(RepoRelativePath, SourceRange)> {
    let path = uri_to_path(&workspace.repository_root, &item.uri)?;
    let source = workspace
        .documents
        .iter()
        .find(|document| document.path == path)?
        .source
        .as_ref();
    let range = convert_range(path.clone(), source, item.selection_range)?;
    Some((path, range))
}

pub(crate) fn convert_incoming(
    calls: Vec<CallHierarchyIncomingCall>,
    workspace: &ProviderWorkspace,
    limit: usize,
    truncated: &mut bool,
) -> Vec<PreciseRelation> {
    if calls.len() > limit {
        *truncated = true;
    }
    let mut result = Vec::new();
    for call in calls.into_iter().take(limit) {
        let Some((path, declaration)) = item_declaration(&call.from, workspace) else {
            continue;
        };
        let source = workspace
            .documents
            .iter()
            .find(|document| document.path == path)
            .map(|document| document.source.as_ref());
        let call_site = source.and_then(|source| {
            call.from_ranges
                .first()
                .and_then(|range| convert_range(path.clone(), source, *range))
        });
        result.push(PreciseRelation {
            name: call.from.name,
            declaration,
            call_site,
            provenance: Provenance::RustAnalyzer,
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
    let caller_source = workspace
        .documents
        .iter()
        .find(|document| document.path == *caller_path)
        .map(|document| document.source.as_ref());
    let mut result = Vec::new();
    if calls.len() > limit {
        *truncated = true;
    }
    for call in calls.into_iter().take(limit) {
        let Some((_, declaration)) = item_declaration(&call.to, workspace) else {
            continue;
        };
        let call_site = caller_source.and_then(|source| {
            call.from_ranges
                .first()
                .and_then(|range| convert_range(caller_path.clone(), source, *range))
        });
        result.push(PreciseRelation {
            name: call.to.name,
            declaration,
            call_site,
            provenance: Provenance::RustAnalyzer,
        });
    }
    result
}
