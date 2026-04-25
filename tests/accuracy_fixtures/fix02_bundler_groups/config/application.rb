# Rails-style boot. The Bundler.require line pulls in *all* groups, so
# dev/test gems should NOT be flagged as unused even with no direct require.
require "bundler/setup"
Bundler.require(*Rails.groups)

module MyApp
  class Application < Rails::Application
  end
end
