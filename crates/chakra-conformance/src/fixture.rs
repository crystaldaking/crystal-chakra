//! Temporary worktree plus live engine wiring for scenario runs.
//!
//! Mirrors the wiring used by `crates/chakra-language/tests/live_updates.rs`:
//! copy the fixture tree into a temporary Git repository, build the composed
//! syntax index, publish it atomically, then start the live index so every
//! `RequireFresh` query doubles as a freshness barrier (never a sleep).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use chakra_domain::identity::WorkspaceIdentity;
use chakra_domain::state::{Freshness, WorkspaceStatus};
use chakra_engine::WorkspaceEngine;
use chakra_language::{LiveIndex, index_repository, start_live_index};
use tempfile::TempDir;

use crate::{Check, failure};

/// A live workspace under test: temporary repository, engine, and the owned
/// live index. Freshness barriers come from `RequireFresh` queries.
pub struct LiveFixture {
    repository: TempDir,
    /// Engine under test, shared with the live index.
    pub engine: Arc<WorkspaceEngine>,
    live: LiveIndex,
}

impl LiveFixture {
    /// Copies `fixture_dir` (excluding `manifest.json`) into a fresh Git
    /// worktree and starts the live index over it.
    pub fn start(fixture_dir: &Path) -> Check<Self> {
        let repository = TempDir::new()?;
        copy_fixture_tree(fixture_dir, repository.path())?;
        git(repository.path(), &["init", "--quiet"])?;
        let report = index_repository(repository.path())?;
        let identity = WorkspaceIdentity::for_primary_worktree(&report.repository_root)?;
        let engine = Arc::new(WorkspaceEngine::new(identity));
        let mut update = engine.begin_update();
        update.replace_graph(report.graph);
        update.set_indexing(report.metrics.indexing);
        update.set_status(WorkspaceStatus::Indexing);
        update.set_freshness(Freshness::Stale);
        engine.publish(update)?;
        let live = start_live_index(report.repository_root, report.syntax_index, engine.clone())?;
        engine.install_diff_provider(Arc::new(chakra_git::GitWorkspaceDiff))?;
        Ok(Self {
            repository,
            engine,
            live,
        })
    }

    /// Repository root of the temporary worktree.
    pub fn root(&self) -> &Path {
        self.repository.path()
    }

    /// Runs `git` in the worktree and returns trimmed stdout.
    pub fn git(&self, args: &[&str]) -> Check<String> {
        git(self.root(), args)
    }

    /// Reads a worktree file relative to the root.
    pub fn read(&self, relative: &str) -> Check<String> {
        Ok(fs::read_to_string(self.root().join(relative))?)
    }

    /// Writes a worktree file, creating parent directories.
    pub fn write(&self, relative: &str, contents: &str) -> Check<()> {
        let path = self.root().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    /// Appends to an existing worktree file.
    pub fn append(&self, relative: &str, contents: &str) -> Check<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.root().join(relative))?;
        file.write_all(contents.as_bytes())?;
        Ok(())
    }

    /// Renames a worktree path (also used for atomic-save emulation).
    pub fn rename(&self, from: &str, to: &str) -> Check<()> {
        fs::rename(self.root().join(from), self.root().join(to))?;
        Ok(())
    }

    /// Deletes a worktree file.
    pub fn remove(&self, relative: &str) -> Check<()> {
        fs::remove_file(self.root().join(relative))?;
        Ok(())
    }

    /// Stops the watcher and worker before the worktree is deleted.
    pub fn shutdown(self) -> Check<()> {
        let Self { live, .. } = self;
        live.shutdown()?;
        Ok(())
    }
}

/// Runs `body` against a fresh live workspace and always shuts the live
/// index down, even when the body fails.
pub fn with_live<T>(fixture_dir: &Path, body: impl FnOnce(&LiveFixture) -> Check<T>) -> Check<T> {
    let fixture = LiveFixture::start(fixture_dir)?;
    let result = body(&fixture);
    let shutdown = fixture.shutdown();
    match (result, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn copy_fixture_tree(source: &Path, target: &Path) -> Check<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| failure("fixture entry name is not UTF-8"))?;
        if name == "manifest.json" || name == "target" || name == ".git" {
            continue;
        }
        let destination = target.join(name);
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_fixture_tree(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Check<String> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Commits the whole worktree with a fixed identity and returns the commit.
pub fn commit_all(fixture: &LiveFixture, message: &str) -> Check<String> {
    fixture.git(&["add", "-A"])?;
    fixture.git(&[
        "-c",
        "user.email=conformance@example.invalid",
        "-c",
        "user.name=Chakra Conformance",
        "commit",
        "--quiet",
        "-m",
        message,
    ])?;
    fixture.git(&["rev-parse", "HEAD"])
}
