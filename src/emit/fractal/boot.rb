# frozen_string_literal: true

# Fractal boot regions, written by `roundhouse --target fractal --boot ENV`.
#
# A boot region is the work Rails does for a class while the app boots in
# ENV, decided once at build time from a boot of this app and written
# out as plain Ruby under config/fractal/boot/. Today a region holds an
# Active Record model's attribute methods: the bodies Active Support's
# CodeGenerator would compile from strings, kept once each in
# attribute_methods.rb, and for each model the names it binds to them.
# A region is bound when its class loads (every load, so a reload gets it
# again), so the boot compiles none of those methods.
#
# Nothing in a region is checked when it is bound. The build verified the
# overlay against the app it was built from, so the overlay belongs to that
# build of the app, the way compiled output does. The checks below run once
# per boot, not once per class: the regions apply only in the environment
# and Active Record version they were built for, and otherwise Rails does
# its own work as usual.
#
#   FRACTAL_BOOT=0          # leave every class to Rails
#   FRACTAL_BOOT=1          # bind the regions in any environment (for the app's suite)
#   Fractal::Boot.bound     # { class name => true } for the regions bound so far

module Fractal
  module Boot
    DIR = File.expand_path("../fractal/boot", __dir__)

    @bound = {}
    @classes = {}.compare_by_identity

    class << self
      attr_reader :bound, :env, :active_record, :regions

      def configure(env:, active_record:, regions:)
        @env = env
        @active_record = active_record
        @regions = regions
      end

      def active?
        case ENV["FRACTAL_BOOT"]
        when "0" then false
        when "1" then true
        else Rails.env.to_s == @env && ActiveRecord.version.to_s == @active_record
        end
      end

      # Bind each region when its class loads, and on each prepare for a
      # class that was loaded before this initializer ran.
      def attach(app)
        return unless active?

        @regions.each_key do |name|
          Rails.autoloaders.main.on_load(name) { |klass, _path| bind_region(klass, name) }
        end
        app.config.to_prepare { Fractal::Boot.bind_loaded }
      end

      def bind_loaded
        @regions.each_key do |name|
          klass = loaded(name)
          bind_region(klass, name) if klass
        end
      end

      # A model whose attribute methods are already generated keeps them,
      # as Rails' own define_attribute_methods does.
      def bind_region(klass, name)
        return if @classes[klass] || klass.instance_variable_get(:@attribute_methods_generated)

        @classes[klass] = true
        require File.join(DIR, "attribute_methods")
        load File.join(DIR, "classes", @regions.fetch(name))
        @bound[name] = true
      end

      # Called by each region file: define `pairs` (name, canonical name,
      # ...) on the class's generated attribute methods module from the
      # bodies in `bodies`, as CodeGenerator#apply does once it has
      # compiled them.
      def bind(owner, bodies, pairs)
        i = 0
        while i < pairs.size
          owner.define_method(pairs[i], bodies.instance_method(pairs[i + 1]))
          i += 2
        end
      end

      private

      def loaded(name)
        name.split("::").reduce(Object) do |namespace, part|
          return nil unless namespace.const_defined?(part, false) && !namespace.autoload?(part)

          namespace.const_get(part, false)
        end
      end
    end
  end
end
