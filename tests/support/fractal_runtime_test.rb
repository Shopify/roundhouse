# The fractal query runtime (`src/emit/fractal/fractal.rb`)
# against real Active Record on in-memory SQLite. Each test drives a
# specialized site the way generated code does, `Fractal.records(key,
# binds) { original }`, and checks two things: the answer is the original
# chain's answer, and the fast path ran (or, where the runtime must step
# aside, that it fell back and said why).
#
# Run by `tests/fractal_runtime.rs`; directly:
#   ruby tests/support/fractal_runtime_test.rb src/emit/fractal/fractal.rb
require "active_record"
require "minitest/autorun"

RUNTIME = ARGV.shift || File.expand_path("../../src/emit/fractal/fractal.rb", __dir__)

ActiveRecord::Base.establish_connection(adapter: "sqlite3", database: ":memory:")
ActiveRecord::Schema.verbose = false
ActiveRecord::Schema.define do
  create_table(:accounts) { |t| t.string :name }
  create_table(:rooms) { |t| t.string :name }
  create_table(:messages) { |t| t.integer :room_id; t.string :body }
end

class ApplicationRecord < ActiveRecord::Base
  self.abstract_class = true
end

load RUNTIME

class FractalRuntimeTest < Minitest::Test
  def setup
    ActiveRecord::Base.connection.begin_transaction(joinable: false)
    define_models
    Fractal.reset_counts
  end

  def teardown
    ActiveRecord::Base.connection.rollback_transaction
    %i[Account Room Message].each { |c| Object.send(:remove_const, c) if Object.const_defined?(c, false) }
  end

  # What `Rails.application.config.to_prepare` does on boot and after every
  # reload: fresh model classes, then the defines naming them.
  def define_models
    Fractal.prepare
    Object.const_set(:Account, Class.new(ApplicationRecord))
    Object.const_set(:Message, Class.new(ApplicationRecord))
    Object.const_set(:Room, Class.new(ApplicationRecord) { has_many :messages })
    Fractal.define(:account_first, Account, one: true, via: :first) { |p| Account.all.send(:ordered_relation).limit(1) }
    Fractal.define(:account_named, Account, one: true, via: :find_by) { |p| Account.all.where(name: p.bind).limit(1) }
    Fractal.define(:room_messages, Message) { |p| Message.all.where(room_id: p.bind).order(:id) }
  end

  def account_first = Fractal.records(:account_first, []) { Account.first }
  def account_named(name) = Fractal.records(:account_named, [name]) { Account.find_by(name: name) }

  def outcomes(key) = Fractal.counts[key]

  def test_empty_table_answers_nil_on_the_fast_path
    assert_nil account_first
    assert_equal({ fast: 1 }, outcomes(:account_first))
  end

  def test_first_follows_rails_implicit_order
    Account.create!(name: "b")
    Account.create!(name: "a")
    assert_equal Account.first, account_first
    assert_equal "b", account_first.name
    assert_equal 2, Fractal.fast_path_count(:account_first)
  end

  def test_a_request_value_is_a_bind
    Account.create!(name: "a")
    b = Account.create!(name: "b")
    assert_equal b, account_named("b")
    assert_nil account_named("c")
    assert_equal({ fast: 2 }, outcomes(:account_named))
  end

  def test_a_value_statement_cache_refuses_falls_back
    Account.create!(name: nil)
    assert_equal Account.find_by(name: nil), account_named(nil)
    assert_equal({ unsupported_value: 1 }, outcomes(:account_named))
  end

  def test_rows_written_in_an_open_transaction_are_visible_and_roll_back
    ActiveRecord::Base.transaction(requires_new: true) do
      account = Account.create!(name: "inside")
      assert_equal account, account_first
      raise ActiveRecord::Rollback
    end
    assert_nil account_first
    assert_equal({ fast: 2 }, outcomes(:account_first))
  end

  # Rule: remember who's asking. A statement built while one caller had a
  # scope open must not answer for another caller, and a caller with a
  # scope open must get its scope.
  def test_a_scoping_block_takes_the_original_and_does_not_poison_the_statement
    Account.create!(name: "a")
    b = Account.create!(name: "b")
    Account.where(name: "b").scoping do
      assert_equal b, account_first
    end
    assert_equal "a", account_first.name
    Account.unscoped { assert_equal "a", account_first.name }
    assert_equal({ scoped: 2, fast: 1 }, outcomes(:account_first))
  end

  def test_an_association_scope_takes_the_original
    room = Room.create!(name: "r")
    room.messages.create!(body: "hi")
    msgs = Message.where(room_id: room.id).scoping do
      Fractal.records(:room_messages, [room.id]) { Message.where(room_id: room.id).order(:id).to_a }
    end
    assert_equal room.messages.to_a, msgs
    assert_equal({ scoped: 1 }, outcomes(:room_messages))
  end

  # Rule: notice reloads.
  def test_after_a_reload_the_new_class_answers
    Account.create!(name: "a")
    old = Account
    assert_instance_of old, account_first

    Object.send(:remove_const, :Account)
    Object.const_set(:Account, Class.new(ApplicationRecord))
    # Before to_prepare runs again, the statement still names the old class.
    assert_instance_of Account, account_first
    assert_equal({ fast: 1, reloaded: 1 }, outcomes(:account_first))

    Fractal.define(:account_first, Account, one: true, via: :first) { |p| Account.all.send(:ordered_relation).limit(1) }
    refute_same old, Account
    assert_instance_of Account, account_first
    assert_equal({ fast: 2, reloaded: 1 }, outcomes(:account_first))
  end

  # Rule: don't skip hooks.
  def test_an_app_defined_finder_is_not_bypassed
    Account.create!(name: "a")
    Account.define_singleton_method(:first) { |*| :the_apps_own_first }
    assert_equal :the_apps_own_first, account_first
    assert_equal({ uncompilable: 1 }, outcomes(:account_first))
  end

  def test_a_finder_a_module_mixes_in_is_not_bypassed
    calls = []
    Account.singleton_class.prepend(Module.new do
      define_method(:find_by) { |*args, **kw| calls << kw; super(*args, **kw) }
    end)
    a = Account.create!(name: "a")
    assert_equal a, account_named("a")
    assert_equal [{ name: "a" }], calls
    assert_equal({ uncompilable: 1 }, outcomes(:account_named))
  end

  def test_an_sti_subclass_reads_its_parents_association
    Object.const_set(:OpenRoom, Class.new(Room))
    room = OpenRoom.create!(name: "r")
    message = room.messages.create!(body: "hi")
    got = Fractal.records(:room_messages, [room.id], owner: room, association: :messages) { room.messages.order(:id).to_a }
    assert_equal [message], got
    assert_equal({ fast: 1 }, outcomes(:room_messages))
  ensure
    Object.send(:remove_const, :OpenRoom) if Object.const_defined?(:OpenRoom, false)
  end

  def test_a_redefined_association_reader_is_not_bypassed
    room = Room.create!(name: "r")
    room.messages.create!(body: "hi")
    Room.class_eval { def messages = super.where.not(body: "hi") }
    got = Fractal.records(:room_messages, [room.id], owner: room, association: :messages) { room.messages.order(:id).to_a }
    assert_equal [], got
    assert_equal({ association_redefined: 1 }, outcomes(:room_messages))
  end

  # Rule: ask in order. The values a call site passes are matched to the
  # statement's placeholders by position; a block that issues them in an
  # order other than the SQL's must not be compiled.
  def test_placeholders_out_of_sql_order_are_refused
    a = Account.create!(name: "a")
    Account.create!(name: "b")
    Fractal.define(:swapped, Account, one: true) do |p|
      id = p.bind
      name = p.bind
      Account.all.where(name: name).where(id: id).limit(1)
    end
    # Bound positionally, these would ask for name = 1 and id = "a".
    got = Fractal.records(:swapped, [a.id, "a"]) { Account.where(name: "a").find_by(id: a.id) }
    assert_equal a, got
    assert_equal({ uncompilable: 1 }, outcomes(:swapped))
  end

  # Rails 8.1 casts LIMIT with Integer() as it is set, so a request-time
  # limit cannot be a placeholder. The original answers instead of the
  # request raising.
  def test_a_relation_that_cannot_be_built_falls_back
    Account.create!(name: "a")
    Account.create!(name: "b")
    Fractal.define(:limited, Account) { |p| Account.all.order(:id).limit(p.bind) }
    got = Fractal.records(:limited, [1]) { Account.order(:id).limit(1).to_a }
    assert_equal ["a"], got.map(&:name)
    assert_equal({ uncompilable: 1 }, outcomes(:limited))
  end

  def test_a_wrong_number_of_values_is_refused
    Account.create!(name: "a")
    got = Fractal.records(:account_named, ["a", "extra"]) { Account.find_by(name: "a") }
    assert_equal "a", got.name
    assert_equal({ arity: 1 }, outcomes(:account_named))
  end

  # Relation#load applies strict_loading! with the relation's value
  # whenever it is set, so `strict_loading(false)` overrides a model that
  # loads strictly by default.
  def test_strict_loading_false_overrides_the_model_default
    Message.strict_loading_by_default = true
    Message.belongs_to :room
    room = Room.create!(name: "r")
    room.messages.create!(body: "hi")
    Fractal.define(:lenient, Message) { |p| Message.all.where(room_id: p.bind).strict_loading(false).order(:id) }
    got = Fractal.records(:lenient, [room.id]) { Message.where(room_id: room.id).strict_loading(false).order(:id).to_a }
    want = Message.where(room_id: room.id).strict_loading(false).order(:id).to_a
    assert_equal want.map(&:strict_loading?), got.map(&:strict_loading?)
    assert_equal [false], got.map(&:strict_loading?)
    assert_equal room, got.first.room
    assert_equal({ fast: 1 }, outcomes(:lenient))
  end

  def test_strict_loading_true_is_applied
    Account.create!(name: "a")
    Fractal.define(:strict, Account, one: true) { |p| Account.all.strict_loading.order(:id).limit(1) }
    got = Fractal.records(:strict, []) { Account.strict_loading.order(:id).first }
    assert_predicate got, :strict_loading?
    assert_equal({ fast: 1 }, outcomes(:strict))
  end

  # `prepare` runs first in every to_prepare: what was built for the
  # classes a reload retired is dropped, and the new classes build afresh.
  def test_prepare_forgets_what_was_built_for_retired_classes
    room = Room.create!(name: "r")
    room.messages.create!(body: "hi")
    Fractal.records(:room_messages, [room.id], owner: room, association: :messages) { room.messages.order(:id).to_a }
    old = Room
    readers = Fractal.instance_variable_get(:@readers)
    compiled = Fractal.instance_variable_get(:@compiled)
    assert(readers.keys.any? { |(klass, _)| klass.equal?(old) })
    refute_empty compiled.keys

    %i[Account Room Message].each { |c| Object.send(:remove_const, c) }
    define_models
    assert_empty readers.keys
    assert_empty compiled.keys
    assert_empty Fractal::Preload.instance_variable_get(:@plans).keys
    assert_empty Fractal::Preload.instance_variable_get(:@scopes).keys

    room = Room.create!(name: "s")
    room.messages.create!(body: "there")
    got = Fractal.records(:room_messages, [room.id], owner: room, association: :messages) { room.messages.order(:id).to_a }
    assert_equal ["there"], got.map(&:body)
    refute(readers.keys.any? { |(klass, _)| klass.equal?(old) })
  end

  def test_errors_are_rails_errors
    Account.create!(name: "a")
    ActiveRecord::Base.connection.execute("ALTER TABLE accounts RENAME TO gone")
    original = assert_raises(ActiveRecord::StatementInvalid) { Account.first }
    fast = assert_raises(ActiveRecord::StatementInvalid) { account_first }
    assert_equal original.class, fast.class
  ensure
    ActiveRecord::Base.connection.execute("ALTER TABLE gone RENAME TO accounts")
  end

  def test_verify_mode_runs_both_and_agrees
    Account.create!(name: "a")
    Fractal.send(:remove_const, :VERIFY)
    Fractal.const_set(:VERIFY, true)
    ran = 0
    got = Fractal.records(:account_first, []) { ran += 1; Account.first }
    assert_equal 1, ran
    assert_equal "a", got.name
  ensure
    Fractal.send(:remove_const, :VERIFY)
    Fractal.const_set(:VERIFY, false)
  end
end
