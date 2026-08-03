use std::collections::HashSet;

use once_cell::sync::Lazy;

#[derive(Debug)]
struct RuntimePlatform {
    dsl_tokens: HashSet<String>,
    gem_platform: Option<String>,
}

static CURRENT_RUNTIME: Lazy<RuntimePlatform> = Lazy::new(resolve_current_runtime);

pub fn current_platform_set() -> &'static HashSet<String> {
    &CURRENT_RUNTIME.dsl_tokens
}

pub fn current_gem_platform() -> Option<&'static str> {
    CURRENT_RUNTIME.gem_platform.as_deref()
}

fn resolve_current_runtime() -> RuntimePlatform {
    let mut runtime = std::env::var("UNUSED_GEMS_RUBY_RUNTIME")
        .ok()
        .and_then(|value| parse_runtime_override(&value))
        .or_else(probe_ruby_platform)
        .unwrap_or_else(fallback_runtime);

    if let Ok(tokens) = std::env::var("UNUSED_GEMS_PLATFORM_TOKENS") {
        let parsed: HashSet<String> = tokens
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect();
        if !parsed.is_empty() {
            runtime.dsl_tokens = parsed;
        }
    }
    runtime
}

fn parse_runtime_override(value: &str) -> Option<RuntimePlatform> {
    let mut fields = value.split('|');
    let engine = fields.next()?;
    let ruby_platform = fields.next()?;
    let version = fields.next()?;
    let gem_platform = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    Some(RuntimePlatform {
        dsl_tokens: derive_platform_tokens(engine, ruby_platform, version),
        gem_platform: (!gem_platform.is_empty()).then(|| gem_platform.to_string()),
    })
}

fn probe_ruby_platform() -> Option<RuntimePlatform> {
    let output = std::process::Command::new("ruby")
        .args([
            "-rrubygems",
            "-e",
            "puts RUBY_ENGINE; puts RUBY_PLATFORM; puts RUBY_VERSION; puts Gem::Platform.local",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let engine = lines.next()?;
    let ruby_platform = lines.next()?;
    let version = lines.next()?;
    let gem_platform = lines.next()?;
    Some(RuntimePlatform {
        dsl_tokens: derive_platform_tokens(engine, ruby_platform, version),
        gem_platform: (!gem_platform.is_empty()).then(|| gem_platform.to_string()),
    })
}

pub fn derive_platform_tokens(engine: &str, ruby_platform: &str, version: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let windows = is_windows_platform(ruby_platform);
    let version_suffix = bundler_version_suffix(version);

    if windows {
        add_windows_tokens(&mut tokens, version_suffix.as_deref());
        return tokens;
    }

    match engine {
        "ruby" => {
            add_family_token(&mut tokens, "ruby", version_suffix.as_deref());
            add_family_token(&mut tokens, "mri", version_suffix.as_deref());
        }
        "rbx" => {
            add_family_token(&mut tokens, "ruby", version_suffix.as_deref());
            tokens.insert("rbx".to_string());
        }
        "truffleruby" => {
            add_family_token(&mut tokens, "ruby", version_suffix.as_deref());
            tokens.insert("truffleruby".to_string());
        }
        "jruby" => {
            tokens.insert("jruby".to_string());
            if matches!(version_suffix.as_deref(), Some("18" | "19")) {
                tokens.insert(format!("jruby_{}", version_suffix.unwrap()));
            }
        }
        _ => {}
    }

    tokens
}

fn add_family_token(tokens: &mut HashSet<String>, family: &str, version_suffix: Option<&str>) {
    tokens.insert(family.to_string());
    if let Some(version_suffix) = version_suffix {
        tokens.insert(format!("{family}_{version_suffix}"));
    }
}

fn add_windows_tokens(tokens: &mut HashSet<String>, version_suffix: Option<&str>) {
    const FAMILIES: &[&str] = &["windows", "mswin", "mswin64", "mingw", "x64_mingw"];
    for family in FAMILIES {
        tokens.insert((*family).to_string());
        let Some(version_suffix) = version_suffix else {
            continue;
        };
        let numeric = version_suffix.parse::<u8>().unwrap_or_default();
        if (*family == "mswin64" && numeric == 18)
            || (*family == "x64_mingw" && matches!(numeric, 18 | 19))
        {
            continue;
        }
        tokens.insert(format!("{family}_{version_suffix}"));
    }
}

fn bundler_version_suffix(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let suffix = format!("{major}{minor}");
    const VALID: &[&str] = &[
        "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "30", "31", "32", "33", "34",
        "40", "41",
    ];
    VALID.contains(&suffix.as_str()).then_some(suffix)
}

fn is_windows_platform(ruby_platform: &str) -> bool {
    let platform = ruby_platform.to_ascii_lowercase();
    platform.contains("mingw") || platform.contains("mswin")
}

fn fallback_runtime() -> RuntimePlatform {
    let mut dsl_tokens = HashSet::new();
    #[cfg(target_os = "windows")]
    add_windows_tokens(&mut dsl_tokens, None);
    #[cfg(not(target_os = "windows"))]
    {
        dsl_tokens.insert("ruby".to_string());
        dsl_tokens.insert("mri".to_string());
    }
    RuntimePlatform {
        dsl_tokens,
        gem_platform: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(tokens: &[&str]) -> HashSet<String> {
        tokens.iter().map(|token| (*token).to_string()).collect()
    }

    #[test]
    fn maps_mri_34_to_only_bundler_dsl_tokens() {
        assert_eq!(
            derive_platform_tokens("ruby", "arm64-darwin23", "3.4.2"),
            set(&["ruby", "ruby_34", "mri", "mri_34"])
        );
    }

    #[test]
    fn maps_jruby_without_invalid_java_or_modern_version_token() {
        assert_eq!(
            derive_platform_tokens("jruby", "java", "3.1.4"),
            set(&["jruby"])
        );
        assert_eq!(
            derive_platform_tokens("jruby", "java", "1.9.3"),
            set(&["jruby", "jruby_19"])
        );
    }

    #[test]
    fn maps_windows_mri_to_all_bundler_windows_families() {
        assert_eq!(
            derive_platform_tokens("ruby", "x64-mingw-ucrt", "3.4.2"),
            set(&[
                "windows",
                "windows_34",
                "mswin",
                "mswin_34",
                "mswin64",
                "mswin64_34",
                "mingw",
                "mingw_34",
                "x64_mingw",
                "x64_mingw_34",
            ])
        );
    }

    #[test]
    fn does_not_generate_unknown_future_version_tokens() {
        assert_eq!(
            derive_platform_tokens("ruby", "x86_64-linux", "3.5.0"),
            set(&["ruby", "mri"])
        );
    }

    #[test]
    fn parses_deterministic_runtime_override() {
        let runtime = parse_runtime_override("ruby|arm64-darwin25|3.4.2|arm64-darwin-25")
            .expect("valid override");
        assert_eq!(
            runtime.dsl_tokens,
            set(&["ruby", "ruby_34", "mri", "mri_34"])
        );
        assert_eq!(runtime.gem_platform.as_deref(), Some("arm64-darwin-25"));
    }
}
