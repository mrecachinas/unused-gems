use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct GemEntry {
    pub name: String,
    pub groups: Vec<String>,
    pub require_paths: Vec<String>,
    pub module_names: Vec<String>,
    pub require_false: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FileResult {
    pub rel_path: String,
    pub requires: Vec<String>,
    pub bundler_groups: Vec<String>,
    pub bundler_require_all: bool,
}

#[derive(Debug, Default)]
pub struct ScanResults {
    pub requires: HashMap<String, HashSet<String>>,
    pub modules: HashMap<String, HashSet<String>>,
    pub bundler_groups: HashSet<String>,
    pub bundler_require_all: bool,
    /// Script file contents collected during the walk: (rel_path, content_bytes)
    pub script_files: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GemStatus {
    Used,
    Unused,
    BundlerRequired,
    RequireFalse,
    Ignored,
}

impl std::fmt::Display for GemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GemStatus::Used => write!(f, "used"),
            GemStatus::Unused => write!(f, "unused"),
            GemStatus::BundlerRequired => write!(f, "bundler_required"),
            GemStatus::RequireFalse => write!(f, "require_false"),
            GemStatus::Ignored => write!(f, "ignored"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Evidence {
    Require { path: String, file: String },
    Module { name: String, file: String },
    Script { file: String },
    BundlerRequire,
    DatabaseAdapter { adapter: String },
}

#[derive(Debug)]
pub struct GemResult {
    pub status: GemStatus,
    pub files: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub gem: GemEntry,
}
