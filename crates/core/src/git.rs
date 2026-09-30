use crate::{Error, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    PlainDirectory,
    Repository,
}

impl Source {
    pub(crate) fn is_repository(self) -> bool {
        matches!(self, Self::Repository)
    }
}

pub(crate) fn check_source(path: &Path) -> Result<Source> {
    let git = path.join(".git");
    let metadata = match fs::symlink_metadata(&git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Source::PlainDirectory);
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Err(Error::UnsafeGit(
            "linked Git worktree sources are not supported".into(),
        ));
    }

    for state in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
        "HEAD.lock",
    ] {
        if git.join(state).exists() {
            return Err(Error::UnsafeGit(format!("Git state in progress: {state}")));
        }
    }
    Ok(Source::Repository)
}

pub(crate) fn hide_marker(path: &Path) -> Result<()> {
    let info = path.join(".git").join("info");
    fs::create_dir_all(&info)?;
    let exclude = info.join("exclude");
    let existing = match fs::read_to_string(&exclude) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    // `/.rift` anchors the root marker; `/.rift.tmp` hides the temp file an
    // interrupted atomic marker write can leave behind.
    if existing
        .lines()
        .any(|line| line.trim_end_matches(' ') == "/.rift")
    {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(
        exclude,
        format!("{existing}{separator}/.rift\n/.rift.tmp\n"),
    )?;
    Ok(())
}

pub(crate) fn detach_destination(path: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    // Avoid process startup when libgit2 understands the repository;
    // the Git CLI remains the authority for layouts it cannot resolve.
    if let Some(commit) = resolve_head_commit(path) {
        fs::write(path.join(".git").join("HEAD"), format!("{commit}\n"))?;
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    fs::write(path.join(".git").join("HEAD"), format!("{commit}\n"))?;
    Ok(())
}

/// Clears the read-only bit on files under `.git`. Windows refuses to modify
/// read-only files, and a plain copy preserves the attributes Git sets on its
/// objects, which would block the post-copy fixup and later `git` commands.
#[cfg(windows)]
pub(crate) fn make_writable(path: &Path) -> Result<()> {
    let git = path.join(".git");
    if !git.is_dir() {
        return Ok(());
    }
    // Objects stay read-only: they are immutable and never modified in place,
    // so keeping Git's read-only convention also keeps `diff` honest.
    let objects = git.join("objects");
    for entry in walkdir::WalkDir::new(&git).min_depth(1) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.path().starts_with(&objects) {
            continue;
        }
        let mut permissions = entry.metadata()?.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(entry.path(), permissions)?;
        }
    }
    Ok(())
}

/// The commit `.git/HEAD` points at — the hash for a detached HEAD, or the
/// ref it names resolved through loose refs or `packed-refs`. `None` when
/// HEAD cannot be resolved (unborn branch, no `.git`, odd layout). Recorded
/// in the base manifest as provenance; it never participates in merges.
pub(crate) fn head_commit(workspace: &Path) -> Option<String> {
    let head = fs::read_to_string(workspace.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref: ") else {
        return Some(head.to_owned());
    };
    let reference = reference.trim();
    if let Ok(hash) = fs::read_to_string(workspace.join(".git").join(reference)) {
        return Some(hash.trim().to_owned());
    }
    fs::read_to_string(workspace.join(".git").join("packed-refs"))
        .ok()?
        .lines()
        .find(|line| line.ends_with(&format!(" {reference}")))
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_owned)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_head_commit(path: &Path) -> Option<git2::Oid> {
    let repository = git2::Repository::open(path).ok()?;
    repository
        .head()
        .ok()?
        .peel_to_commit()
        .ok()
        .map(|commit| commit.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn linked_worktree_marker_is_rejected() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".git"), "gitdir: elsewhere").unwrap();

        assert!(matches!(
            check_source(temp.path()),
            Err(Error::UnsafeGit(_))
        ));
    }

    #[test]
    fn check_source_distinguishes_plain_and_git_directories() {
        let plain = TempDir::new().unwrap();
        assert_eq!(check_source(plain.path()).unwrap(), Source::PlainDirectory);

        let git = TempDir::new().unwrap();
        fs::create_dir(git.path().join(".git")).unwrap();
        assert_eq!(check_source(git.path()).unwrap(), Source::Repository);
    }

    #[test]
    fn hide_marker_creates_and_appends_exclude_cleanly() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();

        hide_marker(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
            "/.rift\n/.rift.tmp\n"
        );
        fs::write(temp.path().join(".git/info/exclude"), "existing").unwrap();
        hide_marker(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
            "existing\n/.rift\n/.rift.tmp\n"
        );
        hide_marker(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
            "existing\n/.rift\n/.rift.tmp\n"
        );
        fs::write(temp.path().join(".git/info/exclude"), " /.rift\n").unwrap();
        hide_marker(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
            " /.rift\n/.rift\n/.rift.tmp\n"
        );
    }

    #[test]
    fn detach_does_nothing_for_a_repository_without_a_commit() {
        let temp = TempDir::new().unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .arg("init")
                .status()
                .unwrap()
                .success()
        );
        let head = fs::read_to_string(temp.path().join(".git/HEAD")).unwrap();

        detach_destination(temp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
            head
        );
    }
}
