//! APFS directory snapshots for new linked worktrees.
//!
//! This keeps Git worktree metadata and Worktrunk's lifecycle intact. Only the
//! working files come from an existing worktree; Git still owns the branch and index.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use worktrunk::git::{CommandError, Repository};
use worktrunk::path::format_path_for_display;
use worktrunk::shell_exec::Cmd;

#[cfg(target_os = "macos")]
pub(super) struct PreparedSnapshot {
    temporary: tempfile::TempDir,
    source: PathBuf,
    target_commit: String,
}

/// Stage a copy of an existing worktree before Git creates the new worktree.
/// An unavailable copy is an optimization miss; the caller uses Git checkout.
#[cfg(target_os = "macos")]
pub(super) fn prepare(
    repo: &Repository,
    target_ref: &str,
    destination: &Path,
) -> Option<PreparedSnapshot> {
    let target_commit = resolve_commit(repo, target_ref).ok()?;
    let mut candidates: Vec<_> = repo
        .list_worktrees()
        .ok()?
        .iter()
        .filter(|worktree| !worktree.is_prunable() && has_dependency_cache(&worktree.path))
        .collect();
    // Linked worktrees have a small .git pointer. The primary checkout can
    // carry a large Git database that would be copied only to be removed.
    candidates.sort_by_key(|worktree| {
        (
            worktree.head != target_commit,
            !worktree.path.join(".git").is_file(),
        )
    });

    for candidate in candidates {
        match prepare_source(repo, &candidate.path, &target_commit, destination) {
            Ok(snapshot) => return Some(snapshot),
            Err(error) => {
                log::debug!("Cannot snapshot {}: {error:#}", candidate.path.display());
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn has_dependency_cache(source: &Path) -> bool {
    ["node_modules", "target", ".venv"].into_iter().any(|name| {
        fs::symlink_metadata(source.join(name)).is_ok_and(|metadata| metadata.file_type().is_dir())
    })
}

#[cfg(target_os = "macos")]
fn resolve_commit(repo: &Repository, target_ref: &str) -> anyhow::Result<String> {
    let commit = format!("{target_ref}^{{commit}}");
    repo.run_command(&["rev-parse", "--verify", "--end-of-options", &commit])
        .map(|output| output.trim().to_owned())
}

#[cfg(target_os = "macos")]
fn prepare_source(
    repo: &Repository,
    source: &Path,
    target_commit: &str,
    destination: &Path,
) -> anyhow::Result<PreparedSnapshot> {
    let source = dunce::canonicalize(source)
        .with_context(|| format!("Cannot read snapshot source {}", source.display()))?;
    let git_metadata = fs::symlink_metadata(source.join(".git"))?;
    if !git_metadata.file_type().is_dir() && !git_metadata.file_type().is_file() {
        bail!("Snapshot source has no Git metadata: {}", source.display());
    }
    let source_root = PathBuf::from(git_at(&source, &["rev-parse", "--show-toplevel"])?.trim());
    let source_root = dunce::canonicalize(source_root)?;
    if source_root != source {
        bail!(
            "Snapshot source must name the checkout root: {}",
            source.display()
        );
    }
    let source_commit = git_at(&source, &["rev-parse", "HEAD"])?;
    let source_commit = source_commit.trim();
    ensure_clean(&source)?;
    let tracked = git_at(&source, &["ls-files", "--stage", "-z"])?;
    if tracked
        .split('\0')
        .any(|entry| entry.starts_with("160000 "))
    {
        bail!("Snapshot source has submodules: {}", source.display());
    }

    let destination_resolved = worktrunk::path::canonicalize_with_parents(destination);
    if destination_resolved.starts_with(&source) || source.starts_with(&destination_resolved) {
        bail!("Snapshot source and new worktree paths must not contain each other");
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
    // The new linked worktree supplies its own .git pointer at installation.
    if git_metadata.file_type().is_dir() {
        fs::remove_dir_all(staged.join(".git"))?;
    } else {
        fs::remove_file(staged.join(".git"))?;
    }
    scrub_auto_copy(repo, &staged, temporary.path(), source_commit)?;
    ensure_clean(&source)?;
    let after = git_at(&source, &["rev-parse", "HEAD"])?;
    if after.trim() != source_commit {
        bail!(
            "Snapshot source changed while it was copied: {}",
            source.display()
        );
    }
    if source_commit != target_commit {
        reconcile_to_target(
            repo,
            &staged,
            temporary.path(),
            source_commit,
            target_commit,
        )?;
    }
    ensure_auto_copy_clean(repo, &staged, temporary.path(), target_commit)?;
    Ok(PreparedSnapshot {
        temporary,
        source,
        target_commit: target_commit.to_owned(),
    })
}

#[cfg(target_os = "macos")]
fn scrub_auto_copy(
    repo: &Repository,
    staged: &Path,
    temporary: &Path,
    source_commit: &str,
) -> anyhow::Result<()> {
    let index = temporary.join("snapshot-index");
    staged_git(repo, staged, &index, &["read-tree", source_commit])?;
    // Only dependency/build caches cross into a new branch. In particular,
    // ignored credentials and unrelated untracked files stay in the source.
    staged_git(
        repo,
        staged,
        &index,
        &[
            "clean",
            "-ffdx",
            "-e",
            "node_modules/",
            "-e",
            "target/",
            "-e",
            ".venv/",
        ],
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_auto_copy_clean(
    repo: &Repository,
    staged: &Path,
    temporary: &Path,
    target_commit: &str,
) -> anyhow::Result<()> {
    let index = temporary.join("snapshot-index");
    // The shared Git directory's HEAD can name a different branch. Compare
    // against the requested commit, not that checkout's HEAD.
    staged_git(
        repo,
        staged,
        &index,
        &["diff", "--quiet", target_commit, "--"],
    )?;
    let untracked = staged_git(
        repo,
        staged,
        &index,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if !untracked.stdout.is_empty() {
        bail!("Automatic snapshot has files outside the selected commit");
    }
    Ok(())
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
    staged_git(repo, staged, &index, &["read-tree", source_commit])?;
    staged_git(repo, staged, &index, &["update-index", "--refresh"])?;
    staged_git(
        repo,
        staged,
        &index,
        &["read-tree", "-m", "-u", source_commit, target_commit],
    )
    .context("Cannot update snapshot files to the selected base commit")?;
    staged_git(repo, staged, &index, &["diff-files", "--quiet"])
        .context("Snapshot files differ from the selected base commit")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn staged_git(
    repo: &Repository,
    staged: &Path,
    index: &Path,
    args: &[&str],
) -> anyhow::Result<std::process::Output> {
    let output = Cmd::new("git")
        .args(args.iter().copied())
        .current_dir(staged)
        .scrub_git_discovery_env()
        .env("GIT_DIR", repo.git_common_dir())
        .env("GIT_WORK_TREE", staged)
        .env("GIT_INDEX_FILE", index)
        .run()?;
    if !output.status.success() {
        return Err(CommandError::from_failed_output("git", args, &output).into());
    }
    Ok(output)
}

#[cfg(target_os = "macos")]
fn ensure_clean(source: &Path) -> anyhow::Result<()> {
    let status = git_at(source, &["status", "--porcelain", "--untracked-files=no"])?;
    if !status.trim().is_empty() {
        bail!("Snapshot source has tracked changes: {}", source.display());
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
    pub(super) fn install(mut self, repo: &Repository, destination: &Path) -> anyhow::Result<()> {
        let staged = self.temporary.path().join("tree");
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
        replace_worktree(&mut self.temporary, destination)?;
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

/// Replace a newly registered worktree without recursively deleting its old
/// directory: another process can add files after the caller checked it.
#[cfg(target_os = "macos")]
fn replace_worktree(temporary: &mut tempfile::TempDir, destination: &Path) -> anyhow::Result<()> {
    let staged = temporary.path().join("tree");
    let original = temporary.path().join("empty-worktree");
    // From the rename until rollback or rmdir succeeds, this directory can
    // contain user files. Every early return must leave those files on disk.
    temporary.disable_cleanup(true);
    if let Err(error) = fs::rename(destination, &original) {
        temporary.disable_cleanup(false);
        return Err(error).context("Cannot move new worktree for APFS snapshot installation");
    }
    if let Err(error) = renamore::rename_exclusive(&staged, destination) {
        if let Err(restore_error) = renamore::rename_exclusive(&original, destination) {
            bail!(
                "Cannot install APFS snapshot ({error}); cannot restore worktree ({restore_error}); original worktree kept @ {}",
                format_path_for_display(&original)
            );
        }
        temporary.disable_cleanup(false);
        return Err(error).context("Cannot install APFS snapshot");
    }
    if !fs::symlink_metadata(&original)?.file_type().is_dir() {
        bail!(
            "APFS snapshot installed, but the original worktree path changed; inspect it @ {}",
            format_path_for_display(&original)
        );
    }
    // The installed copy now holds the Git pointer. rmdir refuses any files
    // that arrived in the old directory, including through an open handle.
    fs::remove_file(original.join(".git"))
        .and_then(|()| fs::remove_dir(&original))
        .with_context(|| {
            format!(
                "APFS snapshot installed, but the original worktree has remaining files; inspect them @ {}",
                format_path_for_display(&original)
            )
        })?;
    temporary.disable_cleanup(false);
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn replacement_keeps_files_created_after_validation() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("worktree");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join(".git"), "gitdir: metadata\n").unwrap();
        // Model a writer arriving after install's only-.git check.
        fs::write(destination.join("work.txt"), "concurrent work\n").unwrap();
        let mut temporary = tempfile::tempdir_in(parent.path()).unwrap();
        let recovery = temporary.path().join("empty-worktree");
        let staged = temporary.path().join("tree");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join(".git"), "gitdir: metadata\n").unwrap();

        let error = replace_worktree(&mut temporary, &destination).unwrap_err();
        drop(temporary);

        assert!(error.to_string().contains("remaining files"));
        assert!(
            error
                .to_string()
                .contains(&format_path_for_display(&recovery))
        );
        assert_eq!(
            fs::read_to_string(recovery.join("work.txt")).unwrap(),
            "concurrent work\n"
        );
        assert!(destination.join(".git").is_file());
    }

    #[test]
    fn failed_replacement_restores_files_created_after_validation() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("worktree");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join(".git"), "gitdir: metadata\n").unwrap();
        fs::write(destination.join("work.txt"), "concurrent work\n").unwrap();
        let mut temporary = tempfile::tempdir_in(parent.path()).unwrap();
        let temporary_path = temporary.path().to_path_buf();
        // No staged tree: installation fails after moving the original.
        assert!(replace_worktree(&mut temporary, &destination).is_err());
        drop(temporary);

        assert_eq!(
            fs::read_to_string(destination.join("work.txt")).unwrap(),
            "concurrent work\n"
        );
        assert!(destination.join(".git").is_file());
        assert!(!temporary_path.exists());
    }

    #[test]
    fn replacement_does_not_follow_a_replaced_worktree_symlink() {
        let parent = tempfile::tempdir().unwrap();
        let other = parent.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join(".git"), "another repository\n").unwrap();
        let destination = parent.path().join("worktree");
        std::os::unix::fs::symlink(&other, &destination).unwrap();
        let mut temporary = tempfile::tempdir_in(parent.path()).unwrap();
        fs::create_dir(temporary.path().join("tree")).unwrap();

        assert!(replace_worktree(&mut temporary, &destination).is_err());
        drop(temporary);

        assert_eq!(
            fs::read_to_string(other.join(".git")).unwrap(),
            "another repository\n"
        );
    }
}
