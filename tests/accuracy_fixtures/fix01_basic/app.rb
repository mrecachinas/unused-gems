require "json"
require "fileutils"

FileUtils.mkdir_p("/tmp/example")
puts JSON.dump({ ok: true })
