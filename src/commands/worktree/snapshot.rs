//! APFS directory snapshots for new linked worktrees.
//!
//! This keeps Git worktree metadata and Worktrunk's lifecycle intact. Only the
//! working files come from the template; Git still owns the branch and index.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use worktrunk::git::{CommandError, Repository};
use worktrunk::shell_exec::Cmd;

#[cfg(target_os = "macos")]
pub(super) struct PreparedSnapshot {
    temporary: tempfile::TempDir,
    source: PathBuf,
    target_commit: String,
}

#[cfg(target_os = "macos")]
pub(super) enum SnapshotPlan {
    Prepared(PreparedSnapshot),
    Checkout(anyhow::Error),
}

/// Prepare the selected commit before `git worktree add` creates a worktree.
#[cfg(target_os = "macos")]
pub(super) fn prepare(
    repo: &Repository,
    configured_source: &Path,
    target_ref: &str,
    destination: &Path,
) -> anyhow::Result<SnapshotPlan> {
    if !configured_source.is_absolute() {
        bail!("switch.snapshot-from must be an absolute path");
    }
    let source = dunce::canonicalize(configured_source).with_context(|| {
        format!(
            "Cannot read snapshot template {}",
            configured_source.display()
        )
    })?;
    if !fs::symlink_metadata(source.join(".git"))?
        .file_type()
        .is_dir()
    {
        bail!(
            "Snapshot template must be a standalone Git checkout: {}",
            source.display()
        );
    }
    let source_root = PathBuf::from(git_at(&source, &["rev-parse", "--show-toplevel"])?.trim());
    let source_root = dunce::canonicalize(source_root)?;
    if source_root != source {
        bail!(
            "Snapshot template must name the checkout root: {}",
            source.display()
        );
    }
    let source_commit = git_at(&source, &["rev-parse", "HEAD"])?;
    let source_commit = source_commit.trim();
    let base_commit = format!("{target_ref}^{{commit}}");
    let target_commit =
        repo.run_command(&["rev-parse", "--verify", "--end-of-options", &base_commit])?;
    let target_commit = target_commit.trim();
    ensure_clean(&source)?;
    let tracked = git_at(&source, &["ls-files", "--stage", "-z"])?;
    if tracked
        .split('\0')
        .any(|entry| entry.starts_with("160000 "))
    {
        bail!("Snapshot template has submodules: {}", source.display());
    }

    let destination_resolved = worktrunk::path::canonicalize_with_parents(destination);
    if destination_resolved.starts_with(&source) || source.starts_with(&destination_resolved) {
        bail!("Snapshot template and new worktree paths must not contain each other");
    }
    if let Err(error) =
        repo.run_command(&["cat-file", "-e", &format!("{source_commit}^{{commit}}")])
    {
        return Ok(SnapshotPlan::Checkout(
            error.context("Template commit is absent from the target repository"),
        ));
    }

    let parent = destination
        .parent()
        .context("Worktree path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    if !renamore::rename_exclusive_is_atomic(parent)? {
        bail!(
            "Snapshot path requires atomic no-overwrite rename: {}",
            parent.display()
        );
    }
    let temporary = tempfile::Builder::new()
        .prefix(".wt-snapshot-")
        .tempdir_in(parent)?;
    let staged = temporary.path().join("tree");
    reflink_copy::reflink(&source, &staged).context("APFS snapshot failed")?;
    // The cloned repository has its own Git database. The linked worktree's
    // small .git pointer replaces it at installation time.
    fs::remove_dir_all(staged.join(".git"))?;
    ensure_clean(&source)?;
    let after = git_at(&source, &["rev-parse", "HEAD"])?;
    if after.trim() != source_commit {
        bail!(
            "Snapshot template changed while it was copied: {}",
            source.display()
        );
    }
    if source_commit != target_commit
        && let Err(error) = reconcile_to_target(
            repo,
            &staged,
            temporary.path(),
            source_commit,
            target_commit,
        )
    {
        return Ok(SnapshotPlan::Checkout(error));
    }
    Ok(SnapshotPlan::Prepared(PreparedSnapshot {
        temporary,
        source,
        target_commit: target_commit.to_owned(),
    }))
}

#[cfg(target_os = "macos")]
fn reconcile_to_target(
    repo: &Repository,
    staged: &Path,
    temporary: &Path,
    source_commit: &str,
    target_commit: &str,
) -> anyhow::Result<()> {
    let index = temporary.join("snapshot-index");
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let output = Cmd::new("git")
            .args(args.iter().copied())
            .current_dir(staged)
            .scrub_git_discovery_env()
            .env("GIT_DIR", repo.git_common_dir())
            .env("GIT_WORK_TREE", staged)
            .env("GIT_INDEX_FILE", &index)
            .run()?;
        if !output.status.success() {
            return Err(CommandError::from_failed_output("git", args, &output).into());
        }
        Ok(())
    };
    run(&["read-tree", source_commit])?;
    run(&["update-index", "--refresh"])?;
    run(&["read-tree", "-m", "-u", source_commit, target_commit])
        .context("Cannot update snapshot files to the selected base commit")?;
    run(&["diff-files", "--quiet"])
        .context("Snapshot files differ from the selected base commit")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_clean(source: &Path) -> anyhow::Result<()> {
    let status = git_at(source, &["status", "--porcelain", "--untracked-files=no"])?;
    if !status.trim().is_empty() {
        bail!(
            "Snapshot template has tracked changes: {}",
            source.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn git_at(path: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Cmd::new("git")
        .args(args.iter().copied())
        .current_dir(path)
        .scrub_git_discovery_env()
        .run()
        .with_context(|| format!("Cannot run git in {}", path.display()))?;
    if !output.status.success() {
        return Err(CommandError::from_failed_output("git", args, &output).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
impl PreparedSnapshot {
    /// Replace the empty `--no-checkout` worktree with the staged tree. Both
    /// renames stay on one volume. Keep the empty directory for rollback.
    pub(super) fn install(self, repo: &Repository, destination: &Path) -> anyhow::Result<()> {
        let staged = self.temporary.path().join("tree");
        let empty = self.temporary.path().join("empty-worktree");
        let linked = repo.worktree_at(destination);
        let actual_commit = linked.run_command(&["rev-parse", "HEAD"])?;
        if actual_commit.trim() != self.target_commit {
            bail!(
                "New worktree base changed during snapshot creation; empty worktree kept: {}",
                destination.display()
            );
        }
        let mut entries = fs::read_dir(destination)?;
        let only_git = entries
            .next()
            .transpose()?
            .is_some_and(|entry| entry.file_name() == ".git")
            && entries.next().is_none();
        if !only_git || !destination.join(".git").is_file() {
            bail!(
                "New worktree is not empty; snapshot was not installed: {}",
                destination.display()
            );
        }
        let pointer = fs::read(destination.join(".git"))?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staged.join(".git"))?
            .write_all(&pointer)?;
        fs::rename(destination, &empty)?;
        if let Err(error) = renamore::rename_exclusive(&staged, destination) {
            if let Err(restore_error) = renamore::rename_exclusive(&empty, destination) {
                let recovery = self.temporary.keep().join("empty-worktree");
                bail!(
                    "Cannot install APFS snapshot ({error}); cannot restore worktree ({restore_error}); original Git pointer kept at {}",
                    recovery.display()
                );
            }
            return Err(error).context("Cannot install APFS snapshot");
        }
        // `git worktree add --no-checkout` leaves an empty index. Populate it
        // from HEAD without touching the copied working files.
        let installed = repo.worktree_at(destination);
        installed.run_command(&["read-tree", "HEAD"])?;
        let status = installed.run_command(&["status", "--porcelain", "--untracked-files=no"])?;
        if !status.trim().is_empty() {
            bail!(
                "Snapshot files differ from HEAD; worktree kept for inspection: {}",
                destination.display()
            );
        }
        log::debug!(
            "Installed APFS snapshot from {} at {}",
            self.source.display(),
            self.target_commit
        );
        Ok(())
    }
}
