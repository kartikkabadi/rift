use crate::test_support::linux_extents::{assert_shared_extents_when_reliable, is_btrfs_subvolume};
use crate::{Backend, CowMode, Create, CreateOptions, Error, InitOutcome, Manager};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{Builder, TempDir};

#[test]
fn production_supported_linux_filesystem_round_trip() {
    if !requires_supported_linux_filesystem_tests() {
        return;
    }
    let temp = current_filesystem_temp();
    let source = rich_git_workspace(temp.path());
    let registry = temp.path().join("registry.sqlite");
    let mut manager = Manager::open(&registry).unwrap();

    assert!(matches!(
        manager.init(&source).unwrap(),
        InitOutcome::Registered | InitOutcome::Converted
    ));
    assert_eq!(
        manager.init(&source).unwrap(),
        InitOutcome::AlreadyInitialized
    );
    assert!(source.join(".rift").exists());
    assert_btrfs_subvolume_if_required(&source);

    let child = manager
        .create(Create {
            from: source.clone(),
            name: Some("child".into()),
            into: None,
        })
        .unwrap();
    assert_btrfs_subvolume_if_required(&child);
    assert_rich_copy(&child);
    assert_detached_git_copy(&source, &child);

    let custom_parent = temp.path().join("custom-storage");
    let custom = manager
        .create(Create {
            from: source.clone(),
            name: Some("custom".into()),
            into: Some(custom_parent.clone()),
        })
        .unwrap();
    assert_eq!(custom, custom_parent.join("custom"));
    assert_btrfs_subvolume_if_required(&custom);
    assert_rich_copy(&custom);
    assert_native_cow_copy(
        &source.join("nested/deeper/leaf.txt"),
        &child.join("nested/deeper/leaf.txt"),
    );
    assert_native_cow_copy(&source.join("untracked.txt"), &custom.join("untracked.txt"));

    let grandchild = manager
        .create(Create {
            from: child.clone(),
            name: Some("grandchild".into()),
            into: None,
        })
        .unwrap();
    assert_eq!(
        manager.ancestors(&grandchild).unwrap(),
        vec![child.clone(), source.clone()]
    );
    assert_contains_exactly(
        manager.list(&source).unwrap(),
        &[child.clone(), custom.clone()],
    );

    assert_different_filesystem_storage_falls_back(&mut manager, &source);

    manager.remove(&grandchild).unwrap();
    assert!(!grandchild.exists());
    assert!(!manager.gc().unwrap().is_empty());
}

#[test]
fn production_unsupported_linux_filesystem_falls_back_to_plain_copies() {
    if std::env::var_os("RIFT_REQUIRE_UNSUPPORTED_LINUX_TESTS").is_none() {
        return;
    }
    let temp = current_filesystem_temp();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file.txt"), "hello").unwrap();
    let mut manager = Manager::open(temp.path().join("registry.sqlite")).unwrap();

    // Strict mode keeps the old failure mode.
    assert!(matches!(
        manager.init_with_cow_mode(&source, CowMode::Require, |_| {}),
        Err(Error::CowUnavailable(_))
    ));
    assert!(!source.join(".rift").exists());
    assert!(!temp.path().join(".rifts").exists());
    assert_reflink_probe_cleaned_up(&source);
    assert_registry_empty(&manager);

    // Default init registers the workspace and reports the degraded backend.
    assert_eq!(manager.init(&source).unwrap(), InitOutcome::Degraded);
    assert!(source.join(".rift").exists());
    assert_eq!(manager.probe(&source).unwrap().backend, Backend::Portable);

    // Strict create still refuses; the default create makes a regular copy.
    assert!(matches!(
        manager.create_with_options(
            Create {
                from: source.clone(),
                name: Some("strict".into()),
                into: None,
            },
            CreateOptions::default().cow_mode(CowMode::Require),
        ),
        Err(Error::CowUnavailable(_))
    ));
    assert!(!temp.path().join(".rifts/source/strict").exists());

    let child = manager
        .create(Create {
            from: source.clone(),
            name: Some("child".into()),
            into: None,
        })
        .unwrap();
    assert_eq!(child, temp.path().join(".rifts/source/child"));
    assert_eq!(fs::read_to_string(child.join("file.txt")).unwrap(), "hello");
    assert!(child.join(".rift").exists());
    assert_eq!(manager.list(&source).unwrap(), vec![child.clone()]);

    // A plain copy diverges in both directions.
    fs::write(source.join("file.txt"), "changed").unwrap();
    assert_eq!(fs::read_to_string(child.join("file.txt")).unwrap(), "hello");

    manager.remove(&child).unwrap();
    assert!(!child.exists());
}

#[test]
fn production_supported_linux_filesystem_cleans_partial_copy_failure() {
    if !requires_supported_linux_filesystem_tests() {
        return;
    }
    let temp = current_filesystem_temp();
    let source = rich_git_workspace(temp.path());
    let registry = temp.path().join("registry.sqlite");
    let mut manager = Manager::open(&registry).unwrap();
    manager.init(&source).unwrap();
    assert_only_registered_path(&manager, &source);

    let fifo = source.join("fifo");
    make_fifo(&fifo);
    let storage = temp.path().join(".rifts/source");
    let expected = storage.join("unsupported-entry");

    let error = manager
        .create(Create {
            from: source.clone(),
            name: Some("unsupported-entry".into()),
            into: None,
        })
        .unwrap_err();

    assert!(matches!(error, Error::UnsupportedEntry(path) if path == fifo));
    assert!(source.join(".rift").exists());
    assert!(!expected.exists());
    assert!(manager.list(&source).unwrap().is_empty());
    assert_only_registered_path(&manager, &source);
    assert_reflink_probe_cleaned_up(&storage);
}

fn requires_supported_linux_filesystem_tests() -> bool {
    ["RIFT_REQUIRE_BTRFS_TESTS", "RIFT_REQUIRE_REFLINK_TESTS"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some())
}

fn current_filesystem_temp() -> TempDir {
    Builder::new()
        .prefix(".rift-manager-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap()
}

fn rich_git_workspace(root: &Path) -> PathBuf {
    let source = root.join("source");
    let nested = source.join("nested");
    let deeper = nested.join("deeper");
    fs::create_dir(&source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
    fs::create_dir_all(&deeper).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();

    let file = nested.join("file.txt");
    fs::write(&file, "hello").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    set_xattr(&file, "user.rift_test", b"xattr");
    fs::hard_link(&file, nested.join("hard.txt")).unwrap();
    std::os::unix::fs::symlink("file.txt", nested.join("link.txt")).unwrap();
    fs::write(deeper.join("leaf.txt"), "leaf").unwrap();

    git(&source, &["init"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "initial"]);
    fs::write(&file, "changed").unwrap();
    git(&source, &["add", "nested/file.txt"]);
    fs::write(source.join("untracked.txt"), "new").unwrap();
    source
}

fn assert_rich_copy(path: &Path) {
    let file = path.join("nested/file.txt");
    let hard = path.join("nested/hard.txt");

    assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
    assert_eq!(
        fs::read_to_string(path.join("nested/deeper/leaf.txt")).unwrap(),
        "leaf"
    );
    assert_eq!(
        fs::read_link(path.join("nested/link.txt")).unwrap(),
        Path::new("file.txt")
    );
    assert_eq!(
        fs::metadata(&file).unwrap().ino(),
        fs::metadata(&hard).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        fs::metadata(path.join("nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(get_xattr(&file, "user.rift_test"), b"xattr");
    assert!(path.join("untracked.txt").exists());
}

fn assert_detached_git_copy(source: &Path, destination: &Path) {
    let source_commit = git_output(source, &["rev-parse", "--verify", "HEAD^{commit}"]);
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(destination)
            .args(["symbolic-ref", "-q", "HEAD"])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        fs::read_to_string(destination.join(".git/HEAD")).unwrap(),
        format!("{}\n", source_commit.trim())
    );
    assert!(
        git_output(destination, &["diff", "--cached", "--name-only"]).contains("nested/file.txt")
    );
    assert!(git_output(destination, &["status", "--porcelain", "--", ".rift"]).is_empty());
}

fn assert_different_filesystem_storage_falls_back(manager: &mut Manager, source: &Path) {
    let other = TempDir::new().unwrap();
    if same_device(source, other.path()) {
        return;
    }
    let parent = other.path().join("storage");
    assert!(matches!(
        manager.create_with_options(
            Create {
                from: source.to_path_buf(),
                name: Some("other-fs".into()),
                into: Some(parent.clone()),
            },
            CreateOptions::default().cow_mode(CowMode::Require),
        ),
        Err(Error::CowUnavailable(_))
    ));
    assert!(!parent.join("other-fs").exists());

    let created = manager
        .create(Create {
            from: source.to_path_buf(),
            name: Some("other-fs".into()),
            into: Some(parent.clone()),
        })
        .unwrap();
    assert_eq!(created, parent.join("other-fs"));
    assert_eq!(
        fs::read_to_string(created.join("nested/file.txt")).unwrap(),
        "changed"
    );
}

fn assert_contains_exactly(mut actual: Vec<PathBuf>, expected: &[PathBuf]) {
    actual.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(actual, expected);
}

fn assert_reflink_probe_cleaned_up(path: &Path) {
    assert!(
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .all(|name| !name.to_string_lossy().starts_with(".rift-reflink-probe"))
    );
}

fn assert_registry_empty(manager: &Manager) {
    assert!(manager.registry.active_paths().unwrap().is_empty());
    assert!(manager.registry.trashed_paths().unwrap().is_empty());
}

fn assert_only_registered_path(manager: &Manager, path: &Path) {
    assert_eq!(
        manager
            .registry
            .active_paths()
            .unwrap()
            .into_iter()
            .map(|record| record.path)
            .collect::<Vec<_>>(),
        vec![path.to_path_buf()]
    );
    assert!(manager.registry.trashed_paths().unwrap().is_empty());
}

fn assert_btrfs_subvolume_if_required(path: &Path) {
    if std::env::var_os("RIFT_REQUIRE_BTRFS_TESTS").is_some() {
        assert!(
            is_btrfs_subvolume(path).unwrap(),
            "{} should be a btrfs subvolume",
            path.display()
        );
    }
}

fn assert_native_cow_copy(source: &Path, child: &Path) {
    if std::env::var_os("RIFT_REQUIRE_REFLINK_TESTS").is_some() {
        assert_shared_extents_when_reliable(source, child);
    }
    assert_copy_diverges_after_mutation(source, child);
}

fn assert_copy_diverges_after_mutation(source: &Path, child: &Path) {
    let original = fs::read_to_string(source).unwrap();
    assert_eq!(fs::read_to_string(child).unwrap(), original);
    fs::write(source, "parent mutation").unwrap();
    assert_eq!(fs::read_to_string(child).unwrap(), original);
    fs::write(child, "child mutation").unwrap();
    assert_eq!(fs::read_to_string(source).unwrap(), "parent mutation");
    assert_eq!(fs::read_to_string(child).unwrap(), "child mutation");
}

fn same_device(left: &Path, right: &Path) -> bool {
    fs::metadata(left).unwrap().dev() == fs::metadata(right).unwrap().dev()
}

fn make_fifo(path: &Path) {
    let path = c_path(path);
    assert_eq!(
        // SAFETY: `path` is a valid C path and the mode is a standard FIFO
        // permission bitmask for this test fixture.
        unsafe { libc::mkfifo(path.as_ptr(), 0o600) },
        0
    );
}

fn git(path: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn git_output(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    let path = c_path(path);
    let name = std::ffi::CString::new(name).unwrap();
    assert_eq!(
        // SAFETY: test inputs are valid C strings and `value` is copied by the kernel.
        unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        },
        0
    );
}

fn get_xattr(path: &Path, name: &str) -> Vec<u8> {
    let path = c_path(path);
    let name = std::ffi::CString::new(name).unwrap();
    // SAFETY: test inputs are valid C strings. A null buffer requests the value length.
    let size = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    assert!(size >= 0);
    let mut value = vec![0; size as usize];
    assert_eq!(
        // SAFETY: `value` is allocated with the exact size reported above.
        unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        },
        size
    );
    value
}

fn c_path(path: &Path) -> std::ffi::CString {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap()
}
