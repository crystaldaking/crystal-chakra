//! `diff_context` scope scenario over real commits in the temporary worktree.

use chakra_domain::provenance::Provenance;
use chakra_domain::query::{ChangeKind, DiffContextRequest, DiffScope, QueryService};
use chakra_domain::state::Freshness;

use crate::fixture::{commit_all, with_live};
use crate::manifest::Manifest;
use crate::runner::fixtures_root;
use crate::{Check, ensure};

pub(super) fn diff_context_scopes(manifest: &Manifest) -> Check<Vec<String>> {
    with_live(&fixtures_root().join(&manifest.language), |fixture| {
        let expectations = &manifest.expectations;

        let base = commit_all(fixture, "conformance base")?;
        fixture.append(
            &expectations.diff_second_commit_file,
            &expectations.snippet("conformance_second_commit_marker"),
        )?;
        commit_all(fixture, "second commit")?;
        fixture.append(
            &expectations.diff_worktree_file,
            &expectations.snippet("conformance_worktree_marker"),
        )?;

        let worktree = fixture.engine.diff_context(DiffContextRequest::default())?;
        ensure(
            worktree.freshness == Freshness::Fresh,
            "worktree diff did not observe a fresh revision",
        )?;
        ensure(
            worktree.data.scope.base_commit.is_some(),
            "worktree scope must resolve a base commit",
        )?;
        ensure(
            worktree.data.changed_files.len() == 1,
            format!(
                "worktree scope: expected exactly one changed file, found {:?}",
                worktree
                    .data
                    .changed_files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>()
            ),
        )?;
        let changed = &worktree.data.changed_files[0];
        ensure(
            changed.path.as_str() == expectations.diff_worktree_file
                && changed.change == ChangeKind::Modified
                && changed.previous_path.is_none(),
            format!("worktree change attribution wrong: {changed:?}"),
        )?;
        ensure(
            changed.provenance == Provenance::Git,
            format!(
                "changed file provenance is {:?}, not git",
                changed.provenance
            ),
        )?;
        ensure(
            worktree.data.changed_symbols.iter().any(|symbol| {
                symbol.symbol.location.file().as_str() == expectations.diff_worktree_file
            }),
            "no changed symbol attributed to the modified worktree file",
        )?;
        ensure(
            worktree.data.changed_symbols.iter().all(|symbol| {
                worktree
                    .data
                    .changed_files
                    .iter()
                    .any(|file| file.path == *symbol.symbol.location.file())
            }),
            "a changed symbol is attributed to an unchanged file",
        )?;

        let base_ref = fixture.engine.diff_context(DiffContextRequest {
            scope: DiffScope::BaseRef {
                reference: base.clone(),
            },
            ..DiffContextRequest::default()
        })?;
        ensure(
            base_ref.data.scope.base_commit.as_deref() == Some(base.as_str()),
            "base-ref scope resolved a different base commit",
        )?;
        let mut paths: Vec<&str> = base_ref
            .data
            .changed_files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        paths.sort_unstable();
        let mut expected = [
            expectations.diff_second_commit_file.as_str(),
            expectations.diff_worktree_file.as_str(),
        ];
        expected.sort_unstable();
        ensure(
            paths == expected,
            format!("base-ref scope: expected {expected:?}, found {paths:?}"),
        )?;
        ensure(
            base_ref
                .data
                .changed_files
                .iter()
                .all(|file| file.change == ChangeKind::Modified),
            "base-ref scope must report both files as modified",
        )?;
        Ok(vec![
            "changed-file facts: git provenance; worktree and base-ref scopes attribute disjoint file sets"
                .to_owned(),
        ])
    })
}
