#[cfg(target_os = "linux")]
mod support;

#[cfg(target_os = "linux")]
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use support::filesystem::{
    CliFixture, assert_different_filesystems, assert_registry_empty,
    supported_linux_filesystem_tests_required, unsupported_linux_filesystem_tests_required,
};

#[cfg(target_os = "linux")]
#[test]
fn supported_filesystem_cli_round_trip() {
    if !supported_linux_filesystem_tests_required() {
        return;
    }
    let fixture = CliFixture::current_filesystem(".rift-cli-supported-");
    let source = fixture.root().join("source");
    create_workspace(&source);

    let init = fixture.success(
        fixture.root(),
        [os("init"), os(source.as_os_str()), os("--here")],
    );
    assert!(init.stdout.is_empty());
    assert!(source.join(".rift").exists());

    let child = fixture
        .success(&source, ["create", "--name", "child"])
        .single_stdout_path();
    assert_eq!(child, fixture.root().join(".rifts/source/child"));
    assert_workspace_copy(&child);

    let custom_parent = fixture.root().join("custom-storage");
    let custom = fixture
        .success(
            &source,
            [
                os("create"),
                os("--name"),
                os("custom"),
                os("--into"),
                os(custom_parent.as_os_str()),
            ],
        )
        .single_stdout_path();
    assert_eq!(custom, custom_parent.join("custom"));
    assert_workspace_copy(&custom);

    assert_paths_unordered(
        fixture.success(&source, ["list"]).stdout_paths(),
        &[child.clone(), custom.clone()],
    );
    assert_eq!(
        fixture
            .success(&source, [os("ancestors"), os(custom.as_os_str())])
            .stdout_paths(),
        vec![source.clone()]
    );

    let doctor = fixture.success(&source, ["doctor"]);
    assert!(doctor.stdout.contains("instant copies"));
    assert!(
        fixture
            .success(&source, [os("doctor"), os("--json")])
            .stdout
            .contains("backend")
    );

    let external = tempfile::TempDir::new().unwrap();
    assert_different_filesystems(&source, external.path());
    let external_parent = external.path().join("external-storage");
    let failed = fixture.failure(
        &source,
        [
            os("create"),
            os("--name"),
            os("external"),
            os("--into"),
            os(external_parent.as_os_str()),
            os("--cow-only"),
        ],
    );
    assert!(failed.stderr.contains("copy-on-write cloning unavailable"));
    assert!(!external_parent.join("external").exists());

    let fallback = fixture
        .success(
            &source,
            [
                os("create"),
                os("--name"),
                os("external"),
                os("--into"),
                os(external_parent.as_os_str()),
            ],
        )
        .single_stdout_path();
    assert_eq!(fallback, external_parent.join("external"));
    assert_workspace_copy(&fallback);

    let remove = fixture.success(&source, [os("remove"), os(child.as_os_str())]);
    assert!(remove.stdout.is_empty());
    assert!(!child.exists());
    assert_eq!(
        fixture.success(&source, ["list"]).stdout_paths(),
        vec![custom.clone(), fallback.clone()]
    );

    let removed = fixture.success(&source, ["gc"]).stdout_paths();
    assert_eq!(removed.len(), 1);
    assert!(removed[0].starts_with(fixture.root().join(".rifts/source/.trash")));
    assert!(!removed[0].exists());
    assert!(!fixture.default_database().exists());
}

#[cfg(target_os = "linux")]
#[test]
fn unsupported_filesystem_cli_falls_back_to_plain_copies() {
    if !unsupported_linux_filesystem_tests_required() {
        return;
    }
    let fixture = CliFixture::current_filesystem(".rift-cli-unsupported-");
    let source = fixture.root().join("source");
    create_workspace(&source);

    // Strict mode preserves the old hard failure.
    let init = fixture.failure(
        fixture.root(),
        [
            os("init"),
            os(source.as_os_str()),
            os("--here"),
            os("--cow-only"),
        ],
    );
    assert!(init.stdout.is_empty());
    assert!(init.stderr.contains("copy-on-write cloning unavailable"));
    assert!(!source.join(".rift").exists());
    assert!(!fixture.root().join(".rifts").exists());
    assert_no_reflink_probe_files(&source);
    assert_registry_empty(fixture.database());

    // Default init succeeds with a plain-copy note.
    let init = fixture.success(
        fixture.root(),
        [os("init"), os(source.as_os_str()), os("--here")],
    );
    assert!(source.join(".rift").exists());
    assert!(init.stderr.contains("regular copies"));

    let doctor = fixture.success(&source, ["doctor"]);
    assert!(doctor.stdout.contains("regular copies only"));
    assert!(
        fixture
            .success(&source, [os("doctor"), os("--json")])
            .stdout
            .contains("portable")
    );

    let strict = fixture.failure(&source, ["create", "--name", "strict", "--cow-only"]);
    assert!(strict.stderr.contains("copy-on-write cloning unavailable"));
    assert!(!fixture.root().join(".rifts/source/strict").exists());

    let child = fixture
        .success(&source, ["create", "--name", "child"])
        .single_stdout_path();
    assert_eq!(child, fixture.root().join(".rifts/source/child"));
    assert_workspace_copy(&child);

    // A plain copy diverges from the source.
    fs::write(source.join("untracked.txt"), "changed after copy").unwrap();
    assert_eq!(
        fs::read_to_string(child.join("untracked.txt")).unwrap(),
        "kept"
    );
}

#[cfg(target_os = "linux")]
fn create_workspace(source: &Path) {
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("nested/file.txt"), "hello from cli e2e").unwrap();
    fs::write(source.join("untracked.txt"), "kept").unwrap();
}

#[cfg(target_os = "linux")]
fn assert_workspace_copy(path: &Path) {
    assert_eq!(
        fs::read_to_string(path.join("nested/file.txt")).unwrap(),
        "hello from cli e2e"
    );
    assert_eq!(
        fs::read_to_string(path.join("untracked.txt")).unwrap(),
        "kept"
    );
    assert!(path.join(".rift").exists());
}

#[cfg(target_os = "linux")]
fn assert_no_reflink_probe_files(path: &Path) {
    assert!(
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .all(|name| !name.to_string_lossy().starts_with(".rift-reflink-probe"))
    );
}

#[cfg(target_os = "linux")]
fn assert_paths_unordered(mut actual: Vec<PathBuf>, expected: &[PathBuf]) {
    let mut expected = expected.to_vec();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[cfg(target_os = "linux")]
fn os(arg: impl AsRef<OsStr>) -> OsString {
    arg.as_ref().to_os_string()
}
