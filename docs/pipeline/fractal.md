# Fractal: compile a Rails app in place, one file at a time

`roundhouse --target fractal --file PATH APP -o OUT` writes the
*compiled app*: a complete copy of `APP` in which the files named with
`--file` (paths relative to `APP`, repeatable) have been rewritten. The
compiled app is still a Rails app, it runs on the same Rails and gems,
and every line sits where it did in the source. It is the program you
test and ship. Nothing in it installs code at boot, checks assumptions
at load or keeps the original code around: the rewritten code is the
only code there is. Dropping a `--file` and rebuilding gives that file
back as written.

The point is application knowledge. Roundhouse ingests and analyzes the
files it compiles and what its passes need, so at build time it knows
things a JIT cannot learn from inside one method, and that Rails
recomputes on every boot: the types flowing through a body, its effects,
which definitions exist and where, which descriptions (an `enum`
mapping, a column list) are fixed. A Fractal pass uses that to remove
work: framework machinery that interprets a description fixed at build
time, an intermediate object nothing can observe, a method Rails
generates at boot from a description the build already read. Inlining
for its own sake, devirtualization and constant folding are the JIT's
job, not Fractal's.

Because a rewrite is an edit to source, a rewrite of a class body (work
Rails does at boot) and a rewrite of a method body (work it does at run
time) are the same kind of edit, applied the same way.

## Pipeline

All of it lives in [`src/emit/fractal/`](../../src/emit/fractal/).

1. **Scope** ([`scope.rs`](../../src/emit/fractal/scope.rs)). The build
   ingests the files it compiles, plus what each pass declares it needs
   ([`Pass::scope`](../../src/emit/fractal/pass.rs)), found through a
   Rubydex index of the app's constants (the files defining the
   ancestors of the classes the compiled files define, say), or the
   whole app when a pass needs it or `--whole-app` asks. A `--file` that
   does not exist, or lies outside the app, is an error.
2. **Ingest and analyze** those files, without lowering. Files ingest
   can only partly read (a `--survey` gap, a syntax error) are copied
   as written and never rewritten.
3. **Select** ([`select.rs`](../../src/emit/fractal/select.rs)). The
   compiled files, and every `class`/`module` body in them, so a pass
   knows which class each site belongs to: the innermost body around it.
4. **Passes** ([`pass.rs`](../../src/emit/fractal/pass.rs)). Each
   compiled file is a [`Unit`](../../src/emit/fractal/pass.rs): its text,
   its class bodies, and its methods with the analyzer's facts
   ([`region.rs`](../../src/emit/fractal/region.rs)). Each pass proposes
   edits anywhere in it, and declines what it cannot show equivalent,
   with a reason.
5. **Rewrite** ([`rewrite.rs`](../../src/emit/fractal/rewrite.rs)). The
   driver applies the proposals in pass order. A proposal is refused
   whole, and reported, when an edit falls outside the file, overlaps
   another edit, or changes the number of lines; or when the rewritten
   file parses with an error or warning the original did not have.
6. **Write** ([`tree.rs`](../../src/emit/fractal/tree.rs)). The output
   directory gets a full copy of the app (except `.git`, `tmp`, `log`
   and `node_modules` at its root) with the rewritten files in place of
   the originals, and `.roundhouse-fractal.json`, which lists the
   compiled files, the passes and the rewritten files. A rebuild
   replaces an earlier build; any other non-empty directory is refused,
   as is the app itself or a directory containing it. An output inside
   the app is left out of the copy.

`--report PATH` writes the build's decisions as JSON: the compiled
files, every method in them with the analyzer's facts, files skipped
and why, every rewrite (`path:line:col`, pass, old and new text) and every
declined site with the pass and reason.

## Line numbers

Every edit keeps the number of lines it replaces, so a line of the
compiled app is the same line of the source. Backtraces, error grouping,
coverage, `__LINE__` and `source_location` point at the code a person
wrote, and the compiled file can be read side by side with the original.
A pass that needs more room writes several statements on one line.

## No runtime fallback

The compiled app has no guard that declines a rewrite when an assumption
fails at load, and no original to fall back to. Every assumption a
rewrite rests on is checked at build time, from the app's source and
lockfile, and anything a pass cannot check is declined. That makes the
build, not the boot, responsible for correctness:

- a pass must account for every way the app could make its rewrite
  wrong (an app definition of a framework method it relies on, a
  subclass that overrides it, a version of Rails that behaves
  differently), and decline where it cannot;
- the compiled app is what gets tested: run the app's test suite
  against the output, not against the source;
- the build must see what the deploy will run: the same source, the same
  `Gemfile.lock`.

Checking a booted app's state against the source's, and stamping a build
with what it was compiled from, are natural next steps for running this
on a large app; this proof of concept does not do them yet.

## Passes

### `inline-enum-predicates`

[`enum_predicates.rs`](../../src/emit/fractal/enum_predicates.rs). Rails
defines each enum predicate with
`define_method("#{value_method_name}?") { public_send(:"#{name}_for_database") == value }`
(unchanged from 7.1 to 8.1). In a compiled file that declares an enum in
a class body, a call to its predicate inside an instance method of that
class becomes that comparison:

```ruby
enum :status, { pending: 0, paid: 1 }
def shippable? = paid? && !archived?
# compiled
def shippable? = (status_for_database == 1) && !archived?
```

The call no longer goes through a block-backed method, which ZJIT cannot
call directly, or a `public_send` of a dynamic symbol; `status_for_database`
is an ordinary `def` Active Record generates. A string value is frozen
(`"r".freeze`) unless the file freezes its literals, so the comparison
allocates nothing, as Rails' does not.

It declines, with a reason, when:

- `Gemfile.lock` pins no activerecord, or one older than 7.1;
- the declaration is not literal (`enum :status, STATUSES`), uses the
  keyword form removed in 8.0 (`enum status: {...}`), maps a label to a
  symbol (8.1 compares the symbol's name, earlier versions the symbol),
  or has an option it does not know;
- the app's own source defines the predicate or `<name>_for_database`
  anywhere, by `def`, `define_method`, `alias`, `alias_method`, `attr_*`
  or `delegate`;
- another enum in the app produces the same predicate, or the class, a
  subclass, or a module a subclass includes declares an enum it cannot
  read or defines methods with computed names, either of which could
  produce it;
- the method has a local variable named `<name>_for_database`.

It leaves alone calls with a receiver, arguments or a block; calls
inside a block or lambda (where `self` may differ), under `defined?`, in
class methods, and in subclasses' methods; and labels Ruby cannot call
bare.

## Writing a pass

Implement [`Pass`](../../src/emit/fractal/pass.rs): a name, a `scope`,
and a `run` that reads a [`Unit`](../../src/emit/fractal/pass.rs) and
returns a `Proposal` of `Edit`s (spans of the unit's file and their
replacement text) and declined sites. [`Cx`](../../src/emit/fractal/pass.rs)
gives app-wide facts read from source without ingest: every Ruby file of
the program, a Rubydex index of classes and their ancestors and
descendants, every method name the app defines, and the lockfile's gem
versions. Decline, with a reason, whatever cannot be shown to behave as
the original does for every caller of the compiled app. Register the pass
in `pass::registered()`, pin its rewrites and declines in a build test,
and add a case to `tests/fractal_runtime.rs`, which runs an app and its
compiled tree against Active Record and SQLite and compares what they
answer.

## JIT shape

Fractal emits Ruby for CRuby's JITs to compile, so its output shape is a
choice. Where YJIT and ZJIT would want different shapes, it takes ZJIT's:
plain `def`s rather than `define_method`, direct calls rather than
`send`, and no singleton classes created on instances. Measuring a pass
means comparing the app with the JIT against the compiled app with the
same JIT, after warmup, for CPU, allocated objects and bytes, GC time and
retained memory, and for boot time when the pass removes boot work.
