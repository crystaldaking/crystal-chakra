//! Real-repository coverage for `diff_context` Git baseline scopes.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use chakra_domain::location::RepoRelativePath;
use chakra_domain::query::{ChangeKind, DiffScope};
use chakra_domain::revision::Revision;
use chakra_engine::{DiffDocument, DiffWorkspace, WorkspaceDiff, WorkspaceDiffProvider};
use chakra_git::GitWorkspaceDiff;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into())
    }
}

fn git_text(root: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn write(root: &Path, path: &str, source: &str) -> Result<(), Box<dyn Error>> {
    let absolute = root.join(path);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(absolute, source)?;
    Ok(())
}

fn init_repository() -> Result<TempDir, Box<dyn Error>> {
    let repository = TempDir::new()?;
    git(repository.path(), &["init", "--quiet"])?;
    git(
        repository.path(),
        &["config", "user.email", "tests@example.invalid"],
    )?;
    git(repository.path(), &["config", "user.name", "Chakra Tests"])?;
    Ok(repository)
}

fn workspace(
    root: &Path,
    scope: DiffScope,
    paths: &[&str],
) -> Result<DiffWorkspace, Box<dyn Error>> {
    let documents = paths
        .iter()
        .map(|path| {
            Ok(DiffDocument {
                path: RepoRelativePath::new(*path)?,
                source: Arc::<str>::from(fs::read_to_string(root.join(path))?),
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    Ok(DiffWorkspace {
        repository_root: root.to_path_buf(),
        revision: Revision(17),
        scope,
        documents,
    })
}

fn changes(diff: &WorkspaceDiff) -> BTreeMap<&str, ChangeKind> {
    diff.files
        .iter()
        .map(|change| (change.path.as_str(), change.change))
        .collect()
}

#[test]
fn every_scope_combines_committed_and_final_worktree_state_as_documented()
-> Result<(), Box<dyn Error>> {
    let repository = init_repository()?;
    let root = repository.path();
    for (path, source) in [
        ("src/staged.rs", "pub fn staged_before() {}\n"),
        ("src/unstaged.rs", "pub fn unstaged_before() {}\n"),
        ("src/deleted.rs", "pub fn deleted() {}\n"),
        ("src/old.rs", "pub fn renamed() {}\n"),
    ] {
        write(root, path, source)?;
    }
    git(root, &["add", "src"])?;
    git(root, &["commit", "--quiet", "-m", "base"])?;
    git(root, &["branch", "develop"])?;
    let base_commit = git_text(root, &["rev-parse", "develop"])?;

    git(root, &["switch", "--quiet", "-c", "feature"])?;
    write(root, "src/committed.rs", "pub fn committed() {}\n")?;
    git(root, &["add", "src/committed.rs"])?;
    git(root, &["commit", "--quiet", "-m", "feature change"])?;
    let head_commit = git_text(root, &["rev-parse", "HEAD"])?;

    write(root, "src/staged.rs", "pub fn staged_after() {}\n")?;
    git(root, &["add", "src/staged.rs"])?;
    write(root, "src/unstaged.rs", "pub fn unstaged_after() {}\n")?;
    fs::remove_file(root.join("src/deleted.rs"))?;
    git(root, &["mv", "src/old.rs", "src/renamed.rs"])?;
    write(root, "src/untracked.rs", "pub fn untracked() {}\n")?;

    let current_documents = [
        "src/committed.rs",
        "src/staged.rs",
        "src/unstaged.rs",
        "src/renamed.rs",
        "src/untracked.rs",
    ];
    let worktree =
        GitWorkspaceDiff.diff(workspace(root, DiffScope::Worktree, &current_documents)?)?;
    assert_eq!(
        worktree.scope.base_commit.as_deref(),
        Some(head_commit.as_str())
    );
    let worktree_changes = changes(&worktree);
    assert!(!worktree_changes.contains_key("src/committed.rs"));

    for scope in [
        DiffScope::BaseRef {
            reference: "refs/heads/develop".to_owned(),
        },
        DiffScope::MergeBase {
            reference: "refs/heads/develop".to_owned(),
        },
    ] {
        let diff = GitWorkspaceDiff.diff(workspace(root, scope.clone(), &current_documents)?)?;
        assert_eq!(diff.scope.requested, scope);
        assert_eq!(
            diff.scope.base_commit.as_deref(),
            Some(base_commit.as_str())
        );
        assert_eq!(changes(&diff)["src/committed.rs"], ChangeKind::Added);
        assert_common_worktree_changes(&diff)?;
    }
    assert_common_worktree_changes(&worktree)?;
    Ok(())
}

fn assert_common_worktree_changes(diff: &WorkspaceDiff) -> Result<(), Box<dyn Error>> {
    let by_path: BTreeMap<_, _> = diff
        .files
        .iter()
        .map(|change| (change.path.as_str(), change))
        .collect();
    assert_eq!(by_path["src/staged.rs"].change, ChangeKind::Modified);
    assert_eq!(by_path["src/unstaged.rs"].change, ChangeKind::Modified);
    assert_eq!(by_path["src/deleted.rs"].change, ChangeKind::Deleted);
    assert_eq!(by_path["src/untracked.rs"].change, ChangeKind::Added);
    assert_eq!(by_path["src/renamed.rs"].change, ChangeKind::Renamed);
    assert_eq!(
        by_path["src/renamed.rs"]
            .previous_path
            .as_ref()
            .map(RepoRelativePath::as_str),
        Some("src/old.rs")
    );
    Ok(())
}

#[test]
fn base_ref_and_merge_base_have_two_dot_and_three_dot_semantics() -> Result<(), Box<dyn Error>> {
    let repository = init_repository()?;
    let root = repository.path();
    write(root, "src/common.rs", "pub fn common_base() {}\n")?;
    git(root, &["add", "src/common.rs"])?;
    git(root, &["commit", "--quiet", "-m", "common base"])?;
    git(root, &["branch", "develop"])?;
    let fork_commit = git_text(root, &["rev-parse", "HEAD"])?;

    git(root, &["switch", "--quiet", "-c", "feature"])?;
    write(root, "src/feature.rs", "pub fn feature() {}\n")?;
    git(root, &["add", "src/feature.rs"])?;
    git(root, &["commit", "--quiet", "-m", "feature"])?;

    git(root, &["switch", "--quiet", "develop"])?;
    write(root, "src/common.rs", "pub fn common_on_develop() {}\n")?;
    git(root, &["add", "src/common.rs"])?;
    git(root, &["commit", "--quiet", "-m", "develop moved"])?;
    let develop_commit = git_text(root, &["rev-parse", "develop"])?;
    git(root, &["switch", "--quiet", "feature"])?;

    let documents = ["src/common.rs", "src/feature.rs"];
    let direct = GitWorkspaceDiff.diff(workspace(
        root,
        DiffScope::BaseRef {
            reference: "develop".to_owned(),
        },
        &documents,
    )?)?;
    assert_eq!(
        direct.scope.base_commit.as_deref(),
        Some(develop_commit.as_str())
    );
    assert_eq!(changes(&direct)["src/common.rs"], ChangeKind::Modified);
    assert_eq!(changes(&direct)["src/feature.rs"], ChangeKind::Added);

    let merge_base = GitWorkspaceDiff.diff(workspace(
        root,
        DiffScope::MergeBase {
            reference: "develop".to_owned(),
        },
        &documents,
    )?)?;
    assert_eq!(
        merge_base.scope.base_commit.as_deref(),
        Some(fork_commit.as_str())
    );
    assert_eq!(changes(&merge_base).len(), 1);
    assert_eq!(changes(&merge_base)["src/feature.rs"], ChangeKind::Added);
    Ok(())
}

#[test]
fn invalid_and_ambiguous_references_fail_explicitly() -> Result<(), Box<dyn Error>> {
    let repository = init_repository()?;
    let root = repository.path();
    write(root, "src/lib.rs", "pub fn indexed() {}\n")?;
    git(root, &["add", "src/lib.rs"])?;
    git(root, &["commit", "--quiet", "-m", "base"])?;

    let missing = GitWorkspaceDiff.diff(workspace(
        root,
        DiffScope::BaseRef {
            reference: "does-not-exist".to_owned(),
        },
        &["src/lib.rs"],
    )?);
    assert!(
        missing
            .err()
            .is_some_and(|error| error.to_string().contains("invalid Git base reference"))
    );

    git(root, &["branch", "shared"])?;
    git(root, &["tag", "shared"])?;
    let ambiguous = GitWorkspaceDiff.diff(workspace(
        root,
        DiffScope::BaseRef {
            reference: "shared".to_owned(),
        },
        &["src/lib.rs"],
    )?);
    assert!(
        ambiguous
            .err()
            .is_some_and(|error| error.to_string().contains("ambiguous Git base reference"))
    );
    Ok(())
}
