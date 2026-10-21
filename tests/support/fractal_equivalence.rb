# Runs one app tree against real Active Record and prints what its
# models answer, for tests/fractal_runtime.rs to compare between an app
# and its fractal build.
#
#   ruby fractal_equivalence.rb APP_ROOT
#
# Loads APP_ROOT/db/schema.rb into an in-memory SQLite database, loads
# every model (ApplicationRecord first), then evaluates
# APP_ROOT/probe.rb, which returns the value to print as JSON. Exits 2
# when activerecord or sqlite3 is not installed, so the caller can skip.

begin
  require "active_record"
  require "sqlite3"
rescue LoadError => e
  warn "skip: #{e.message}"
  exit 2
end
require "json"

app = ARGV.fetch(0)
ActiveRecord::Base.establish_connection(adapter: "sqlite3", database: ":memory:")
ActiveRecord::Schema.verbose = false
load File.join(app, "db/schema.rb")
models = Dir[File.join(app, "app/models/**/*.rb")].sort_by { |f| [f.end_with?("/application_record.rb") ? 0 : 1, f] }
models.each { |f| load f }
puts JSON.generate(eval(File.read(File.join(app, "probe.rb")), TOPLEVEL_BINDING, File.join(app, "probe.rb")))
