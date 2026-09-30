//! Three-way `land`/`sync` semantics: independent changes merge, divergent
//! changes report conflicts, excluded paths stay invisible, and `.git` is
//! never replayed file-by-file.

use rift::{Create, Error, LandOptions, Manager, OnConflict};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn manager(temp: &TempDir) -> Manager {
    Manager::open(temp.path().join("registry.sqlite")).unwrap()
}

fn source(temp: &TempDir) -> PathBuf {
    let source = temp.path().join("app");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file.txt"), "hello").unwrap();
    fs::write(source.join("other.txt"), "other").unwrap();
    fs::canonicalize(source).unwrap()
}

fn git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        path.display()
    );
}

fn git_stdout(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn land_applies_only_fork_changes_and_keeps_parent_edits() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // Each side edits a different file after the copy.
    fs::write(child.join("file.txt"), "from rift").unwrap();
    fs::write(source.join("other.txt"), "from parent").unwrap();
    let outcome = manager.land(&child).unwrap();

    assert!(outcome.conflicts.is_empty());
    assert_eq!(outcome.applied.entries.len(), 1);
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "from rift"
    );
    assert_eq!(
        fs::read_to_string(source.join("other.txt")).unwrap(),
        "from parent"
    );
}

#[test]
fn land_reports_conflicts_without_applying_them() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    fs::write(child.join("file.txt"), "rift version").unwrap();
    fs::write(source.join("file.txt"), "parent version").unwrap();
    // An unaffected path still lands.
    fs::write(child.join("clean.txt"), "clean").unwrap();
    let outcome = manager.land(&child).unwrap();

    assert_eq!(outcome.conflicts.len(), 1);
    assert_eq!(outcome.conflicts[0].path, Path::new("file.txt"));
    assert_eq!(outcome.applied.entries.len(), 1);
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "parent version"
    );
    assert_eq!(
        fs::read_to_string(source.join("clean.txt")).unwrap(),
        "clean"
    );

    // The conflict is sticky: it is not silently reclassified as clean,
    // and the next land reports it again rather than landing the rift's
    // version over the parent's.
    let second = manager.land(&child).unwrap();
    assert_eq!(second.conflicts.len(), 1);
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "parent version"
    );
}

#[test]
fn land_abort_writes_nothing() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    fs::write(child.join("file.txt"), "rift version").unwrap();
    fs::write(child.join("clean.txt"), "clean").unwrap();
    fs::write(source.join("file.txt"), "parent version").unwrap();

    let error = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Abort),
        )
        .unwrap_err();
    assert!(matches!(error, Error::LandConflict { .. }), "{error}");
    assert!(!source.join("clean.txt").exists());
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "parent version"
    );
}

#[test]
fn land_force_takes_the_rifts_version() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    fs::write(child.join("file.txt"), "rift version").unwrap();
    fs::write(source.join("file.txt"), "parent version").unwrap();
    let outcome = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();

    assert!(outcome.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "rift version"
    );
    assert!(manager.diff(&child).unwrap().is_clean());
}

#[test]
fn delete_against_edit_conflicts_in_both_directions() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The rift deletes a file the parent edited.
    fs::remove_file(child.join("file.txt")).unwrap();
    fs::write(source.join("file.txt"), "parent edit").unwrap();
    // The rift edits a file the parent deleted.
    fs::write(child.join("other.txt"), "rift edit").unwrap();
    fs::remove_file(source.join("other.txt")).unwrap();

    let outcome = manager.land(&child).unwrap();

    assert_eq!(outcome.conflicts.len(), 2);
    // Neither conflicting path was applied.
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "parent edit"
    );
    assert!(!source.join("other.txt").exists());
    assert_eq!(
        fs::read_to_string(child.join("other.txt")).unwrap(),
        "rift edit"
    );
}

#[test]
fn sync_never_overwrites_rift_changes() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    fs::write(child.join("file.txt"), "rift version").unwrap();
    fs::write(source.join("file.txt"), "parent version").unwrap();
    fs::write(source.join("upstream.txt"), "new upstream").unwrap();
    let outcome = manager.sync(&child).unwrap();

    // The parent's clean additions sync in; the divergent edit conflicts.
    assert_eq!(outcome.conflicts.len(), 1);
    assert_eq!(outcome.conflicts[0].path, Path::new("file.txt"));
    assert_eq!(
        fs::read_to_string(child.join("file.txt")).unwrap(),
        "rift version"
    );
    assert_eq!(
        fs::read_to_string(child.join("upstream.txt")).unwrap(),
        "new upstream"
    );
}

#[test]
fn base_manifest_survives_manager_reopen() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let child;
    {
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        child = manager.create(Create::new(&source)).unwrap();
    }
    // A fresh process view must load the recorded base.
    let mut manager = manager(&temp);

    fs::write(child.join("file.txt"), "from rift").unwrap();
    fs::write(source.join("other.txt"), "from parent").unwrap();
    let outcome = manager.land(&child).unwrap();

    assert!(outcome.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "from rift"
    );
    assert_eq!(
        fs::read_to_string(source.join("other.txt")).unwrap(),
        "from parent"
    );
}

#[test]
fn land_and_sync_in_a_git_repository_require_files_only() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    git(&source, &["init"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["add", "file.txt"]);
    git(&source, &["commit", "-m", "initial"]);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    fs::write(child.join("file.txt"), "from rift").unwrap();
    assert!(matches!(manager.land(&child), Err(Error::UseGit(_))));
    assert!(matches!(manager.sync(&child), Err(Error::UseGit(_))));

    // filesOnly merges the working tree without touching `.git`.
    let head = git_stdout(&source, &["rev-parse", "HEAD"]);
    let outcome = manager
        .land_with_options(&child, LandOptions::default().files_only(true))
        .unwrap();
    assert!(outcome.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "from rift"
    );
    assert_eq!(git_stdout(&source, &["rev-parse", "HEAD"]), head);
}

#[test]
fn same_size_same_mtime_content_change_lands() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // Rewrite with identical length and the copied mtime: only the content
    // hash can see this change.
    let child_file = child.join("file.txt");
    let mtime =
        filetime::FileTime::from_last_modification_time(&fs::metadata(&child_file).unwrap());
    fs::write(&child_file, "xxxxx").unwrap();
    filetime::set_file_times(&child_file, mtime, mtime).unwrap();

    let outcome = manager.land(&child).unwrap();
    assert_eq!(outcome.applied.entries.len(), 1);
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "xxxxx"
    );
}

#[test]
fn excluded_paths_are_invisible_to_diff_and_land() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // A fresh filtered rift diffs clean, and the parent's regenerable
    // folders never appear as removals.
    assert!(manager.diff(&child).unwrap().is_clean());

    // The rift installs its own dependencies; they must never land.
    fs::create_dir_all(child.join("node_modules/left-pad")).unwrap();
    fs::write(child.join("node_modules/left-pad/index.js"), "pad").unwrap();
    let outcome = manager.land(&child).unwrap();
    assert!(outcome.conflicts.is_empty());
    assert!(outcome.applied.is_clean());
    assert!(!source.join("node_modules/left-pad").exists());
    assert_eq!(
        fs::read_to_string(source.join("node_modules/pkg/index.js")).unwrap(),
        "module"
    );
    assert!(manager.diff(&child).unwrap().is_clean());
}

#[test]
fn unicode_names_empty_directories_and_binary_files_land() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    fs::create_dir_all(child.join("ünïcode/空")).unwrap();
    fs::write(child.join("ünïcode/空/nöte.bin"), [0_u8, 159, 146, 150]).unwrap();
    fs::create_dir_all(child.join("empty")).unwrap();
    fs::remove_file(child.join("file.txt")).unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(outcome.conflicts.is_empty());
    assert_eq!(
        fs::read(source.join("ünïcode/空/nöte.bin")).unwrap(),
        [0_u8, 159, 146, 150]
    );
    assert!(source.join("empty").is_dir());
    assert!(!source.join("file.txt").exists());
}

#[test]
fn land_conflicts_instead_of_writing_into_a_deleted_directory() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/old.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The parent deletes the whole directory; the fork adds a file inside
    // it. That is a delete-vs-modify conflict, not a clean add into a
    // missing parent.
    fs::remove_dir_all(source.join("d")).unwrap();
    fs::write(child.join("d/new.txt"), "new").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("d/new.txt")),
        "{:?}",
        outcome.conflicts
    );
    assert!(!source.join("d").exists());

    // A retry reports the same conflict instead of failing identically.
    let retry = manager.land(&child).unwrap();
    assert!(
        retry
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("d/new.txt"))
    );
}

#[test]
fn sync_conflicts_instead_of_writing_into_a_deleted_directory() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/old.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The rift deletes the directory; the parent adds a file inside it.
    fs::remove_dir_all(child.join("d")).unwrap();
    fs::write(source.join("d/new.txt"), "new").unwrap();

    let outcome = manager.sync(&child).unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("d/new.txt")),
        "{:?}",
        outcome.conflicts
    );
    assert!(!child.join("d").exists());
    assert_eq!(fs::read_to_string(source.join("d/new.txt")).unwrap(), "new");
}

#[test]
fn land_conflicts_when_the_parent_turned_the_directory_into_a_file() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/old.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The parent replaces `d` with a plain file; the fork adds inside it.
    fs::remove_dir_all(source.join("d")).unwrap();
    fs::write(source.join("d"), "a file").unwrap();
    fs::write(child.join("d/new.txt"), "new").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("d/new.txt")),
        "{:?}",
        outcome.conflicts
    );
    assert_eq!(fs::read_to_string(source.join("d")).unwrap(), "a file");
}

#[test]
fn land_still_applies_into_a_directory_with_a_mode_change() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("e")).unwrap();
    fs::write(source.join("e/old.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // A mode-only change to the container is not a broken chain: the
    // incoming add lands in the real directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(source.join("e"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(child.join("e/new.txt"), "new").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    assert_eq!(fs::read_to_string(source.join("e/new.txt")).unwrap(), "new");
}

#[cfg(unix)]
#[test]
fn land_never_writes_through_a_symlinked_directory() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let victim = temp.path().join("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("sentinel.txt"), "untouched").unwrap();
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/old.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The parent replaced `d` with a symlink outside the workspace; the
    // fork added a file under `d`. The write must not follow the link.
    fs::remove_dir_all(source.join("d")).unwrap();
    std::os::unix::fs::symlink(&victim, source.join("d")).unwrap();
    fs::write(child.join("d/evil.txt"), "pwned").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("d/evil.txt")),
        "{:?}",
        outcome.conflicts
    );
    assert!(!victim.join("evil.txt").exists());
    assert_eq!(
        fs::read_to_string(victim.join("sentinel.txt")).unwrap(),
        "untouched"
    );
    assert_eq!(fs::read_link(source.join("d")).unwrap(), victim.as_path());

    // Force resolves the conflict by restoring a real directory — it must
    // not write through the link either.
    let forced = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    assert!(!victim.join("evil.txt").exists());
    assert!(source.join("d").is_dir());
    assert!(!fs::symlink_metadata(source.join("d")).unwrap().is_symlink());
    assert_eq!(
        fs::read_to_string(source.join("d/evil.txt")).unwrap(),
        "pwned"
    );
}

#[test]
fn inflight_copy_temps_are_invisible_and_never_left_behind() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // Crash debris from an earlier apply: a `.rift.tmp.*` sibling must be
    // invisible to diffs and merges on every side.
    fs::write(child.join(".rift.tmp.123"), "partial").unwrap();
    fs::write(child.join("d/.rift.tmp.7"), "partial").unwrap();
    fs::write(source.join(".rift.tmp.9"), "partial").unwrap();
    fs::write(child.join("file.txt"), "edited").unwrap();
    fs::write(child.join("d/new.txt"), "new").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "edited"
    );
    assert_eq!(fs::read_to_string(source.join("d/new.txt")).unwrap(), "new");

    // No temp file landed, and apply left none of its own behind — only
    // the deliberately planted pre-existing leftover may remain.
    for entry in walkdir::WalkDir::new(&source) {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy();
        assert!(
            name == ".rift.tmp.9" || !name.contains(".rift.tmp"),
            "temp leaked: {}",
            entry.path().display()
        );
    }
    // The parent-side leftover stays put but stays invisible.
    assert_eq!(
        fs::read_to_string(source.join(".rift.tmp.9")).unwrap(),
        "partial"
    );
    assert!(manager.diff(&child).unwrap().is_clean());
}

#[test]
fn case_folded_name_collisions_report_a_conflict() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // On a case-insensitive volume the two names are one file: landing
    // must report it instead of silently overwriting the parent's file.
    fs::write(source.join("report.txt"), "parent-version").unwrap();
    fs::write(child.join("Report.txt"), "rift-version").unwrap();

    let outcome = manager.land(&child).unwrap();
    if source.join(".RIFT").exists() {
        assert!(
            outcome
                .conflicts
                .iter()
                .any(|conflict| conflict.path == Path::new("Report.txt")),
            "{:?}",
            outcome.conflicts
        );
        assert_eq!(
            fs::read_to_string(source.join("report.txt")).unwrap(),
            "parent-version"
        );
    } else {
        // A case-sensitive filesystem keeps the two names distinct.
        assert!(outcome.conflicts.is_empty());
        assert_eq!(
            fs::read_to_string(source.join("report.txt")).unwrap(),
            "parent-version"
        );
        assert_eq!(
            fs::read_to_string(source.join("Report.txt")).unwrap(),
            "rift-version"
        );
    }
}

#[cfg(unix)]
#[test]
fn kind_changes_and_renames_land() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(source.join("old-name.txt"), "renamed").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // file.txt becomes a directory, other.txt becomes a symlink, and
    // old-name.txt is renamed (a delete plus an add).
    fs::remove_file(child.join("file.txt")).unwrap();
    fs::create_dir_all(child.join("file.txt/nested")).unwrap();
    fs::write(child.join("file.txt/nested/inner.txt"), "inner").unwrap();
    fs::remove_file(child.join("other.txt")).unwrap();
    std::os::unix::fs::symlink("file.txt", child.join("other.txt")).unwrap();
    fs::remove_file(child.join("old-name.txt")).unwrap();
    fs::write(child.join("new-name.txt"), "renamed").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    assert_eq!(
        fs::read_to_string(source.join("file.txt/nested/inner.txt")).unwrap(),
        "inner"
    );
    assert_eq!(
        fs::read_link(source.join("other.txt")).unwrap(),
        Path::new("file.txt")
    );
    assert!(!source.join("old-name.txt").exists());
    assert_eq!(
        fs::read_to_string(source.join("new-name.txt")).unwrap(),
        "renamed"
    );
}

#[test]
fn land_force_restores_a_container_ours_deleted() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/x.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The parent deletes the whole directory; the rift edits a file inside
    // it — a natural conflict under a missing container. Force must
    // restore the incoming side's chain instead of failing on ENOENT.
    fs::remove_dir_all(source.join("d")).unwrap();
    fs::write(child.join("d/x.txt"), "changed").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("d/x.txt")),
        "{:?}",
        outcome.conflicts
    );
    assert!(!source.join("d").exists());

    let forced = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(source.join("d/x.txt")).unwrap(),
        "changed"
    );
    assert!(manager.diff(&child).unwrap().is_clean());
}

#[test]
fn land_force_restores_a_container_ours_turned_into_a_file() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/x.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The parent replaced `d` with a plain file; the rift edits inside it.
    fs::remove_dir_all(source.join("d")).unwrap();
    fs::write(source.join("d"), "a file").unwrap();
    fs::write(child.join("d/x.txt"), "changed").unwrap();

    let forced = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    assert!(source.join("d").is_dir());
    assert_eq!(
        fs::read_to_string(source.join("d/x.txt")).unwrap(),
        "changed"
    );
}

#[test]
fn sync_force_restores_a_container_the_rift_deleted() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join("d")).unwrap();
    fs::write(source.join("d/x.txt"), "old").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The rift deletes the directory; the parent edits inside it. Forcing
    // a sync must rebuild the rift-side chain before writing the conflict.
    fs::remove_dir_all(child.join("d")).unwrap();
    fs::write(source.join("d/x.txt"), "changed").unwrap();

    let forced = manager
        .sync_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(child.join("d/x.txt")).unwrap(),
        "changed"
    );
}

/// Whether the volume holding a workspace folds letter case: `.RIFT` only
/// resolves to the `.rift` marker when it does. Mirrors the merge's own
/// probe so expectations can branch per filesystem.
fn folds_case(workspace: &Path) -> bool {
    workspace.join(".RIFT").exists()
}

/// On a case-folding volume a theirs-side case-only rename is one
/// filesystem slot: the `Added` half collides with the ours-side twin and
/// the `Removed` half must be held back with it — never applied alone.
/// Under force the pair applies together and the slot takes the incoming
/// side's name.
fn case_only_rename_keeps_the_file(old_name: &str, new_name: &str) {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(source.join(old_name), "v1").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();
    fs::rename(child.join(old_name), child.join(new_name)).unwrap();

    let outcome = manager.land(&child).unwrap();
    if !folds_case(&source) {
        // Case-sensitive volumes hold both names: the rename is a plain
        // remove plus add.
        assert!(outcome.conflicts.is_empty());
        assert!(!source.join(old_name).exists());
        assert_eq!(fs::read_to_string(source.join(new_name)).unwrap(), "v1");
        return;
    }

    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new(new_name)),
        "{:?}",
        outcome.conflicts
    );
    // The shared slot still holds ours' entry: nothing was deleted.
    assert_eq!(fs::read_to_string(source.join(old_name)).unwrap(), "v1");

    let forced = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    let names = fs::read_dir(&source)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(names.iter().any(|name| name == new_name), "{names:?}");
    assert!(!names.iter().any(|name| name == old_name), "{names:?}");
    assert_eq!(fs::read_to_string(source.join(new_name)).unwrap(), "v1");
}

#[test]
fn case_only_file_rename_to_uppercase_keeps_the_file() {
    case_only_rename_keeps_the_file("report.txt", "Report.txt");
}

#[test]
fn case_only_file_rename_to_lowercase_keeps_the_file() {
    case_only_rename_keeps_the_file("README.md", "readme.md");
}

/// The directory version of `case_only_rename_keeps_the_file`: every
/// removal inside the recased tree pairs with a same-slot write.
fn case_only_dir_rename_keeps_the_tree(old_name: &str, new_name: &str) {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir(source.join(old_name)).unwrap();
    fs::write(source.join(old_name).join("note.txt"), "v1").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();
    fs::rename(child.join(old_name), child.join(new_name)).unwrap();

    let outcome = manager.land(&child).unwrap();
    if !folds_case(&source) {
        assert!(outcome.conflicts.is_empty());
        assert_eq!(
            fs::read_to_string(source.join(new_name).join("note.txt")).unwrap(),
            "v1"
        );
        assert!(!source.join(old_name).exists());
        return;
    }

    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new(new_name)),
        "{:?}",
        outcome.conflicts
    );
    assert_eq!(
        fs::read_to_string(source.join(old_name).join("note.txt")).unwrap(),
        "v1"
    );

    let forced = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    let names = fs::read_dir(&source)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(names.iter().any(|name| name == new_name), "{names:?}");
    assert!(!names.iter().any(|name| name == old_name), "{names:?}");
    assert_eq!(
        fs::read_to_string(source.join(new_name).join("note.txt")).unwrap(),
        "v1"
    );
}

#[test]
fn case_only_dir_rename_to_lowercase_keeps_the_tree() {
    case_only_dir_rename_keeps_the_tree("Docs", "docs");
}

#[test]
fn case_only_dir_rename_to_uppercase_keeps_the_tree() {
    case_only_dir_rename_keeps_the_tree("notes", "Notes");
}

#[test]
fn case_only_rename_over_an_ours_edit_holds_and_force_absorbs_the_slot() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(source.join("report.txt"), "v1").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();
    if !folds_case(&source) {
        return;
    }

    // Ours edits the file while theirs recases it: both halves conflict.
    fs::write(source.join("report.txt"), "ours-edit").unwrap();
    fs::rename(child.join("report.txt"), child.join("Report.txt")).unwrap();
    fs::write(child.join("Report.txt"), "theirs-edit").unwrap();

    let outcome = manager.land(&child).unwrap();
    assert!(
        outcome
            .conflicts
            .iter()
            .any(|conflict| conflict.path == Path::new("Report.txt")),
        "{:?}",
        outcome.conflicts
    );
    assert_eq!(
        fs::read_to_string(source.join("report.txt")).unwrap(),
        "ours-edit"
    );

    // Force takes the incoming side: the slot ends with theirs' content
    // under theirs' name — applying the removal after the add must not
    // delete the entry just written.
    let forced = manager
        .land_with_options(
            &child,
            LandOptions::default().on_conflict(OnConflict::Force),
        )
        .unwrap();
    assert!(forced.conflicts.is_empty());
    assert_eq!(
        fs::read_to_string(source.join("Report.txt")).unwrap(),
        "theirs-edit"
    );
}
