//! Live-update scenarios: temporary syntax errors and file lifecycle, all
//! synchronized through freshness barriers (fresh queries), never sleeps.

use chakra_domain::query::{QueryService, RepoMapRequest, StatusRequest};
use chakra_domain::state::Freshness;

use super::{candidate, search_symbols, simple_name};
use crate::fixture::with_live;
use crate::manifest::Manifest;
use crate::runner::fixtures_root;
use crate::{Check, ensure};

pub(super) fn syntax_error_recovery(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let original = fixture.read(&expectations.breakable_file)?;

        fixture.write(&expectations.breakable_file, &expectations.broken_content)?;
        let retained = search_symbols(fixture, &expectations.retained_symbol, None)?;
        ensure(
            retained
                .data
                .candidates
                .iter()
                .any(|symbol| symbol.name == expectations.retained_symbol),
            format!(
                "intact declaration `{}` lost while `{}` is broken",
                expectations.retained_symbol, expectations.breakable_file
            ),
        )?;
        let broken = fixture.engine.status(StatusRequest)?;
        ensure(
            broken.data.syntax_diagnostics.files_with_diagnostics >= 1,
            "broken file produced no syntax diagnostics",
        )?;
        ensure(
            broken
                .data
                .syntax_diagnostics
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.range.file().as_str() == expectations.breakable_file),
            format!(
                "no diagnostic is attributed to `{}`",
                expectations.breakable_file
            ),
        )?;

        fixture.write(&expectations.breakable_file, &original)?;
        let recovered = search_symbols(fixture, simple_name(&expectations.caller), None)?;
        candidate(&recovered.data, &expectations.caller)?;
        let healed = fixture.engine.status(StatusRequest)?;
        ensure(
            healed.data.syntax_diagnostics.total_diagnostics == 0
                && healed.data.syntax_diagnostics.diagnostics.is_empty(),
            "diagnostics did not clear after repairing the file",
        )?;
        ensure(
            healed.revision > broken.revision,
            "recovery did not publish a newer revision",
        )?;
        Ok(vec![
            "broken file: actionable diagnostics with file attribution; intact declarations stay queryable"
                .to_owned(),
            "repair: diagnostics cleared on a newer published revision".to_owned(),
        ])
    })
}

pub(super) fn file_lifecycle(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;
        let prefix = &expectations.lifecycle_symbol_prefix;
        let one = format!("{prefix}_one");
        let two = format!("{prefix}_two");
        let three = format!("{prefix}_three");

        let present = |name: &str| -> Check<bool> {
            Ok(!search_symbols(fixture, name, None)?
                .data
                .candidates
                .is_empty())
        };

        // Create.
        fixture.write(
            &expectations.lifecycle_file,
            &expectations.declaration(&one),
        )?;
        ensure(
            present(&one)?,
            "created file was not indexed (read-your-writes)",
        )?;

        // Modify.
        fixture.write(
            &expectations.lifecycle_file,
            &expectations.declaration(&two),
        )?;
        ensure(present(&two)?, "modified content was not indexed")?;
        ensure(!present(&one)?, "replaced declaration survived the modify")?;

        // Atomic save: write a temporary sibling, then rename over the file.
        let swap = match expectations.lifecycle_file.rsplit_once('/') {
            Some((directory, name)) => format!("{directory}/.{name}.chakra-swap"),
            None => format!(".{}.chakra-swap", expectations.lifecycle_file),
        };
        fixture.write(&swap, &expectations.declaration(&three))?;
        fixture.rename(&swap, &expectations.lifecycle_file)?;
        ensure(present(&three)?, "atomic-save content was not indexed")?;
        ensure(
            !present(&two)?,
            "pre-save declaration survived the atomic save",
        )?;

        // Rename.
        fixture.rename(
            &expectations.lifecycle_file,
            &expectations.lifecycle_renamed_file,
        )?;
        ensure(present(&three)?, "renamed file content was lost")?;

        // Delete.
        fixture.remove(&expectations.lifecycle_renamed_file)?;
        ensure(
            !present(&three)?,
            "deleted file content survived the delete",
        )?;
        let map = fixture.engine.repo_map(RepoMapRequest {
            include_project_scope: false,
            limit: None,
            ..RepoMapRequest::default()
        })?;
        ensure(
            map.freshness == Freshness::Fresh,
            "repo_map did not observe a fresh revision",
        )?;
        ensure(
            map.data
                .files
                .iter()
                .all(|file| file.path.as_str() != expectations.lifecycle_renamed_file),
            "deleted file still listed in repo_map",
        )?;
        Ok(vec![
            "create/modify/atomic-save/rename/delete: each step visible to the next fresh query"
                .to_owned(),
        ])
    })
}
