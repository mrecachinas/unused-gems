use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct GemEntry {
    pub name: String,
    /// Union of the groups of every declaration.
    pub groups: Vec<String>,
    /// Union of declared platforms; empty when any declaration targets all platforms.
    pub platforms: Vec<String>,
    pub require_paths: Vec<String>,
    pub module_names: Vec<String>,
    /// True only when every declaration disables autorequire.
    pub require_false: bool,
    pub source: Option<GemSource>,
    /// Every declaration of this gem. Bundler keeps duplicate `gem` lines as separate
    /// dependencies and `Bundler.require` evaluates each one on its own. Entries built
    /// without declarations fall back to the aggregate fields above.
    pub declarations: Vec<GemDeclaration>,
}

/// Static evaluation of Bundler's `install_if` conditions for one declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallCondition {
    #[default]
    Always,
    /// Statically false (e.g. `install_if: -> { false }`): Bundler never installs
    /// or requires this declaration.
    Never,
    /// Depends on runtime state; treated as possibly installed.
    Unknown,
}

impl InstallCondition {
    /// Bundler ANDs all enclosing and inline conditions together.
    pub fn all(conditions: &[InstallCondition]) -> InstallCondition {
        if conditions.contains(&InstallCondition::Never) {
            InstallCondition::Never
        } else if conditions.contains(&InstallCondition::Unknown) {
            InstallCondition::Unknown
        } else {
            InstallCondition::Always
        }
    }
}

/// One `gem`/`gemspec` declaration, mirroring a single `Bundler::Dependency`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GemDeclaration {
    pub groups: Vec<String>,
    pub platforms: Vec<String>,
    /// False for `require: false`, `require: nil` and `require: []`.
    pub autorequire: bool,
    pub install_if: InstallCondition,
    /// File and 1-based line that declared the gem.
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
}

impl GemDeclaration {
    fn applies_to_platform(&self, tokens: &HashSet<String>) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|p| tokens.contains(p))
    }
}

impl GemEntry {
    pub fn has_platform_constraints(&self) -> bool {
        !self.platforms.is_empty() || self.declarations.iter().any(|d| !d.platforms.is_empty())
    }

    /// Whether `Bundler.require` loads this gem for the active groups: any single
    /// declaration must be in an active group, keep autorequire enabled and not be
    /// statically excluded by `install_if`.
    pub fn bundler_autorequired(&self, is_active: impl Fn(&str) -> bool) -> bool {
        if self.declarations.is_empty() {
            return !self.require_false && self.groups.iter().any(|g| is_active(g));
        }
        self.declarations.iter().any(|d| {
            d.autorequire
                && d.install_if != InstallCondition::Never
                && d.groups.iter().any(|g| is_active(g))
        })
    }

    /// Drop declarations that do not apply on the current platform, like Bundler's
    /// `Dependency#current_platform?`. When none applies, every declaration is kept
    /// so the gem is still reported as platform-excluded.
    pub fn retain_declarations_for_platform(&mut self, tokens: &HashSet<String>) {
        let applicable = self
            .declarations
            .iter()
            .filter(|d| d.applies_to_platform(tokens))
            .count();
        if applicable == 0 || applicable == self.declarations.len() {
            return;
        }
        self.declarations.retain(|d| d.applies_to_platform(tokens));
        self.refresh_aggregates();
    }

    /// Recompute `groups`, `platforms` and `require_false` from `declarations`.
    pub fn refresh_aggregates(&mut self) {
        if self.declarations.is_empty() {
            return;
        }
        let mut groups: Vec<String> = Vec::new();
        let mut platforms: Vec<String> = Vec::new();
        let mut all_platforms = false;
        for decl in &self.declarations {
            for g in &decl.groups {
                if !groups.contains(g) {
                    groups.push(g.clone());
                }
            }
            if decl.platforms.is_empty() {
                all_platforms = true;
            }
            for p in &decl.platforms {
                if !platforms.contains(p) {
                    platforms.push(p.clone());
                }
            }
        }
        if all_platforms {
            platforms.clear();
        }
        self.groups = groups;
        self.platforms = platforms;
        self.require_false = self.declarations.iter().all(|d| !d.autorequire);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GemSource {
    Path(PathBuf),
    Git(String),
}

#[derive(Debug, Clone, Default)]
pub struct FileResult {
    pub rel_path: String,
    pub requires: Vec<String>,
    /// Unique constant names referenced by this file (reused for cache entries).
    pub modules: Vec<String>,
    pub bundler_groups: Vec<String>,
    pub bundler_require_all: bool,
    /// Active Storage services named by `has_*_attached ..., service: :name`.
    pub storage_services: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ScanResults {
    pub requires: HashMap<String, HashSet<String>>,
    pub modules: HashMap<String, HashSet<String>>,
    pub bundler_groups: HashSet<String>,
    pub bundler_require_all: bool,
    /// Script file contents collected during the walk: (rel_path, content_bytes)
    pub script_files: Vec<(String, Vec<u8>)>,
    /// Active Storage services selected by attachment declarations in code.
    pub storage_services: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GemStatus {
    Used,
    Unused,
    BundlerRequired,
    Ignored,
    PlatformExcluded,
}

impl std::fmt::Display for GemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GemStatus::Used => write!(f, "used"),
            GemStatus::Unused => write!(f, "unused"),
            GemStatus::BundlerRequired => write!(f, "bundler_required"),
            GemStatus::Ignored => write!(f, "ignored"),
            GemStatus::PlatformExcluded => write!(f, "platform_excluded"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Evidence {
    Require {
        path: String,
        file: String,
    },
    Module {
        name: String,
        file: String,
    },
    Script {
        file: String,
    },
    BundlerRequire,
    /// Rails adapter/service configuration. The JSON `type` stays
    /// `database_adapter` for compatibility; `kind` distinguishes database,
    /// cable, storage and cache configuration and `source` names the file.
    DatabaseAdapter {
        adapter: String,
        kind: String,
        source: String,
    },
}

#[derive(Debug)]
pub struct GemResult {
    pub status: GemStatus,
    pub files: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub gem: GemEntry,
}
