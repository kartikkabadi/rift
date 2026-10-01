//! Regression tests for the four `land`/`sync` bugs reproduced on `pr1`:
//!
//! 1. A fresh default rift's `land` deleted the parent's filtered folders
//!    (`node_modules`, `dist`) because the two-way diff saw them as removals.
//! 2. `land` deleted commits the parent made after the rift was created,
//!    because `.git` was replayed file-by-file.
//! 3. `sync` deleted the rift's own commits for the same reason.
//! 4. A second rift's `land` undid an earlier rift's landed changes.
//!
//! The assertions only depend on the workspace contents afterwards, so the
//! same expectations are red on `pr1` and green after the three-way land.

use rift::{Create, Manager};
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

fn git_init(path: &Path) {
    git(path, &["init"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
}

/// Land only working-tree files (`.git` stays untouched). On `pr1` this is a
/// plain `land`; after the three-way work it is `land` with `filesOnly`,
/// because a full `land` on a repository refuses with `use_git`.
fn land_files_only(manager: &mut Manager, at: &Path) {
    manager
        .land_with_options(at, rift::LandOptions::default().files_only(true))
        .unwrap();
}

/// Sync only working-tree files (`.git` stays untouched).
fn sync_files_only(manager: &mut Manager, at: &Path) {
    manager
        .sync_with_options(at, rift::LandOptions::default().files_only(true))
        .unwrap();
}

#[test]
fn land_preserves_filtered_directories_in_parent() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();
    fs::create_dir_all(source.join("dist")).unwrap();
    fs::write(source.join("dist/bundle.js"), "bundle").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    // A default (filtered) rift does not copy regenerable folders.
    let child = manager.create(Create::new(&source)).unwrap();
    assert!(!child.join("node_modules").exists());

    fs::write(child.join("file.txt"), "edited").unwrap();
    manager.land(&child).unwrap();

    // Landing must not treat the filtered folders as deletions.
    assert_eq!(
        fs::read_to_string(source.join("node_modules/pkg/index.js")).unwrap(),
        "module"
    );
    assert_eq!(
        fs::read_to_string(source.join("dist/bundle.js")).unwrap(),
        "bundle"
    );
    assert_eq!(
        fs::read_to_string(source.join("file.txt")).unwrap(),
        "edited"
    );
}

#[test]
fn land_keeps_parent_commits_made_after_the_fork() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    git_init(&source);
    git(&source, &["add", "file.txt"]);
    git(&source, &["commit", "-m", "initial"]);
    let first = git_stdout(&source, &["rev-parse", "HEAD"]);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The parent moves on after the rift is created.
    fs::write(source.join("parent-work.txt"), "parent").unwrap();
    git(&source, &["add", "parent-work.txt"]);
    git(&source, &["commit", "-m", "parent commit"]);
    let second = git_stdout(&source, &["rev-parse", "HEAD"]);
    assert_ne!(first, second);

    fs::write(child.join("rift-work.txt"), "rift").unwrap();
    land_files_only(&mut manager, &child);

    assert_eq!(git_stdout(&source, &["rev-parse", "HEAD"]), second);
    assert_eq!(
        fs::read_to_string(source.join("rift-work.txt")).unwrap(),
        "rift"
    );
}

#[test]
fn sync_keeps_the_rifts_own_commits() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    git_init(&source);
    git(&source, &["add", "file.txt"]);
    git(&source, &["commit", "-m", "initial"]);
    let parent_head = git_stdout(&source, &["rev-parse", "HEAD"]);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(&source)).unwrap();

    // The rift makes its own commit after creation.
    fs::write(child.join("rift-work.txt"), "rift").unwrap();
    git(&child, &["add", "rift-work.txt"]);
    git(&child, &["commit", "-m", "rift commit"]);
    let rift_head = git_stdout(&child, &["rev-parse", "HEAD"]);
    assert_ne!(parent_head, rift_head);

    fs::write(source.join("parent-work.txt"), "parent").unwrap();
    sync_files_only(&mut manager, &child);

    assert_eq!(git_stdout(&child, &["rev-parse", "HEAD"]), rift_head);
    assert_eq!(
        fs::read_to_string(child.join("parent-work.txt")).unwrap(),
        "parent"
    );
}

#[test]
fn second_fork_land_does_not_undo_the_first() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager.create(Create::new(&source)).unwrap();
    let second = manager.create(Create::new(&source)).unwrap();

    // Fork A adds a file and lands it.
    fs::write(first.join("from-first.txt"), "first rift").unwrap();
    manager.land(&first).unwrap();
    assert!(source.join("from-first.txt").exists());

    // Fork B was created before A landed, so its copy has no such file.
    // Landing B's own change must not remove A's landed file.
    fs::write(second.join("from-second.txt"), "second rift").unwrap();
    manager.land(&second).unwrap();

    assert_eq!(
        fs::read_to_string(source.join("from-first.txt")).unwrap(),
        "first rift"
    );
    assert_eq!(
        fs::read_to_string(source.join("from-second.txt")).unwrap(),
        "second rift"
    );
}
