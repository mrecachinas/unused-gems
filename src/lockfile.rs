use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockedSource {
    Registry,
    Git,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedArtifact {
    pub version: String,
    pub platform: Option<String>,
    pub dependencies: Vec<String>,
    pub source: LockedSource,
}

impl LockedArtifact {
    pub fn full_name(&self, gem_name: &str) -> String {
        match &self.platform {
            Some(platform) => format!("{gem_name}-{}-{platform}", self.version),
            None => format!("{gem_name}-{}", self.version),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Lockfile {
    dependencies: HashMap<String, Vec<String>>,
    artifacts: HashMap<String, Vec<LockedArtifact>>,
}

impl Lockfile {
    pub fn dependencies(&self) -> &HashMap<String, Vec<String>> {
        &self.dependencies
    }

    pub fn artifacts(&self, gem_name: &str) -> &[LockedArtifact] {
        self.artifacts
            .get(gem_name)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn preferred_registry_full_names(
        &self,
        gem_name: &str,
        current_platform: Option<&str>,
    ) -> Vec<String> {
        let artifacts = self.artifacts(gem_name);
        let mut preferred = Vec::new();

        if let Some(current_platform) = current_platform {
            preferred.extend(
                artifacts
                    .iter()
                    .filter(|artifact| {
                        artifact.source == LockedSource::Registry
                            && artifact.platform.as_deref() == Some(current_platform)
                    })
                    .map(|artifact| artifact.full_name(gem_name)),
            );
            preferred.extend(
                artifacts
                    .iter()
                    .filter(|artifact| {
                        artifact.source == LockedSource::Registry
                            && artifact.platform.as_deref().is_some_and(|platform| {
                                platform != current_platform
                                    && Self::compatible_gem_platform(platform, current_platform)
                            })
                    })
                    .map(|artifact| artifact.full_name(gem_name)),
            );
        }

        preferred.extend(
            artifacts
                .iter()
                .filter(|artifact| {
                    artifact.source == LockedSource::Registry && artifact.platform.is_none()
                })
                .map(|artifact| artifact.full_name(gem_name)),
        );
        preferred.dedup();
        preferred
    }

    fn compatible_gem_platform(locked: &str, current: &str) -> bool {
        locked
            .strip_prefix(current)
            .is_some_and(|suffix| suffix.starts_with('-'))
            || current
                .strip_prefix(locked)
                .is_some_and(|suffix| suffix.starts_with('-'))
    }

    fn merge(&mut self, other: Lockfile) {
        for (gem, deps) in other.dependencies {
            let entry = self.dependencies.entry(gem).or_default();
            for dep in deps {
                if !entry.contains(&dep) {
                    entry.push(dep);
                }
            }
        }

        for (gem, artifacts) in other.artifacts {
            let entry = self.artifacts.entry(gem).or_default();
            for artifact in artifacts {
                if let Some(existing) = entry.iter_mut().find(|existing| {
                    existing.version == artifact.version
                        && existing.platform == artifact.platform
                        && existing.source == artifact.source
                }) {
                    for dep in artifact.dependencies {
                        if !existing.dependencies.contains(&dep) {
                            existing.dependencies.push(dep);
                        }
                    }
                } else {
                    entry.push(artifact);
                }
            }
        }
    }
}

pub fn parse_gemfile_lock(dir: &Path) -> Lockfile {
    parse_lockfile_path(&dir.join("Gemfile.lock"))
}

pub fn parse_gemfile_locks(root_dir: &Path, gemfile_paths: &[PathBuf]) -> Lockfile {
    let mut lockfiles = vec![root_dir.join("Gemfile.lock")];
    for gemfile in gemfile_paths {
        if let Some(parent) = gemfile.parent() {
            let lockfile = parent.join("Gemfile.lock");
            if !lockfiles.contains(&lockfile) {
                lockfiles.push(lockfile);
            }
        }
    }

    let mut merged = Lockfile::default();
    for lockfile in lockfiles {
        merged.merge(parse_lockfile_path(&lockfile));
    }
    merged
}

#[derive(Debug)]
struct RawArtifact {
    name: String,
    resolved: String,
    dependencies: Vec<String>,
    source: LockedSource,
}

fn parse_lockfile_path(lock_path: &Path) -> Lockfile {
    let content = match std::fs::read_to_string(lock_path) {
        Ok(content) => content,
        Err(_) => return Lockfile::default(),
    };

    let mut raw_artifacts = Vec::new();
    let mut current_artifact: Option<usize> = None;
    let mut current_source: Option<LockedSource> = None;
    let mut in_specs = false;
    let mut in_platforms = false;
    let mut platforms = Vec::new();

    for line in content.lines() {
        if !line.starts_with(' ') && !line.is_empty() {
            current_artifact = None;
            in_specs = false;
            in_platforms = line == "PLATFORMS";
            current_source = match line {
                "GEM" => Some(LockedSource::Registry),
                "GIT" => Some(LockedSource::Git),
                "PATH" => Some(LockedSource::Path),
                _ => None,
            };
            continue;
        }

        if in_platforms {
            if let Some(platform) = line.strip_prefix("  ") {
                let platform = platform.trim();
                if !platform.is_empty() {
                    platforms.push(platform.to_string());
                }
            }
            continue;
        }

        if line == "  specs:" {
            in_specs = current_source.is_some();
            current_artifact = None;
            continue;
        }

        if !in_specs {
            continue;
        }

        if line.starts_with("    ") && !line.starts_with("      ") {
            let Some((name, resolved)) = parse_spec_line(line.trim()) else {
                current_artifact = None;
                continue;
            };
            raw_artifacts.push(RawArtifact {
                name,
                resolved,
                dependencies: Vec::new(),
                source: current_source.unwrap_or(LockedSource::Registry),
            });
            current_artifact = Some(raw_artifacts.len() - 1);
        } else if let (Some(index), Some(dep_name)) =
            (current_artifact, line.split_whitespace().next())
        {
            raw_artifacts[index].dependencies.push(dep_name.to_string());
        }
    }

    platforms.sort_by_key(|platform| std::cmp::Reverse(platform.len()));
    let mut lockfile = Lockfile::default();
    for raw in raw_artifacts {
        let (version, platform) = split_resolved_artifact(&raw.resolved, &platforms);
        let dependencies = raw.dependencies;
        let artifact = LockedArtifact {
            version,
            platform,
            dependencies: dependencies.clone(),
            source: raw.source,
        };
        lockfile
            .artifacts
            .entry(raw.name.clone())
            .or_default()
            .push(artifact);
        let deps = lockfile.dependencies.entry(raw.name).or_default();
        for dep in dependencies {
            if !deps.contains(&dep) {
                deps.push(dep);
            }
        }
    }
    lockfile
}

fn parse_spec_line(line: &str) -> Option<(String, String)> {
    let open = line.find(" (")?;
    let close = line[open + 2..].find(')')? + open + 2;
    let name = line[..open].trim();
    let resolved = line[open + 2..close].trim();
    if name.is_empty() || resolved.is_empty() {
        return None;
    }
    Some((name.to_string(), resolved.to_string()))
}

fn split_resolved_artifact(resolved: &str, platforms: &[String]) -> (String, Option<String>) {
    let mut suffixes: Vec<String> = platforms
        .iter()
        .filter(|platform| platform.as_str() != "ruby")
        .flat_map(|platform| {
            let mut suffixes = vec![platform.clone()];
            if let Some(normalized) = normalize_os_versioned_platform(platform) {
                suffixes.push(normalized);
            }
            suffixes
        })
        .collect();
    suffixes.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    suffixes.dedup();

    for platform in suffixes {
        let suffix = format!("-{platform}");
        if let Some(version) = resolved.strip_suffix(&suffix) {
            return (version.to_string(), Some(platform));
        }
    }
    (resolved.to_string(), None)
}

fn normalize_os_versioned_platform(platform: &str) -> Option<String> {
    let (base, version) = platform.rsplit_once('-')?;
    (!version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| base.to_string())
}

pub fn count_transitive_deps(gem_name: &str, lock_deps: &HashMap<String, Vec<String>>) -> usize {
    let mut visited = HashSet::new();
    count_inner(gem_name, lock_deps, &mut visited)
}

fn count_inner(
    gem_name: &str,
    lock_deps: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
) -> usize {
    if visited.contains(gem_name) {
        return 0;
    }
    visited.insert(gem_name.to_string());

    let direct = match lock_deps.get(gem_name) {
        Some(deps) => deps,
        None => return 0,
    };

    let mut count = direct.len();
    for dependency in direct {
        count += count_inner(dependency, lock_deps, visited);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  remote: https://rubygems.org/
  specs:
    actionpack (7.1.0)
      activesupport (= 7.1.0)
      rack (~> 3.0)
    activesupport (7.1.0)
      i18n (>= 1.6)
    i18n (1.14.1)
    rack (3.0.8)

PLATFORMS
  ruby
",
        )
        .unwrap();
        let lockfile = parse_gemfile_lock(dir.path());
        let deps = lockfile.dependencies();
        assert_eq!(deps["actionpack"], vec!["activesupport", "rack"]);
        assert_eq!(deps["activesupport"], vec!["i18n"]);
        assert!(deps["i18n"].is_empty());
        assert!(deps["rack"].is_empty());
        assert_eq!(lockfile.artifacts("actionpack")[0].version, "7.1.0");
        assert_eq!(lockfile.artifacts("actionpack")[0].platform, None);
    }

    #[test]
    fn parses_exact_native_and_ruby_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  specs:
    nokogiri (1.18.9)
    nokogiri (1.18.9-arm64-darwin-25)

PLATFORMS
  arm64-darwin-25
  ruby
",
        )
        .unwrap();

        let lockfile = parse_gemfile_lock(dir.path());
        assert_eq!(
            lockfile.preferred_registry_full_names("nokogiri", Some("arm64-darwin-25")),
            vec![
                "nokogiri-1.18.9-arm64-darwin-25".to_string(),
                "nokogiri-1.18.9".to_string()
            ]
        );
        assert_eq!(
            lockfile.preferred_registry_full_names("nokogiri", Some("x86_64-linux")),
            vec!["nokogiri-1.18.9".to_string()]
        );
    }

    #[test]
    fn prefers_compatible_generic_native_platform_over_ruby() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  specs:
    nokogiri (1.18.9)
    nokogiri (1.18.9-arm64-darwin)

PLATFORMS
  arm64-darwin-25
  ruby
",
        )
        .unwrap();

        let lockfile = parse_gemfile_lock(dir.path());
        assert_eq!(
            lockfile.artifacts("nokogiri")[1].platform.as_deref(),
            Some("arm64-darwin")
        );
        assert_eq!(
            lockfile.preferred_registry_full_names("nokogiri", Some("arm64-darwin-25")),
            vec![
                "nokogiri-1.18.9-arm64-darwin".to_string(),
                "nokogiri-1.18.9".to_string()
            ]
        );
    }

    #[test]
    fn retains_path_and_git_sources_separately() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
PATH
  remote: vendor/local
  specs:
    local-gem (0.1.0)

GIT
  remote: https://example.test/repo.git
  revision: deadbeef
  specs:
    git-gem (2.0.0)
",
        )
        .unwrap();

        let lockfile = parse_gemfile_lock(dir.path());
        assert_eq!(
            lockfile.artifacts("local-gem")[0].source,
            LockedSource::Path
        );
        assert_eq!(lockfile.artifacts("git-gem")[0].source, LockedSource::Git);
        assert!(lockfile
            .preferred_registry_full_names("local-gem", None)
            .is_empty());
    }

    #[test]
    fn missing_lockfile_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile = parse_gemfile_lock(dir.path());
        assert!(lockfile.dependencies().is_empty());
    }

    #[test]
    fn count_transitive_deps_basic() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["b".to_string(), "c".to_string()]);
        deps.insert("b".to_string(), vec!["d".to_string()]);
        deps.insert("c".to_string(), vec![]);
        deps.insert("d".to_string(), vec![]);
        assert_eq!(count_transitive_deps("a", &deps), 3);
    }

    #[test]
    fn count_transitive_deps_circular() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["b".to_string()]);
        deps.insert("b".to_string(), vec!["a".to_string()]);
        assert_eq!(count_transitive_deps("a", &deps), 2);
    }

    #[test]
    fn count_transitive_deps_missing() {
        let deps = HashMap::new();
        assert_eq!(count_transitive_deps("nonexistent", &deps), 0);
    }

    #[test]
    fn parse_multiple_gemfile_locks_merges_nearest_lockfiles() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/admin");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(
            dir.path().join("Gemfile.lock"),
            "\
GEM
  specs:
    rails (7.1.0)
      actionpack (= 7.1.0)
    actionpack (7.1.0)
",
        )
        .unwrap();
        std::fs::write(
            engine.join("Gemfile.lock"),
            "\
GEM
  specs:
    puma (6.4.0)
      nio4r (~> 2.0)
    nio4r (2.7.0)
",
        )
        .unwrap();

        let lockfile = parse_gemfile_locks(dir.path(), &[engine.join("Gemfile")]);
        assert_eq!(lockfile.dependencies()["rails"], vec!["actionpack"]);
        assert_eq!(lockfile.dependencies()["puma"], vec!["nio4r"]);
        assert_eq!(lockfile.artifacts("rails")[0].version, "7.1.0");
        assert_eq!(lockfile.artifacts("puma")[0].version, "6.4.0");
    }
}
