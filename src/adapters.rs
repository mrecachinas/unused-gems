use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct AdapterOverrides {
    pub database: HashMap<String, String>,
    pub cable: HashMap<String, String>,
    pub storage: HashMap<String, String>,
    pub cache: HashMap<String, String>,
}

/// Gems that provide a Rails database adapter. Adapters backed by a separate
/// activerecord adapter gem also credit the driver gem they load.
pub fn database_gems(adapter: &str, overrides: &AdapterOverrides) -> Vec<String> {
    if let Some(gem) = overrides.database.get(adapter) {
        return vec![gem.clone()];
    }
    let gems: &[&str] = match adapter {
        "postgresql" | "postgres" => &["pg"],
        "mysql" | "mysql2" => &["mysql2"],
        "sqlite" | "sqlite3" => &["sqlite3"],
        "trilogy" => &["trilogy"],
        "postgis" => &["activerecord-postgis-adapter", "pg"],
        "sqlserver" => &["activerecord-sqlserver-adapter", "tiny_tds"],
        "oracle_enhanced" => &["activerecord-oracle_enhanced-adapter", "ruby-oci8"],
        "cockroachdb" => &["activerecord-cockroachdb-adapter", "pg"],
        "jdbcmysql" => &["activerecord-jdbcmysql-adapter"],
        "jdbcpostgresql" => &["activerecord-jdbcpostgresql-adapter"],
        "jdbcsqlite3" => &["activerecord-jdbcsqlite3-adapter"],
        other => {
            // Unknown adapter: credit a gem of the same name or the
            // conventional `activerecord-<adapter>-adapter`, whichever is declared.
            return vec![other.to_string(), format!("activerecord-{other}-adapter")];
        }
    };
    gems.iter().map(|gem| gem.to_string()).collect()
}

/// Rails adapter name for a database URL (`postgres://…`, `mysql2://…`,
/// `sqlite3:db/dev.sqlite3`). Accepts an ERB expression and uses the first
/// literal URL inside it, e.g. `<%= ENV.fetch("DATABASE_URL", "postgres://…") %>`.
pub fn database_url_adapter(value: &str) -> Option<String> {
    let value = value.trim();
    let literal = if value.starts_with("<%") {
        string_literals(value)
            .into_iter()
            .find(|literal| url_scheme(literal).is_some())?
    } else {
        value.trim_matches(|c| c == '"' || c == '\'')
    };
    let scheme = url_scheme(literal)?;
    // Mirrors ActiveRecord's ConnectionUrlResolver.
    let adapter = scheme.replace('-', "_");
    Some(if adapter == "postgres" {
        "postgresql".to_string()
    } else {
        adapter
    })
}

fn url_scheme(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(':')?;
    let valid = scheme
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        && (rest.starts_with("//") || scheme.starts_with("sqlite"));
    valid.then_some(scheme)
}

fn string_literals(source: &str) -> Vec<&str> {
    let mut literals = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[start] as char;
        let body = &rest[start + 1..];
        let Some(end) = body.find(quote) else {
            break;
        };
        literals.push(&body[..end]);
        rest = &body[end + 1..];
    }
    literals
}

/// Gem that provides an Action Cable adapter. Rails' built-in `async`,
/// `inline` and `test` adapters need no gem.
pub fn cable_gem(adapter: &str, overrides: &AdapterOverrides) -> Option<String> {
    if let Some(gem) = overrides.cable.get(adapter) {
        return Some(gem.clone());
    }
    match adapter {
        "async" | "inline" | "test" => None,
        "redis" => Some("redis".to_string()),
        "postgresql" | "postgres" => Some("pg".to_string()),
        "solid_cable" => Some("solid_cable".to_string()),
        "any_cable" | "anycable" => Some("anycable".to_string()),
        other => Some(other.to_string()),
    }
}

pub fn storage_gem(service: &str, overrides: &AdapterOverrides) -> Option<String> {
    overrides
        .storage
        .get(service)
        .cloned()
        .or_else(|| match service {
            "S3" | "AmazonS3" => Some("aws-sdk-s3".to_string()),
            "GCS" | "Google" => Some("google-cloud-storage".to_string()),
            "AzureStorage" => Some("azure-storage-blob".to_string()),
            "Disk" | "Mirror" => None,
            _ => None,
        })
}

pub fn cache_gem(store: &str, overrides: &AdapterOverrides) -> Option<String> {
    overrides.cache.get(store).cloned().or_else(|| match store {
        "redis_cache_store" => Some("redis".to_string()),
        "mem_cache_store" => Some("dalli".to_string()),
        "solid_cache_store" => Some("solid_cache".to_string()),
        // Built-in stores: memory_store, file_store, null_store.
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_builtin_database_adapters() {
        let overrides = AdapterOverrides::default();
        assert_eq!(database_gems("postgresql", &overrides), ["pg"]);
        assert_eq!(database_gems("mysql", &overrides), ["mysql2"]);
        assert_eq!(database_gems("mysql2", &overrides), ["mysql2"]);
        assert_eq!(database_gems("sqlite3", &overrides), ["sqlite3"]);
        assert_eq!(
            database_gems("jdbcpostgresql", &overrides),
            ["activerecord-jdbcpostgresql-adapter"]
        );
    }

    #[test]
    fn maps_third_party_database_adapters_to_adapter_and_driver_gems() {
        let overrides = AdapterOverrides::default();
        assert_eq!(
            database_gems("postgis", &overrides),
            ["activerecord-postgis-adapter", "pg"]
        );
        assert_eq!(
            database_gems("sqlserver", &overrides),
            ["activerecord-sqlserver-adapter", "tiny_tds"]
        );
        assert_eq!(
            database_gems("oracle_enhanced", &overrides),
            ["activerecord-oracle_enhanced-adapter", "ruby-oci8"]
        );
        assert_eq!(
            database_gems("cockroachdb", &overrides),
            ["activerecord-cockroachdb-adapter", "pg"]
        );
        assert_eq!(
            database_gems("clickhouse", &overrides),
            ["clickhouse", "activerecord-clickhouse-adapter"]
        );
    }

    #[test]
    fn parses_database_url_schemes() {
        assert_eq!(
            database_url_adapter("mysql2://localhost/app").as_deref(),
            Some("mysql2")
        );
        assert_eq!(
            database_url_adapter("\"postgres://u@h/db\"").as_deref(),
            Some("postgresql")
        );
        assert_eq!(
            database_url_adapter("postgresql://h/db").as_deref(),
            Some("postgresql")
        );
        assert_eq!(
            database_url_adapter("sqlite3:db/development.sqlite3").as_deref(),
            Some("sqlite3")
        );
        assert_eq!(
            database_url_adapter("trilogy://h/db").as_deref(),
            Some("trilogy")
        );
        assert_eq!(
            database_url_adapter("oracle-enhanced://h/db").as_deref(),
            Some("oracle_enhanced")
        );
        assert_eq!(
            database_url_adapter("<%= ENV.fetch(\"DATABASE_URL\", \"postgis://localhost/app\") %>")
                .as_deref(),
            Some("postgis")
        );
        assert_eq!(
            database_url_adapter("<%= ENV.fetch('DATABASE_URL') { 'mysql2://h/db' } %>").as_deref(),
            Some("mysql2")
        );
        assert_eq!(database_url_adapter("<%= ENV['DATABASE_URL'] %>"), None);
        assert_eq!(database_url_adapter("localhost"), None);
    }

    #[test]
    fn maps_builtin_cable_adapters() {
        let overrides = AdapterOverrides::default();
        assert_eq!(cable_gem("redis", &overrides), Some("redis".into()));
        assert_eq!(cable_gem("postgres", &overrides), Some("pg".into()));
        assert_eq!(
            cable_gem("solid_cable", &overrides),
            Some("solid_cable".into())
        );
    }

    #[test]
    fn builtin_cable_adapters_need_no_gem() {
        let overrides = AdapterOverrides::default();
        for adapter in ["async", "test", "inline"] {
            assert_eq!(cable_gem(adapter, &overrides), None, "{adapter}");
        }
    }

    #[test]
    fn builtin_storage_and_cache_backends_need_no_gem() {
        let overrides = AdapterOverrides::default();
        assert_eq!(storage_gem("Disk", &overrides), None);
        assert_eq!(storage_gem("Mirror", &overrides), None);
        for store in ["memory_store", "file_store", "null_store"] {
            assert_eq!(cache_gem(store, &overrides), None, "{store}");
        }
    }

    #[test]
    fn maps_builtin_storage_services() {
        let overrides = AdapterOverrides::default();
        assert_eq!(storage_gem("S3", &overrides), Some("aws-sdk-s3".into()));
        assert_eq!(
            storage_gem("Google", &overrides),
            Some("google-cloud-storage".into())
        );
        assert_eq!(storage_gem("Disk", &overrides), None);
    }

    #[test]
    fn maps_builtin_cache_stores() {
        let overrides = AdapterOverrides::default();
        assert_eq!(
            cache_gem("redis_cache_store", &overrides),
            Some("redis".into())
        );
        assert_eq!(
            cache_gem("mem_cache_store", &overrides),
            Some("dalli".into())
        );
        assert_eq!(cache_gem("unknown", &overrides), None);
    }

    #[test]
    fn user_overrides_take_precedence() {
        let mut overrides = AdapterOverrides::default();
        overrides
            .database
            .insert("postgresql".into(), "custom-pg".into());
        overrides
            .storage
            .insert("Custom".into(), "custom-storage".into());
        assert_eq!(database_gems("postgresql", &overrides), ["custom-pg"]);
        assert_eq!(
            storage_gem("Custom", &overrides),
            Some("custom-storage".into())
        );
    }
}
