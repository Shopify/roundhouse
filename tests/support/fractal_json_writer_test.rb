# The json-writer pass against real Active Support, driven by
# tests/fractal_json_writer.rs.
#
# FRACTAL_APP is the app in that test with its fractal overlay placed on
# its root. Run with no FRACTAL_JSON_MODE, this file is the driver: it
# runs itself once per mode in a fresh process, so the original and the
# variants never share live state, then compares what the runs recorded.
#
#   original   the app's own methods (the initializer is not loaded)
#   fractal    the variants installed
#
# Every probe passes values of the declared types: the writer takes the
# app's RBS and Sorbet types as correct.
#
# Each run calls every probe and records the bytes, encoding and frozen
# state of the result, or the exception's class, message and line in the
# method, plus the values' side effects, any warnings, and how many
# times `Hash#to_json` ran: zero is the writer, one is the original
# expression.

require "rbconfig"

APP = ENV.fetch("FRACTAL_APP")
MODE = ENV["FRACTAL_JSON_MODE"]

begin
  require "active_support"
  require "active_support/json"
rescue LoadError
  puts "SKIP: activesupport is not installed"
  exit 0
end

def path(file) = File.join(APP, file)

# The probe inputs the writer reproduces; every other one must take the
# original expression.
FAST_STRINGS = %i[plain empty utf8 slash del us_ascii binary_ascii sjis_ascii frozen near_sep].freeze
FAST_INTEGERS = %i[zero negative big].freeze

if MODE
  require "pathname"

  # Stands in for Rails, as tests/support/fractal_runtime_test.rb does.
  module Rails
    Loader = Struct.new(:callbacks) do
      def on_load(cpath, &block)
        (callbacks[cpath] ||= []) << block
      end
    end
    Config = Struct.new(:prepares) do
      def to_prepare(&block)
        prepares << block
      end
    end
    Application = Struct.new(:root, :config)
    Autoloaders = Struct.new(:main)

    def self.application
      @application ||= Application.new(Pathname.new(APP), Config.new([]))
    end

    def self.autoloaders
      @autoloaders ||= Autoloaders.new(Loader.new({}))
    end
  end

  # Enough of sorbet-runtime for `extend T::Sig` and `sig { ... }`.
  module T
    module Sig
      def sig(*) = nil
    end
  end

  load path("config/initializers/fractal.rb") unless MODE == "original"

  CLASSES = [%w[Wire app/models/wire.rb], %w[Loose app/models/loose.rb], %w[Typed app/models/typed.rb]].freeze
  CLASSES.each do |cpath, file|
    load path(file)
    klass = Object.const_get(cpath)
    Rails.autoloaders.main.callbacks.fetch(cpath, []).each { |callback| callback.call(klass, path(file)) }
  end

  # Count after the variants install.
  $to_json_calls = 0
  Hash.prepend(Module.new do
    def to_json(*args)
      $to_json_calls += 1
      super
    end
  end)

  class SubString < String; end

  class CustomString < String
    def to_json(*) = %("custom")
  end

  STRINGS = {
    plain: "Quarterly report", empty: "", utf8: "Grüße, 日本", slash: "a/b", del: "a\u007fb",
    us_ascii: "plain".encode("US-ASCII"), binary_ascii: "plain".b, sjis_ascii: "plain".encode("Shift_JIS"),
    frozen: "frozen".freeze,
    # U+2027 and U+202A share their first two UTF-8 bytes with U+2028
    # and U+2029, and need no escaping.
    near_sep: "a\u2027\u202ab",
    # Every one of these takes the original expression.
    quote: %(say "hi"), backslash: "a\\b", newline: "a\nb", tab: "a\tb", control: "a\u0001b",
    lt: "a<b", gt: "a>b", amp: "a&b", line_sep: "a\u2028b", para_sep: "a\u2029b",
    binary_utf8: "Grüße".b, invalid: "a\xFFb", sjis: "日本".encode("Shift_JIS"),
    latin1: "Grüße".encode("ISO-8859-1"), utf16: "plain".encode("UTF-16LE"),
    subclass: SubString.new("sub"), custom: CustomString.new("mine"),
  }.freeze

  INTEGERS = { zero: 0, negative: -7, big: 2**70 }.freeze

  def record(name)
    $to_json_calls = 0
    log = []
    warnings = StringIO.new
    stderr, $stderr = $stderr, warnings
    begin
      value = yield(log)
      result = { ok: true, bytes: value.b, encoding: value.encoding.name, frozen: value.frozen? }
    rescue StandardError => e
      here = e.backtrace_locations&.find { |l| l.path.start_with?(APP) && !l.label.to_s.end_with?("#note") }
      result = { ok: false, error: e.class.name, message: e.message, line: here && [File.basename(here.path), here.lineno, here.label] }
    ensure
      $stderr = stderr
    end
    result.merge(name: name, log: log, warnings: warnings.string, to_json_calls: $to_json_calls)
  end

  require "stringio"
  records = []
  STRINGS.each { |key, value| records << record("pair string #{key}") { Wire.new.pair(value, 1) } }
  INTEGERS.each { |key, value| records << record("pair integer #{key}") { Wire.new.pair("x", value) } }
  [nil, "t", "a<b"].product([nil, true, false]).each do |title, flag|
    records << record("local #{title.inspect} #{flag.inspect}") { Wire.new.local(title, flag) }
  end
  records << record("noted fast") { |log| Wire.new(log).noted("a", "b") }
  records << record("noted fallback") { |log| Wire.new(log).noted("a<", "b") }
  records << record("noted raises") { |log| Wire.new(log).noted("a", "boom") }
  records << record("noted invalid") { |log| Wire.new(log).noted("a", "\xFF") }
  records << record("local invalid") { Wire.new.local("\xFF", nil) }
  records << record("fixed") { Wire.new.fixed }
  records << record("reveal") { Wire.new.reveal(5) }
  records << record("loose fixed") { Loose.new.fixed }
  records << record("loose pair") { Loose.new.pair("loose") }
  records << record("endless") { Wire.new.endless(3) }
  records << record("nested") { Wire.new.nested("in") }
  records << record("typed fast") { Typed.new("Title", 3, true).to_wire }
  records << record("typed escapes") { Typed.new("Fish & chips", 3, false).to_wire }

  first, second = Loose.new.fixed, Loose.new.fixed
  facts = {
    status: defined?(Fractal) ? Fractal.status : {},
    fresh_strings: !first.equal?(second),
    secret_private: Wire.private_method_defined?(:secret),
    locations: %i[pair local noted fixed nested secret endless].to_h { |m| [m, Wire.instance_method(m).source_location] },
    json_encoder: ActiveSupport.json_encoder.name,
    # After every probe, non-ASCII ones included: still one binary form.
    escape_binary: defined?(Fractal) &&
      Fractal::JsonWriter::ESCAPE.fixed_encoding? && Fractal::JsonWriter::ESCAPE.encoding == Encoding::BINARY,
  }
  File.binwrite(ENV.fetch("FRACTAL_JSON_OUT"), Marshal.dump([records, facts]))
  exit 0
end

require "minitest/autorun"
require "tmpdir"

RUNS = Dir.mktmpdir("fractal-json") do |dir|
  %w[original fractal].to_h do |mode|
    out = File.join(dir, "#{mode}.bin")
    env = { "FRACTAL_JSON_MODE" => mode, "FRACTAL_JSON_OUT" => out }
    system(env, RbConfig.ruby, __FILE__, exception: true)
    [mode, Marshal.load(File.binread(out))]
  end
end

class JsonWriterTest < Minitest::Test
  def records(mode) = RUNS.fetch(mode).first
  def facts(mode) = RUNS.fetch(mode).last
  def comparable(record) = record.reject { |key, _| key == :to_json_calls }

  def test_every_variant_installed
    statuses = facts("fractal")[:status]
    assert_equal 10, statuses.size, statuses.keys.inspect
    statuses.each { |key, status| assert_equal [:installed, nil], status, key }
  end

  def test_the_variants_keep_owner_file_line_and_visibility
    assert_equal facts("original")[:locations], facts("fractal")[:locations]
    assert facts("fractal")[:secret_private]
  end

  def test_every_probe_gives_the_original_bytes_exceptions_effects_and_warnings
    original, fractal = records("original"), records("fractal")
    assert_equal original.map { |r| r[:name] }, fractal.map { |r| r[:name] }
    original.zip(fractal).each { |o, f| assert_equal comparable(o), comparable(f), o[:name] }
  end

  def test_the_writer_ran_for_exactly_the_values_it_reproduces
    expected = records("fractal").to_h do |r|
      fast = case r[:name]
             when /\Apair string (\w+)/ then FAST_STRINGS.include?(Regexp.last_match(1).to_sym)
             when /\Apair integer (\w+)/ then FAST_INTEGERS.include?(Regexp.last_match(1).to_sym)
             when /\Alocal (nil|"t") / then true
             when "noted fast", "fixed", "reveal", "loose fixed", "loose pair", "endless", "typed fast" then true
             # A value raised, so neither version reached the encoding.
             when "noted raises" then true
             else false
             end
      # The inner `to_json` is the app's, and its quotes send the outer
      # one to the original expression.
      next [r[:name], 2] if r[:name] == "nested"
      next [r[:name], 0] if r[:name] == "noted raises"

      [r[:name], fast ? 0 : 1]
    end
    assert_equal expected, records("fractal").to_h { |r| [r[:name], r[:to_json_calls]] }
    original = records("original").to_h { |r| [r[:name], r[:to_json_calls]] }
    assert_equal({ "noted raises" => 0, "nested" => 2 }, original.reject { |_, calls| calls == 1 })
  end

  # A regexp whose encoding is not fixed is recompiled for the first
  # non-ASCII string it sees and keeps that slower form for every later
  # string, so the check's must stay binary through every probe.
  def test_the_escape_regexp_keeps_its_binary_form
    assert facts("fractal")[:escape_binary]
  end

  def test_the_writer_returns_a_new_unfrozen_string_every_call
    assert facts("fractal")[:fresh_strings]
    assert facts("original")[:fresh_strings]
    records("fractal").select { |r| r[:ok] }.each { |r| refute r[:frozen], r[:name] }
  end

  def test_side_effects_run_once_in_order_and_an_exception_keeps_its_line
    raised = records("fractal").find { |r| r[:name] == "noted raises" }
    assert_equal %w[a boom], raised[:log]
    assert_equal "ArgumentError", raised[:error]
    assert_equal records("original").find { |r| r[:name] == "noted raises" }[:line], raised[:line]
  end
end
