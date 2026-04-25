use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct AdapterOverrides {
    pub database: HashMap<String, String>,
    pub cable: HashMap<String, String>,
    pub storage: HashMap<String, String>,
    pub cache: HashMap<String, String>,
}

pub fn database_gem(adapter: &str, overrides: &AdapterOverrides) -> String {
    overrides
        .database
        .get(adapter)
        .cloned()
        .unwrap_or_else(|| match adapter {
            "postgresql" => "pg".to_string(),
            "mysql" => "mysql2".to_string(),
            "sqlite" | "sqlite3" => "sqlite3".to_string(),
            "trilogy" => "trilogy".to_string(),
            "jdbcmysql" => "activerecord-jdbcmysql-adapter".to_string(),
            "jdbcpostgresql" => "activerecord-jdbcpostgresql-adapter".to_string(),
            "jdbcsqlite3" => "activerecord-jdbcsqlite3-adapter".to_string(),
            other => other.to_string(),
        })
}

pub fn cable_gem(adapter: &str, overrides: &AdapterOverrides) -> String {
    overrides
        .cable
        .get(adapter)
        .cloned()
        .unwrap_or_else(|| match adapter {
            "redis" => "redis".to_string(),
            "postgresql" | "postgres" => "pg".to_string(),
            "solid_cable" => "solid_cable".to_string(),
            "any_cable" | "anycable" => "anycable".to_string(),
            other => other.to_string(),
        })
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
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_builtin_database_adapters() {
        let overrides = AdapterOverrides::default();
        assert_eq!(database_gem("postgresql", &overrides), "pg");
        assert_eq!(database_gem("mysql", &overrides), "mysql2");
        assert_eq!(database_gem("sqlite3", &overrides), "sqlite3");
        assert_eq!(
            database_gem("jdbcpostgresql", &overrides),
            "activerecord-jdbcpostgresql-adapter"
        );
    }

    #[test]
    fn maps_builtin_cable_adapters() {
        let overrides = AdapterOverrides::default();
        assert_eq!(cable_gem("redis", &overrides), "redis");
        assert_eq!(cable_gem("postgres", &overrides), "pg");
        assert_eq!(cable_gem("solid_cable", &overrides), "solid_cable");
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
        assert_eq!(database_gem("postgresql", &overrides), "custom-pg");
        assert_eq!(
            storage_gem("Custom", &overrides),
            Some("custom-storage".into())
        );
    }
}
