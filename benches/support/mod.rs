use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub fn binary_path(manifest: &Path, binary: Option<&OsStr>, target: Option<&OsStr>) -> PathBuf {
    if let Some(binary) = binary {
        return PathBuf::from(binary);
    }
    let target = target
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("target"));
    target
        .join("release")
        .join(format!("unused-gems{}", std::env::consts::EXE_SUFFIX))
}
