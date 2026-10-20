# Fractal boot regions against real Active Record, driven by
# tests/fractal_runtime.rs.
#
# FRACTAL_SOURCE is a small app whose config/environment.rb stands in for
# a Rails boot (an environment, Zeitwerk-shaped on_load and to_prepare,
# and Rails' production step of defining every model's attribute methods
# once the app has loaded). FRACTAL_OUT is the same app with a
# `--target fractal --boot production` overlay placed on its root.
#
# Run with FRACTAL_BOOT_DUMP=1 in an app root, this file boots that app
# and prints what the boot left behind instead: the description the two
# trees must agree on.

require "json"

def boot_state
  require File.expand_path("config/environment", Dir.pwd)
  ActiveRecord::Base.descendants.sort_by(&:name).to_h do |model|
    owner = model.send(:generated_attribute_methods)
    generated = (owner.instance_methods(false) + owner.private_instance_methods(false)).sort.map do |name|
      method = owner.instance_method(name)
      [name, method.parameters, method.arity, method.original_name, method.owner == owner]
    end
    [model.name, {
      generated: generated,
      public: model.public_instance_methods.sort,
      private: model.private_instance_methods.sort,
      types: model.attribute_types.transform_values { |type| [type.class.name, type.type] },
      columns: model.column_names,
      aliases: model.attribute_aliases,
      # Rails' lookups fill this default-proc Hash with an empty list per
      # attribute they read; an empty list and a missing key say the same.
      aliases_by_attribute: model.send(:aliases_by_attribute_name).reject { |_, names| names.empty? },
      ivars: model.instance_variables.sort,
      ancestors: model.ancestors.map { |a| a.name || "anonymous" },
      flags: %i[@attribute_methods_generated @alias_attributes_mass_generated @schema_loaded].to_h { |f| [f, model.instance_variable_get(f)] },
    }]
  end
end

if ENV["FRACTAL_BOOT_DUMP"] == "1"
  print JSON.generate(boot_state)
  exit
end

require "minitest/autorun"
require "open3"
require "rbconfig"

SOURCE = ENV.fetch("FRACTAL_SOURCE")
OUT = ENV.fetch("FRACTAL_OUT")
BODIES = "config/fractal/boot/attribute_methods.rb"

def probe(dir, env, script)
  stdout, stderr, status = Open3.capture3(env, RbConfig.ruby, "-e", script, chdir: dir)
  raise "boot in #{dir} failed:\n#{stderr}" unless status.success?

  JSON.parse(stdout)
end

def dump(dir, env = {})
  stdout, stderr, status = Open3.capture3({ "FRACTAL_BOOT_DUMP" => "1", "RAILS_ENV" => "production" }.merge(env), RbConfig.ruby, __FILE__, chdir: dir)
  raise "boot in #{dir} failed:\n#{stderr}" unless status.success?

  stdout
end

PROBE = <<~RUBY
  require File.expand_path("config/environment", Dir.pwd)
  located = ->(klass, name) { klass.send(:generated_attribute_methods).instance_method(name).source_location&.first.to_s.delete_prefix(Dir.pwd + "/") }
  article = Article.new(title: "first", rating: "4", body: "hello")
  Widget.new # Rails defines a lazy model's attribute methods on first use
  result = {
    bound: defined?(Fractal::Boot) ? Fractal::Boot.bound.keys.sort : [],
    title_changed: located.(Article, :title_changed?),
    headline: located.(Article, :headline),
    widget: located.(Widget, :name_changed?),
    behaviour: [article.title_changed?, article.headline, article.rating, article.saved_change_to_title?, Comment.new(body: "quiet").body],
  }
  if ENV["RELOAD"] == "1"
    old = Article
    Object.send(:remove_const, :Article)
    Rails.autoload_model("Article")
    result[:reloaded] = [!old.equal?(Article), located.(Article, :title_changed?), Article.new(title: "x").title_changed?]
  end
  print JSON.generate(result)
RUBY

class FractalBootTest < Minitest::Test
  def test_the_overlay_leaves_the_boot_state_rails_builds
    assert_equal dump(SOURCE), dump(OUT)
  end

  def test_regions_are_bound_from_the_overlay_and_behave_as_rails_methods
    source = probe(SOURCE, { "RAILS_ENV" => "production" }, PROBE)
    out = probe(OUT, { "RAILS_ENV" => "production" }, PROBE)
    assert_equal [], source["bound"]
    assert_equal %w[Article Comment], out["bound"]
    refute_equal BODIES, source["title_changed"]
    assert_equal BODIES, out["title_changed"]
    assert_equal BODIES, out["headline"], "alias attribute methods are part of the region"
    refute_equal BODIES, out["widget"], "an unselected model keeps Rails' methods"
    assert_equal [true, "first", 4, false, "QUIET"], out["behaviour"]
    assert_equal source["behaviour"], out["behaviour"]
  end

  def test_a_reload_binds_the_region_on_the_new_class
    out = probe(OUT, { "RAILS_ENV" => "production", "RELOAD" => "1" }, PROBE)
    assert_equal [true, BODIES, true], out["reloaded"]
  end

  def test_other_environments_and_the_off_switch_leave_rails_to_it
    assert_equal [], probe(OUT, { "RAILS_ENV" => "development" }, PROBE)["bound"]
    assert_equal [], probe(OUT, { "RAILS_ENV" => "production", "FRACTAL_BOOT" => "0" }, PROBE)["bound"]
    forced = probe(OUT, { "RAILS_ENV" => "development", "FRACTAL_BOOT" => "1" }, PROBE)
    assert_equal %w[Article Comment], forced["bound"]
    assert_equal [true, "first", 4, false, "QUIET"], forced["behaviour"]
  end
end
