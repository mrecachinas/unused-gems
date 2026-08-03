pub const REQUIRE_PATHS: &[(&str, &[&str])] = &[
    (
        "active_model_serializers",
        &["active_model_serializers", "active_model/serializer"],
    ),
    (
        "activerecord-session_store",
        &["active_record/session_store"],
    ),
    ("argon2", &["argon2"]),
    ("attr_encrypted", &["attr_encrypted"]),
    ("aws-sdk-core", &["aws-sdk-core"]),
    ("aws-sdk-s3", &["aws-sdk-s3"]),
    ("aws-sdk-sqs", &["aws-sdk-sqs"]),
    (
        "azure-storage-blob",
        &["azure/storage/blob", "azure-storage-blob"],
    ),
    ("batch-loader", &["batch-loader", "batch_loader"]),
    ("bcrypt", &["bcrypt"]),
    ("better_errors", &["better_errors"]),
    ("binding_of_caller", &["binding_of_caller"]),
    ("bootsnap", &["bootsnap", "bootsnap/setup"]),
    ("brakeman", &["brakeman"]),
    ("bullet", &["bullet"]),
    ("bundler-audit", &["bundler/audit", "bundler-audit"]),
    ("byebug", &["byebug"]),
    ("capybara", &["capybara"]),
    ("cssbundling-rails", &["cssbundling-rails"]),
    ("dalli", &["dalli"]),
    ("database_cleaner", &["database_cleaner"]),
    (
        "database_cleaner-active_record",
        &[
            "database_cleaner/active_record",
            "database_cleaner-active_record",
        ],
    ),
    ("ddtrace", &["datadog", "ddtrace"]),
    ("delayed_job", &["delayed_job"]),
    ("devise", &["devise"]),
    ("doorkeeper", &["doorkeeper"]),
    ("elasticsearch", &["elasticsearch"]),
    ("factory_bot", &["factory_bot"]),
    ("factory_bot_rails", &["factory_bot_rails"]),
    ("faker", &["faker"]),
    ("falcon", &["falcon"]),
    ("faraday", &["faraday"]),
    ("faraday-retry", &["faraday/retry", "faraday-retry"]),
    ("fast_blank", &["fast_blank"]),
    ("goldiloader", &["goldiloader"]),
    ("good_job", &["good_job"]),
    ("google-cloud-pubsub", &["google/cloud/pubsub"]),
    ("google-cloud-storage", &["google/cloud/storage"]),
    ("google-protobuf", &["google/protobuf"]),
    ("graphql", &["graphql"]),
    ("graphql-batch", &["graphql/batch"]),
    ("grpc", &["grpc"]),
    ("hiredis", &["hiredis"]),
    ("http", &["http"]),
    ("httparty", &["httparty"]),
    (
        "image_processing",
        &[
            "image_processing",
            "image_processing/vips",
            "image_processing/mini_magick",
        ],
    ),
    ("importmap-rails", &["importmap-rails", "importmap/rails"]),
    ("jsbundling-rails", &["jsbundling-rails"]),
    ("jwt", &["jwt"]),
    ("kaminari", &["kaminari"]),
    ("listen", &["listen"]),
    ("mail", &["mail"]),
    ("memory_profiler", &["memory_profiler"]),
    ("mini_magick", &["mini_magick"]),
    ("mysql2", &["mysql2"]),
    ("newrelic_rpm", &["newrelic_rpm"]),
    ("nokogiri", &["nokogiri"]),
    ("nokogiri-diff", &["nokogiri/diff"]),
    ("omniauth", &["omniauth"]),
    ("omniauth-oauth2", &["omniauth-oauth2", "omniauth/oauth2"]),
    ("opensearch-ruby", &["opensearch-ruby", "opensearch"]),
    ("pagy", &["pagy"]),
    ("pg", &["pg"]),
    ("premailer-rails", &["premailer/rails", "premailer-rails"]),
    ("pry", &["pry"]),
    ("pry-byebug", &["pry-byebug", "pry/byebug"]),
    ("pry-rails", &["pry-rails"]),
    ("puma", &["puma"]),
    ("pundit", &["pundit"]),
    ("rack-attack", &["rack/attack"]),
    ("rack-cors", &["rack/cors", "rack-cors"]),
    (
        "rack-mini-profiler",
        &["rack-mini-profiler", "rack/mini_profiler"],
    ),
    ("ransack", &["ransack"]),
    ("redis", &["redis"]),
    ("redis-namespace", &["redis-namespace", "redis/namespace"]),
    ("redis-rails", &["redis-rails", "redis/rails"]),
    ("resque", &["resque"]),
    ("rest-client", &["rest-client", "rest_client"]),
    ("rspec-core", &["rspec/core"]),
    ("rspec-expectations", &["rspec/expectations"]),
    ("rspec-mocks", &["rspec/mocks"]),
    ("rspec-rails", &["rspec/rails", "rspec-rails"]),
    ("rubocop", &["rubocop"]),
    (
        "rubocop-performance",
        &["rubocop-performance", "rubocop/performance"],
    ),
    ("rubocop-rails", &["rubocop-rails", "rubocop/rails"]),
    ("rubocop-rspec", &["rubocop-rspec", "rubocop/rspec"]),
    ("ruby-kafka", &["kafka"]),
    ("ruby-openai", &["openai"]),
    ("ruby-progressbar", &["ruby-progressbar", "progressbar"]),
    ("ruby-saml", &["onelogin/ruby-saml"]),
    ("ruby-vips", &["vips", "ruby-vips"]),
    ("sass-rails", &["sass-rails", "sass/rails"]),
    ("sassc", &["sassc"]),
    ("sassc-embedded", &["sassc-embedded"]),
    ("sassc-rails", &["sassc-rails", "sassc/rails"]),
    ("scout_apm", &["scout_apm"]),
    ("searchkick", &["searchkick"]),
    (
        "selenium-webdriver",
        &["selenium-webdriver", "selenium/webdriver"],
    ),
    ("sidekiq", &["sidekiq"]),
    ("sidekiq-cron", &["sidekiq-cron", "sidekiq/cron"]),
    ("simplecov", &["simplecov"]),
    ("simplecov-lcov", &["simplecov-lcov", "simplecov/lcov"]),
    ("skylight", &["skylight"]),
    ("solid_cable", &["solid_cable"]),
    ("solid_cache", &["solid_cache"]),
    ("solid_queue", &["solid_queue"]),
    ("spring", &["spring"]),
    (
        "spring-watcher-listen",
        &["spring-watcher-listen", "spring/watcher/listen"],
    ),
    ("sqlite3", &["sqlite3"]),
    ("stackprof", &["stackprof"]),
    ("stimulus-rails", &["stimulus-rails", "stimulus/rails"]),
    ("tailwindcss-rails", &["tailwindcss-rails"]),
    ("thin", &["thin"]),
    ("timecop", &["timecop"]),
    ("turbo-rails", &["turbo-rails", "turbo/rails"]),
    ("typhoeus", &["typhoeus"]),
    ("tzinfo-data", &["tzinfo/data", "tzinfo-data"]),
    ("unicorn", &["unicorn"]),
    ("vcr", &["vcr"]),
    ("view_component", &["view_component"]),
    ("webdrivers", &["webdrivers"]),
    ("webmock", &["webmock"]),
    ("will_paginate", &["will_paginate"]),
    ("activerecord-import", &["activerecord-import"]),
    ("cancancan", &["cancancan", "cancan"]),
];

pub const MODULE_NAMES: &[(&str, &[&str])] = &[
    (
        "active_model_serializers",
        &["ActiveModel::Serializer", "ActiveModelSerializers"],
    ),
    ("activerecord-import", &["ActiveRecord::Import"]),
    (
        "activerecord-session_store",
        &["ActiveRecord::SessionStore"],
    ),
    ("argon2", &["Argon2"]),
    ("aws-sdk-cloudwatch", &["Aws::CloudWatch"]),
    ("aws-sdk-core", &["Aws"]),
    ("aws-sdk-dynamodb", &["Aws::DynamoDB"]),
    ("aws-sdk-s3", &["Aws::S3"]),
    ("aws-sdk-sqs", &["Aws::SQS"]),
    ("azure-storage-blob", &["Azure::Storage::Blob"]),
    ("batch-loader", &["BatchLoader"]),
    ("bcrypt", &["BCrypt"]),
    ("bootsnap", &["Bootsnap"]),
    ("bullet", &["Bullet"]),
    ("cancancan", &["CanCanCan", "CanCan"]),
    ("concurrent-ruby", &["Concurrent"]),
    ("dalli", &["Dalli"]),
    ("database_cleaner", &["DatabaseCleaner"]),
    (
        "database_cleaner-active_record",
        &["DatabaseCleaner::ActiveRecord"],
    ),
    ("database_cleaner-redis", &["DatabaseCleaner::Redis"]),
    ("delayed_job", &["Delayed::Job", "DelayedJob"]),
    ("devise", &["Devise"]),
    ("doorkeeper", &["Doorkeeper"]),
    ("elasticsearch", &["Elasticsearch"]),
    ("factory_bot", &["FactoryBot"]),
    ("faraday-net_http", &["Faraday::NetHttp"]),
    ("faraday-retry", &["Faraday::Retry"]),
    ("fast_blank", &["FastBlank"]),
    ("ffi", &["FFI"]),
    ("goldiloader", &["Goldiloader"]),
    ("good_job", &["GoodJob"]),
    ("google-cloud-pubsub", &["Google::Cloud::PubSub"]),
    ("google-cloud-storage", &["Google::Cloud::Storage"]),
    ("google-protobuf", &["Google::Protobuf"]),
    ("graphql", &["GraphQL"]),
    ("graphql-batch", &["GraphQL::Batch"]),
    ("hiredis", &["Hiredis"]),
    ("http", &["HTTP"]),
    ("httparty", &["HTTParty"]),
    ("httpx", &["HTTPX"]),
    ("image_processing", &["ImageProcessing"]),
    ("importmap-rails", &["Importmap"]),
    ("jwt", &["JWT"]),
    ("kaminari", &["Kaminari"]),
    ("mail", &["Mail"]),
    ("mini_magick", &["MiniMagick"]),
    ("mini_portile2", &["MiniPortile"]),
    ("mysql2", &["Mysql2"]),
    ("net-http-persistent", &["Net::HTTP::Persistent"]),
    ("nokogiri", &["Nokogiri"]),
    ("omniauth", &["OmniAuth"]),
    ("omniauth-oauth2", &["OmniAuth::Strategies::OAuth2"]),
    ("pagy", &["Pagy"]),
    ("parallel", &["Parallel"]),
    ("parallel_tests", &["ParallelTests"]),
    ("pg", &["PG"]),
    ("pry-byebug", &["Byebug"]),
    ("pundit", &["Pundit"]),
    ("rack-attack", &["Rack::Attack"]),
    ("rack-cors", &["Rack::Cors"]),
    ("rack-mini-profiler", &["Rack::MiniProfiler"]),
    ("rack-protection", &["Rack::Protection"]),
    ("rack-proxy", &["Rack::Proxy"]),
    ("ransack", &["Ransack"]),
    ("redis", &["Redis"]),
    ("resque", &["Resque"]),
    ("rest-client", &["RestClient"]),
    ("rspec-core", &["RSpec::Core"]),
    ("rspec-expectations", &["RSpec::Expectations"]),
    ("rspec-mocks", &["RSpec::Mocks"]),
    ("rspec-rails", &["RSpec::Rails"]),
    ("rubocop-ast", &["RuboCop::AST"]),
    ("rubocop-performance", &["RuboCop::Performance"]),
    ("rubocop-rails", &["RuboCop::Rails"]),
    ("rubocop-rspec", &["RuboCop::RSpec"]),
    ("ruby-vips", &["Vips"]),
    ("rubyzip", &["Zip"]),
    ("sass-rails", &["Sass::Rails"]),
    ("sassc", &["SassC"]),
    ("sassc-embedded", &["SassC"]),
    ("sassc-rails", &["SassC::Rails"]),
    ("searchkick", &["Searchkick"]),
    ("shoulda-context", &["Shoulda::Context"]),
    ("shoulda-matchers", &["Shoulda::Matchers"]),
    ("sidekiq", &["Sidekiq"]),
    ("sidekiq-cron", &["Sidekiq::Cron", "Sidekiq"]),
    ("solid_cable", &["SolidCable"]),
    ("solid_cache", &["SolidCache"]),
    ("solid_queue", &["SolidQueue"]),
    ("sqlite3", &["SQLite3"]),
    ("stimulus-rails", &["Stimulus"]),
    ("tilt", &["Tilt"]),
    ("trilogy", &["Trilogy"]),
    ("turbo-rails", &["Turbo"]),
    ("tzinfo", &["TZInfo"]),
    ("tzinfo-data", &["TZInfo::Data", "TZInfo"]),
    ("view_component", &["ViewComponent"]),
    ("will_paginate", &["WillPaginate"]),
];

pub fn require_paths_for(gem_name: &str) -> Option<&'static [&'static str]> {
    REQUIRE_PATHS
        .iter()
        .find_map(|(name, values)| (*name == gem_name).then_some(*values))
}

pub fn module_names_for(gem_name: &str) -> Option<&'static [&'static str]> {
    MODULE_NAMES
        .iter()
        .find_map(|(name, values)| (*name == gem_name).then_some(*values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn require_path_table_has_unique_nonempty_entries() {
        assert_valid_table(REQUIRE_PATHS);
        assert_unique_keys(REQUIRE_PATHS);
        for (gem, paths) in REQUIRE_PATHS {
            for path in *paths {
                assert!(
                    !path.chars().any(char::is_whitespace),
                    "require path for {gem} contains whitespace: {path}"
                );
            }
        }
    }

    #[test]
    fn module_name_table_has_unique_nonempty_entries() {
        assert_valid_table(MODULE_NAMES);
        assert_unique_keys(MODULE_NAMES);
        for (gem, names) in MODULE_NAMES {
            for name in *names {
                for segment in name.split("::") {
                    let first = segment
                        .chars()
                        .next()
                        .unwrap_or_else(|| panic!("empty module segment for {gem}: {name}"));
                    assert!(
                        first.is_ascii_uppercase(),
                        "module name for {gem} has invalid segment: {name}"
                    );
                }
            }
        }
    }

    fn assert_valid_table(table: &[(&str, &[&str])]) {
        assert!(!table.is_empty());
        for (key, values) in table {
            assert!(!key.is_empty(), "empty known gem key");
            assert!(!values.is_empty(), "empty mapping for {key}");
            for value in *values {
                assert!(!value.is_empty(), "empty mapping value for {key}");
            }
        }
    }

    fn assert_unique_keys(table: &[(&str, &[&str])]) {
        let mut keys = HashSet::new();
        for (key, _) in table {
            assert!(keys.insert(*key), "duplicate known gem key: {key}");
        }
    }
}
