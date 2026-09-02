use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use chakra_git::{resolve_repository_identity, resolve_workspace_identity};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

fn initialize(root: &Path, commit: bool) -> Result<(), Box<dyn Error>> {
    git(root, &["init", "-q"])?;
    git(root, &["config", "user.email", "chakra@example.invalid"])?;
    git(root, &["config", "user.name", "Chakra Tests"])?;
    if commit {
        fs::write(root.join("README.md"), "fixture\n")?;
        git(root, &["add", "README.md"])?;
        git(root, &["commit", "-q", "-m", "initial"])?;
    }
    Ok(())
}

#[test]
fn committed_repository_identity_survives_path_move_and_remote_change() -> Result<(), Box<dyn Error>>
{
    let parent = TempDir::new()?;
    let original = parent.path().join("original");
    let moved = parent.path().join("moved");
    fs::create_dir(&original)?;
    initialize(&original, true)?;

    let before = resolve_workspace_identity(&original)?;
    git(
        &original,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/first.git",
        ],
    )?;
    let after_remote_add = resolve_repository_identity(&original)?;
    git(
        &original,
        &[
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/second.git",
        ],
    )?;
    let after_remote_change = resolve_repository_identity(&original)?;
    assert_eq!(before.repository, after_remote_add);
    assert_eq!(before.repository, after_remote_change);

    fs::rename(&original, &moved)?;
    let after_move = resolve_workspace_identity(&moved)?;
    assert_eq!(before.repository, after_move.repository);
    assert_ne!(before.workspace, after_move.workspace);
    assert_eq!(after_move.root, fs::canonicalize(&moved)?);
    Ok(())
}

#[test]
fn linked_worktrees_share_repository_but_not_workspace_identity() -> Result<(), Box<dyn Error>> {
    let parent = TempDir::new()?;
    let primary = parent.path().join("primary");
    let linked = parent.path().join("linked");
    fs::create_dir(&primary)?;
    initialize(&primary, true)?;
    git(
        &primary,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked-test",
            linked.to_str().ok_or("non-UTF-8 worktree path")?,
        ],
    )?;

    let primary_identity = resolve_workspace_identity(&primary)?;
    let linked_identity = resolve_workspace_identity(&linked)?;
    assert_eq!(primary_identity.repository, linked_identity.repository);
    assert_ne!(primary_identity.workspace, linked_identity.workspace);
    Ok(())
}

#[test]
fn distinct_unborn_local_repositories_do_not_collapse_to_one_identity() -> Result<(), Box<dyn Error>>
{
    let first = TempDir::new()?;
    let second = TempDir::new()?;
    initialize(first.path(), false)?;
    initialize(second.path(), false)?;

    let first_id = resolve_repository_identity(first.path())?;
    let first_again = resolve_repository_identity(first.path())?;
    let second_id = resolve_repository_identity(second.path())?;
    assert_eq!(first_id, first_again);
    assert_ne!(first_id, second_id);
    assert!(first_id.as_str().starts_with("git-unborn:"));
    Ok(())
}

#[test]
fn unborn_repository_identity_survives_path_move() -> Result<(), Box<dyn Error>> {
    let parent = TempDir::new()?;
    let original = parent.path().join("original-unborn");
    let moved = parent.path().join("moved-unborn");
    fs::create_dir(&original)?;
    initialize(&original, false)?;

    let before = resolve_repository_identity(&original)?;
    fs::rename(&original, &moved)?;
    let after = resolve_repository_identity(&moved)?;

    assert_eq!(before, after);
    Ok(())
}
