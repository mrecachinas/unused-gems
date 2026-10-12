use std::ffi::OsStr;
use std::path::Path;

#[path = "../benches/support/mod.rs"]
mod support;

#[test]
fn default_target_is_relative_to_manifest() {
    let manifest = Path::new("project");
    assert_eq!(
        support::binary_path(manifest, None, None),
        manifest
            .join("target/release")
            .join(format!("unused-gems{}", std::env::consts::EXE_SUFFIX))
    );
}

#[test]
fn honors_absolute_and_relative_target_directories() {
    let dir = tempfile::TempDir::new().unwrap();
    for target in [dir.path(), Path::new("custom-target")] {
        assert_eq!(
            support::binary_path(Path::new("project"), None, Some(target.as_os_str())),
            target
                .join("release")
                .join(format!("unused-gems{}", std::env::consts::EXE_SUFFIX))
        );
    }
}

#[test]
fn explicit_binary_takes_precedence_and_is_not_modified() {
    assert_eq!(
        support::binary_path(
            Path::new("project"),
            Some(OsStr::new("custom-binary")),
            Some(OsStr::new("custom-target")),
        ),
        Path::new("custom-binary")
    );
}
