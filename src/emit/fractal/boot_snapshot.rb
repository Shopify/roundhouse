# frozen_string_literal: true

# Fractal boot snapshot, run by `roundhouse --target fractal --boot ENV`.
#
#   ruby boot_snapshot.rb OUT.json     (cwd: the app root, RAILS_ENV=ENV)
#
# Boots the app the way it boots in ENV and records, for each class named
# in FRACTAL_BOOT_CLASSES, the attribute methods Active Record generated
# for it during that boot: every method Active Support's CodeGenerator
# compiled from a string into the class's generated attribute methods
# module, with the source it compiled. The build turns that into a boot
# region: the same methods as plain `def`s, bound when the class loads,
# so a boot in ENV compiles none of them.
#
# This file only observes. A class whose methods it cannot account for in
# full is recorded with the reason, and gets no region.

require "json"

root = Dir.pwd
out = ARGV.fetch(0)
classes = ENV.fetch("FRACTAL_BOOT_CLASSES", "").split(",")

boot = File.join(root, "config/boot.rb")
require boot if File.exist?(boot)
require "active_support"
require "active_support/code_generator"

module FractalBootSnapshot
  # [cache module, canonical name] => the source CodeGenerator compiled.
  BODIES = {}.compare_by_identity
  # owner module => { name => [cache module, canonical name] }
  BINDINGS = Hash.new { |h, k| h[k] = {} }.compare_by_identity
  # owner module => [source compiled straight into it]
  DIRECT = Hash.new { |h, k| h[k] = [] }.compare_by_identity
  # model => [[new name, old name]] aliased while Rails defined its
  # attribute methods (Active Record adds `id_value` there), as opposed to
  # in the class body, which runs again on every load anyway.
  ALIASES = Hash.new { |h, k| h[k] = [] }.compare_by_identity
  DEFINING = []

  module Recorder
    def define_attribute_methods(*)
      DEFINING.push(self)
      super
    ensure
      DEFINING.pop
    end

    def alias_attribute(new_name, old_name)
      ALIASES[self] << [new_name.to_s, old_name.to_s] if DEFINING.last.equal?(self)
      super
    end
  end
end

ActiveSupport.on_load(:active_record) { singleton_class.prepend(FractalBootSnapshot::Recorder) }

ActiveSupport::CodeGenerator::MethodSet.prepend(Module.new do
  def define_cached_method(canonical_name, as: nil)
    super(canonical_name, as: as) do |sources|
      start = sources.size
      yield sources
      (FractalBootSnapshot::BODIES[@cache] ||= {})[canonical_name.to_sym] = sources[start..].join(";")
    end
  end

  def apply(owner, path, line)
    @methods.each { |as, canonical| FractalBootSnapshot::BINDINGS[owner][as] = [@cache, canonical] }
    super
  end
end)

ActiveSupport::CodeGenerator.prepend(Module.new do
  def execute
    FractalBootSnapshot::DIRECT[@owner] << @sources.join(";") unless @sources.empty?
    super
  end
end)

require File.join(root, "config/environment")

namespaces = ActiveSupport::CodeGenerator::MethodSet::METHOD_CACHES
DEFINABLE = /\A[a-zA-Z_][a-zA-Z0-9_]*[?!=]?\z/
FLAGS = %i[@attribute_methods_generated @alias_attributes_mass_generated].freeze

def loaded(name)
  name.split("::").reduce(Object) do |namespace, part|
    return nil unless namespace.const_defined?(part, false) && !namespace.autoload?(part)

    namespace.const_get(part, false)
  end
end

result = {
  env: (defined?(Rails) && Rails.respond_to?(:env)) ? Rails.env.to_s : ENV["RAILS_ENV"].to_s,
  active_record: defined?(ActiveRecord) ? ActiveRecord.version.to_s : nil,
  ruby: RUBY_VERSION,
  classes: {},
  bodies: {},
}

used = Hash.new { |h, k| h[k] = {} }
classes.each do |name|
  entry = result[:classes][name] = {}
  decline = ->(reason) { entry[:declined] = reason }
  klass = loaded(name)
  next decline.("not loaded by the end of the boot") unless klass
  next decline.("not an Active Record model") unless defined?(ActiveRecord::Base) && klass.is_a?(Class) && klass < ActiveRecord::Base
  next decline.("an abstract class") if klass.abstract_class?

  file, = Object.const_source_location(name)
  next decline.("defined outside the app") unless file&.start_with?(File.join(root, ""))

  entry[:file] = file.delete_prefix(File.join(root, ""))
  next decline.("Rails did not generate its attribute methods during the boot") unless klass.instance_variable_get(:@attribute_methods_generated)

  owner = klass.send(:generated_attribute_methods)
  unless FractalBootSnapshot::DIRECT[owner].empty?
    next decline.("Rails compiled source directly into its generated attribute methods module")
  end

  bindings = FractalBootSnapshot::BINDINGS[owner]
  defined = owner.instance_methods(false) + owner.private_instance_methods(false)
  unaccounted = defined - bindings.keys
  next decline.("#{unaccounted.size} generated method(s) were not compiled by CodeGenerator (#{unaccounted.first})") unless unaccounted.empty?

  methods = []
  sources = []
  missing = nil
  bindings.sort_by { |as, _| as.to_s }.each do |as, (cache, canonical)|
    source = FractalBootSnapshot::BODIES.fetch(cache, {})[canonical]
    break missing = "no source was recorded for #{as}" unless source
    break missing = "#{canonical} cannot be written as a def" unless DEFINABLE.match?(canonical.to_s)

    namespace = namespaces.key(cache).to_s
    sources << [namespace, canonical.to_s, source]
    methods << [as.to_s, namespace, canonical.to_s]
  end
  next decline.(missing) if missing

  sources.each { |namespace, canonical, source| used[namespace][canonical] = source }
  entry[:methods] = methods
  entry[:aliases] = FractalBootSnapshot::ALIASES[klass]
  entry[:schema_loaded] = klass.instance_variable_get(:@schema_loaded) == true
  entry[:flags] = FLAGS.select { |flag| klass.instance_variable_get(flag) == true }.map(&:to_s)
end
result[:bodies] = used.sort.to_h { |namespace, bodies| [namespace, bodies.sort.to_h] }

File.write(out, JSON.generate(result))
