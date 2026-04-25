require "bundler/setup"

bundler_groups = [:default]
bundler_groups = bundler_groups.concat(Rails.groups(assets: %w[development test]))
Bundler.require(*bundler_groups)

module MyApp
  class Application < Rails::Application
  end
end
