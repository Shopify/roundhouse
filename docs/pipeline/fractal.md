# Fractal: compile a Rails app in place, one file at a time

`fractal emit --file PATH APP -o OUT` writes only changed files under
their app-relative paths. The output is a file overlay, not a complete
app. Emit leaves the base app unchanged.

`fractal apply OUT APP` copies those files onto the base app and replaces
files with the same paths. Rails runs the rewritten source with the same
gems and line numbers. Nothing installs code at boot or checks assumptions
at load. Each rewritten file contains the code that runs.

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

## Emit, then apply

1. Emit the file overlay:

   ```sh
   fractal emit --file app/models/article.rb path/to/app -o out/fractal
   ```

2. Apply the overlay to the base app:

   ```sh
   fractal apply out/fractal path/to/app
   ```

The Cargo binary is `fractal`. The shipped binary accepts the same
commands as `roundhouse fractal emit` and `roundhouse fractal apply`.
`roundhouse --target fractal` also uses the same emission step.

Emit defaults to the current app and `out/fractal`. Apply uses the same
defaults. The overlay can be inside the base app. Apply does not create
a second app. It rejects symbolic links and incompatible destination
types before it copies files.

Apply never deletes files or restores earlier source. An overlay with
no changed app files leaves the base app unchanged. Removing a `--file`
removes that file from the next overlay, not from the base app.
Use version control to restore source before compiling a different selection.

The overlay includes `.roundhouse-fractal.json` with the compiled files,
passes, and changed files. Apply keeps this metadata in the overlay.
An unchanged build emits only the metadata file.

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
   can only partly read (a `--survey` gap, a syntax error) stay unchanged.
   The overlay does not include them.
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
6. **Emit** ([`tree.rs`](../../src/emit/fractal/tree.rs)). The output
   contains only changed files and `.roundhouse-fractal.json`.
   A rebuild replaces an earlier overlay. It refuses other non-empty
   directories, the app itself, and directories containing the app.
   It rejects rewritten paths with parent or absolute components before
   it changes an earlier output.
7. **Apply** ([`tree.rs`](../../src/emit/fractal/tree.rs)). The separate
   command overwrites only files present in the overlay. It leaves
   unrelated files unchanged and does not copy the build metadata.

`--report PATH` writes the build's decisions as JSON: the compiled
files, every method in them with the analyzer's facts, files skipped
and why, every rewrite (`path:line:col`, pass, old and new text) and every
declined site with the pass and reason.
Keep the report outside the overlay directory.

Source discovery keeps nested namespaces such as `app/models/public/`
and `app/models/test/`. It excludes test and state directories only at
the app root. It excludes `node_modules`, hidden directories, and earlier
output overlays at any depth.

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
- the applied app is what gets tested: apply the overlay, then run
  the app's test suite;
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
enum :status, { draft: 0, published: 1 }
def listed? = published? && !hidden?
# compiled
def listed? = (status_for_database == 1) && !hidden?
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

Implement [`Pass`](../../src/emit/fractal/pass.rs): a name, a one-line
`summary` (what the pass explorer shows), a `scope`,
and a `run` that reads a [`Unit`](../../src/emit/fractal/pass.rs) and
returns a `Proposal` of `Edit`s (spans of the unit's file and their
replacement text) and declined sites. [`Cx`](../../src/emit/fractal/pass.rs)
gives app-wide facts read from source without ingest: every Ruby file of
the program, a Rubydex index of classes and their ancestors and
descendants, every method name the app defines, and the lockfile's gem
versions. Decline, with a reason, whatever cannot be shown to behave as
the original does for every caller of the compiled app. Register the pass
in `pass::registered()`, pin its rewrites and declines in a build test,
and add a case to `tests/fractal_runtime.rs`. That test emits and applies
the overlay against Active Record and SQLite. It compares the app's
answers before and after apply.

## Pass explorer

[`wasm/fractal/`](../../wasm/fractal/) runs the passes in the browser,
on the compiler wasm, over a small app a person can edit: pick the file
to compile and step through the stages, from the source to the file
after each pass, with what each pass rewrote and what it declined and
why. It calls the wasm's `fractal_stages` export, which runs
[`stages.rs`](../../src/emit/fractal/stages.rs): the same selection,
passes and driver as a build, over sources already in memory
(`Cx::in_memory`, since the browser has no app on disk), keeping what
each pass added. Stage *n* is the passes `1..=n` applied together, the
way a build applies them. It writes nothing. The explorer shows the
complete selected file at each stage, but `fractal emit` writes only
changed files. Use `fractal apply` to overwrite those files in the base app.

The example defines `draft?` in `app/models/public/listing.rb`.
The browser and disk source filters both keep this nested namespace.
The pass declines the app-defined predicate instead of replacing its call.
[`site/fractal.html`](../../site/fractal.html) introduces Fractal and
links to the explorer.

## JIT shape

Fractal emits Ruby for CRuby's JITs to compile, so its output shape is a
choice. Where YJIT and ZJIT would want different shapes, it takes ZJIT's:
plain `def`s rather than `define_method`, direct calls rather than
`send`, and no singleton classes created on instances. Measuring a pass
means comparing the app with the JIT against the compiled app with the
same JIT, after warmup, for CPU, allocated objects and bytes, GC time and
retained memory, and for boot time when the pass removes boot work.
