use std::collections::HashSet;

use once_cell::sync::Lazy;

static CURRENT_PLATFORM_SET: Lazy<HashSet<String>> = Lazy::new(resolve_current_platform_set);

pub fn current_platform_set() -> &'static HashSet<String> {
    &CURRENT_PLATFORM_SET
}

fn resolve_current_platform_set() -> HashSet<String> {
    if let Ok(tokens) = std::env::var("UNUSED_GEMS_PLATFORM_TOKENS") {
        let parsed: HashSet<String> = tokens
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    if std::env::var("UNUSED_GEMS_PROBE_RUBY_PLATFORM").is_ok() {
        if let Some(tokens) = probe_ruby_platform() {
            return tokens;
        }
    }

    fallback_platforms()
}

fn probe_ruby_platform() -> Option<HashSet<String>> {
    let output = std::process::Command::new("ruby")
        .args([
            "-e",
            "puts RUBY_ENGINE; puts RUBY_PLATFORM; puts RUBY_VERSION",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let engine = lines.next()?;
    let platform = lines.next()?;
    let version = lines.next()?;
    Some(derive_platform_tokens(engine, platform, version))
}

pub fn derive_platform_tokens(engine: &str, ruby_platform: &str, version: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    tokens.insert("ruby".to_string());

    match engine {
        "ruby" => {
            tokens.insert("mri".to_string());
            insert_mri_family_tokens(&mut tokens);
            if let Some(version_token) = mri_version_token(version) {
                tokens.insert(version_token);
            }
        }
        "jruby" => {
            tokens.insert("jruby".to_string());
        }
        "truffleruby" => {
            tokens.insert("truffleruby".to_string());
        }
        other if !other.is_empty() => {
            tokens.insert(other.to_string());
        }
        _ => {}
    }

    let platform = ruby_platform.to_ascii_lowercase();
    if platform.contains("darwin") {
        tokens.insert("darwin".to_string());
    }
    if platform.contains("linux") {
        tokens.insert("linux".to_string());
    }
    if platform.contains("mingw") {
        tokens.insert("mingw".to_string());
        if platform.contains("x64") || platform.contains("x86_64") {
            tokens.insert("x64_mingw".to_string());
        }
    }
    if platform.contains("mswin") {
        tokens.insert("mswin".to_string());
    }
    if platform.contains("java") {
        tokens.insert("java".to_string());
    }

    tokens
}

fn fallback_platforms() -> HashSet<String> {
    let mut platforms = HashSet::new();
    platforms.insert("ruby".to_string());
    platforms.insert("mri".to_string());
    insert_mri_family_tokens(&mut platforms);
    #[cfg(target_os = "macos")]
    platforms.insert("darwin".to_string());
    #[cfg(target_os = "linux")]
    platforms.insert("linux".to_string());
    #[cfg(target_os = "windows")]
    {
        platforms.insert("mingw".to_string());
        platforms.insert("x64_mingw".to_string());
        platforms.insert("mswin".to_string());
    }
    platforms
}

fn insert_mri_family_tokens(platforms: &mut HashSet<String>) {
    for minor in 24..=40 {
        platforms.insert(format!("mri_{minor}"));
    }
}

fn mri_version_token(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u8>().ok()?;
    let minor = parts.next()?.parse::<u8>().ok()?;
    Some(format!("mri_{major}{minor}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mri_platforms() {
        let tokens = derive_platform_tokens("ruby", "arm64-darwin23", "3.4.2");
        assert!(tokens.contains("ruby"));
        assert!(tokens.contains("mri"));
        assert!(tokens.contains("mri_34"));
        assert!(tokens.contains("mri_40"));
        assert!(tokens.contains("darwin"));
    }

    #[test]
    fn maps_linux_platform() {
        let tokens = derive_platform_tokens("ruby", "x86_64-linux", "3.3.0");
        assert!(tokens.contains("linux"));
        assert!(tokens.contains("mri_33"));
    }

    #[test]
    fn maps_windows_platforms() {
        let tokens = derive_platform_tokens("ruby", "x64-mingw-ucrt", "3.2.0");
        assert!(tokens.contains("mingw"));
        assert!(tokens.contains("x64_mingw"));
    }

    #[test]
    fn maps_jruby_and_truffleruby() {
        let jruby = derive_platform_tokens("jruby", "java", "9.4.0.0");
        assert!(jruby.contains("jruby"));
        assert!(jruby.contains("java"));

        let truffle = derive_platform_tokens("truffleruby", "x86_64-linux", "24.1.0");
        assert!(truffle.contains("truffleruby"));
        assert!(truffle.contains("linux"));
    }

    #[test]
    fn fallback_keeps_static_mri_tokens() {
        let tokens = fallback_platforms();
        assert!(tokens.contains("ruby"));
        assert!(tokens.contains("mri"));
        assert!(tokens.contains("mri_31"));
        assert!(tokens.contains("mri_40"));
    }
}
