# Fractal: one class at a time

`roundhouse --target fractal --class NAME APP` keeps the app on Rails and
speeds up only the classes it is given. The class is the unit a person
switches on and off: deselect it and rebuild, and the tree is the app again.
The expression (one query chain) is the unit that is rewritten, and only
when it can be shown safe. Everything else is the app's own text.

It is its own target, and shares no code with `--target futamura`
([`specialization.md`](specialization.md)). Its query rewriter started as a
copy of futamura's specialization 1 and is expected to diverge; neither
target's output has to stay compatible with the other's.

## Selection

Rubydex resolves the class. Every `class`/`module` body whose resolved name
is the selected one is selected, reopenings included; the bodies of other
classes nested inside it are not. A method or action is selected when the
innermost class body around its name is a selected one, so `--class A
--class A::B::C` selects `C`'s methods but not `B`'s, and a method a
concern contributes (written in the concern's file) is not selected. A name the app does not
define is an error, so a misspelt `--class` cannot quietly build the
identity. With no `--class` the output is the identity: the app as source
control holds it, the control a fractal build is compared against.

## Rewriting

`src/emit/fractal/query.rs`. A chain rooted at a model constant or a plain
`has_many` reader, with `where(column: value)`, literal
`order`/`preload`/`includes`/`limit` and scope links, ending in a terminal
(`first`, `last`, `take`, `find_by`, `first(n)`/`last(n)` with a literal
count, `to_a`), becomes a Relation over bind placeholders that Rails builds
once into an `ActiveRecord::StatementCache`. The call site keeps the original
chain as its fallback block. A site is rewritten only if it also clears
these:

- every request-time value is a plain read (a local, an instance variable,
  `self`, a literal, or `.id` on one of those). The fallback evaluates the
  values again, and a value that can change between the two evaluations
  would make the paths disagree;
- the app defines no finder or association reader the statement would
  bypass;
- no request-time LIMIT: Active Record 8.1 casts a LIMIT with `Integer()`
  as it is set, so a placeholder raises while building;
- for a relation built in a local and loaded later (`rel =
  Account.where(name: @name)` ... `rel.first`), no value it reads is
  assigned or rebound anywhere in the method. The values are written at
  the terminal, so one the method changes, or a block parameter shadows, in
  between would query with a different value than the original;
- the file is not one ingest could only partly read (a `--survey` gap or a
  syntax error). Its text is copied unchanged.

Every other site stays as written and is reported with its reason. A site's
key carries the source file's id, so two files whose paths flatten to the
same stem never share a statement.

## Runtime

`src/emit/fractal/fractal.rb`, emitted as `config/initializers/fractal.rb`
with one `Fractal.define` per rewritten site. The original chain answers
instead of the statement when:

- a request-time value is one `StatementCache` refuses (nil, an Array, a
  Range, a record);
- the model is inside a `scoping`/`unscoped` block or an association scope
  (`scope_attributes?`), which Rails' own `find_by` statement cache steps
  around too. Building under a scope would bake it in for every later
  caller, and reusing an unscoped statement would ignore the caller's;
- the model constant was reloaded and the defines for the new class have not
  run yet. Statements are built per model class, database configuration and
  prepared-statements setting;
- the finder the chain ends in (`via:`) or the association reader it is
  rooted at is not Active Record's own, checked against the loaded classes;
- building the relation raises;
- the placeholders Rails collected, in SQL order, are not exactly the ones
  the block issued, in the order the call site passes values. Values are
  bound by position, so a mismatch would put each in the other's place.

Records take the relation's `strict_loading` value whenever it is set, as
`Relation#load` does, so `strict_loading(false)` overrides a model that
loads strictly by default. `Fractal.prepare` runs first in every
`to_prepare` and drops what was built for the classes a reload retired.

`Fractal.counts` reports, per site, how often the statement answered
(`fast`) and how often each of these sent it to the original.
`FRACTAL_VERIFY=1` runs the original as well and raises on any difference.
`tests/fractal_runtime.rs` runs each case against real Active Record.

## Verified

Campfire 11d3fb9, selecting `Current`, whose `account` is `Account.first`,
read on most pages:

- the identity tree is byte for byte the source (782 files);
- the selected tree differs in that one line and the generated initializer;
- the suite (601 runs) gives the same result on the source, the identity and
  the selected tree, plain and under `FRACTAL_VERIFY=1`, except for two
  SQLite WAL checkpoint tests that depend on timing. Those errored at random
  on every side, the source included, and passed on every side when run
  alone;
- the fast path answered all 543 `Current.account` calls the suite made,
  with no fallback.

Measured in process (Linux x86_64, Ruby 4.0.7, test environment, one account
row): `Current.account` took 37.5 us on Rails and 26.8 us rewritten with
YJIT, and 126.9 us and 71.3 us without it. The site was chosen to test the
method, not to save time.
