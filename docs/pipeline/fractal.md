# Fractal: specialize a running Rails app, one class at a time

`fractal emit` writes only the generated files in the app's directory
structure. It does not copy the app or change its files.
`fractal apply` copies those files onto the base app and replaces files
with the same paths. Files absent from the overlay stay unchanged.

Rails keeps loading and running the app. The selected methods get
specialized variants. A variant replaces one method only when its
assumptions hold at load time. Otherwise, the app's own method runs.

The point is application knowledge. Roundhouse ingests and analyzes the
selected classes and what its passes need to know about them (or the
whole app, when a pass needs it), so at build time it knows things a
JIT cannot learn from inside one method: the types flowing through a body, its effects, which
definitions exist and where, which shapes never vary. A Fractal pass
uses that to remove work the JIT would have to keep, such as framework
machinery that interprets a description fixed at build time, or an
intermediate object nothing outside the method can observe. Inlining
calls for their own sake, devirtualization and constant folding are the
JIT's job, not Fractal's.

**This baseline registers no pass.** It is the pipeline, the contract
and the runtime that passes attach to. A build today reports what it
would work with and emits nothing.

## Emit, then apply

1. Emit the file overlay:

   ```sh
   fractal emit --class Shop path/to/app -o out/fractal
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

Apply never deletes files. An empty overlay changes nothing in the
base app. Removing a class from the selection removes its variants
from the next generated initializer. If no variants remain, remove
the previously applied `config/initializers/fractal.rb` from the base app.

## Pipeline

All of it lives in [`src/emit/fractal/`](../../src/emit/fractal/).

1. **Scope** ([`scope.rs`](../../src/emit/fractal/scope.rs)). Before
   anything is ingested, Rubydex indexes the app's Ruby for constants
   alone (outside `test/`, `spec/`, `db/`, `vendor/` and the like) and
   answers where each `--class` is defined. The build ingests those
   files, plus what each registered pass declares with `Pass::scope`:
   `Reach::Ancestors` (the files defining the selected classes'
   superclasses and mixins), `Reach::Signatures` (`sig/**/*.rbs`),
   `Reach::Schema` (`db/schema.rb`, else `db/structure.sql`), or a
   `Reach::Custom` function over the index. It always keeps what tells
   ingest the app's shape: `Gemfile.lock`, `roundhouse.yml`,
   `config/application.rb`, the inflections initializer, and, for a
   kept file, its Packwerk `package.yml` or its component's
   `Rails::Engine` declaration. Everything else reads as absent, so what
   it would have told the analyzer is unknown and a pass that needed it
   declines: a narrower scope gives fewer variants, never different
   ones. A pass that returns `Scope::WholeApp`, or `--whole-app`, makes
   the build ingest everything instead.
2. **Ingest and analyze**, without lowering. Files ingest can only
   partly read (a `--survey` gap, a syntax error) keep the app's code.
   None of their methods gets a variant.
3. **Select** ([`select.rs`](../../src/emit/fractal/select.rs)). Rubydex
   finds every `class`/`module` body whose resolved name is a `--class`,
   reopenings included. Bodies of other classes nested inside are not
   selected; the innermost body around a definition decides, so `--class
   A --class A::B::C` covers what is written in `A` and in `C` but not in
   `B`. A name the app does not define is an error.
4. **Regions** ([`region.rs`](../../src/emit/fractal/region.rs)). Each
   `def` written directly in a selected body is a region, with the
   analyzer's IR for it (one copy per includer for a concern's methods)
   and a summary of what the analyzer knows: how much of the body is
   typed, whether it records effects or writes state. A region is
   *admitted* when a variant could be attached to it. It is not when the
   method is defined inside a block or condition, as an argument to a
   call that may wrap it (`memoize def ...`; the visibility calls are
   fine), on an object other than the class, or when its body calls
   `__dir__` or `require_relative`, or defines a method.
5. **Passes** ([`pass.rs`](../../src/emit/fractal/pass.rs)). Each pass
   sees every admitted region and may propose edits inside the method's
   definition, the guards they rely on, and whether replaying the method
   is unobservable. `pass::registered()` is empty.
6. **Variants** ([`variant.rs`](../../src/emit/fractal/variant.rs)). The
   proposals for one region are combined, in pass order. A proposal
   with an edit outside the definition, or overlapping an earlier edit,
   is dropped and reported. A region nothing edited gets no variant.
7. **Emit** ([`overlay.rs`](../../src/emit/fractal/overlay.rs)). The
   output directory gets `config/initializers/fractal.rb` when at least
   one variant exists. Otherwise, it is empty. Emit never copies the app.
   A rebuild removes files from an earlier build in the output directory.
   It refuses other files there, which keeps `-o` from pointing at the app.
8. **Apply** ([`overlay.rs`](../../src/emit/fractal/overlay.rs)). The
   separate apply command copies only overlay files onto the existing app.
   It adds generated files and overwrites files with the same paths.
   It leaves all other files unchanged.

`--report PATH` writes the build's decisions as JSON: the selected
bodies, each region with its admission and knowledge, the variants and
their guards, and each declined site with the pass and reason.
Keep the report outside the overlay directory.

## What a variant is

The variant of `Owner#name` is the method's definition with the edits
applied, wrapped in the class and module headers that reproduce its
lexical nesting, preceded by the file's `frozen_string_literal` comment,
and evaluated with the original file and line. Installed, it is a plain
`def` in the method's own class, so callers, `super`, constant lookup,
backtraces and `instance_method(...).owner` and `.source_location` see
what they saw before. The runtime restores the method's visibility.

## Runtime

[`fractal.rb`](../../src/emit/fractal/fractal.rb), at the top of the
generated initializer, registers each variant and installs it when its
owner loads: from Zeitwerk's `on_load` (every load, so a reload gets it
again), and from `to_prepare` for owners loaded some other way. A
variant installs only if all of these hold, and otherwise the app's
method stays:

- the method is still the app's definition at its file and line, owned
  by its class: nothing prepended, wrapped or redefined it;
- the file is the one the build read (size and CRC-32);
- defining the variant runs no `method_added` (or
  `singleton_method_added`) hook, other than the two framework hooks
  that do nothing for a method that already exists: Current attributes'
  delegator generation and Active Record's relation delegation. The
  method must not also be a `module_function` copy;
- every guard a pass added holds. The one guard kind so far,
  `defined_at`, checks that another app method is still the definition
  at a given file and line, which is what a variant that relies on that
  method's body needs to know.

A variant whose method a later reopening defines, or whose guard names
a constant that has not loaded, stays pending and is tried again at the
next prepare or when that constant loads. `Fractal.status` reports each
variant as installed, declined (with the reason) or pending. `FRACTAL_STRICT=1` raises on a decline instead, for
CI. `FRACTAL_VERIFY=1` runs the original and the variant one after the
other on every call and raises `Fractal::Mismatch` when their values,
or their exceptions' classes and messages, differ; `Fractal.calls`
counts the comparisons. It does so only for a variant whose pass vouched
that replay is unobservable and whose method the analyzer sees as fully
typed, effect-free and writing no state. Calls with a block are not
replayed.

`tests/fractal.rs` covers selection, admission, knowledge, variant
construction and the CLI. `tests/fractal_apply.rs` covers file replacement,
unchanged files, path checks, empty overlays, and the separate commands.
`tests/fractal_runtime.rs` emits an overlay with a test pass, applies it to
the base app, and checks the runtime against Ruby. It includes Active
Record and Current attributes when installed.

## JIT shape

Fractal emits Ruby for CRuby's JITs to compile, so its output shape is
a choice. Where YJIT and ZJIT would want different shapes, it takes
ZJIT's.

- **The unit of specialization is a whole method.** ZJIT compiles a
  method at a time and profiles each ISEQ separately. A variant is its
  own ISEQ, so its profile holds only its own types, and the original's
  paths are not in the code ZJIT optimizes.
- **The choice is made once, at load.** Guards run when the owner loads,
  never per call, so a variant's body carries no check for the JIT to
  compile around. Installing at load, before the first call, also means
  the JIT never compiles the original only to invalidate it when the
  method is redefined.
- **Variants are `def`s, never `define_method`.** ZJIT calls an ISEQ
  method directly in more situations than a `define_method` one: a call
  to a block-backed method that passes a block, or whose proc is not
  shareable outside single-Ractor mode, stays a dynamic send
  ([`zjit/src/hir.rs`](https://github.com/ruby/ruby/blob/master/zjit/src/hir.rs)).
  Only verify mode wraps a method with `define_method`.
- **The runtime never creates singleton classes on instances.** ZJIT's
  direct sends assume no instance of the receiver's class has one, and
  creating one invalidates that code.

Measuring a pass means comparing the original with the JIT against the
variant with the same JIT, after warmup, for CPU and for allocated
objects and bytes, GC time and retained memory.

## Writing a pass

Implement [`Pass`](../../src/emit/fractal/pass.rs): a name, a `scope`
and a `run` that takes a [`Region`](../../src/emit/fractal/region.rs)
and returns a `Proposal`. The scope says what the build has to ingest
for the pass to know what it needs: start from `Scope::Classes` with
the narrowest reaches that give it its facts, and say
`Scope::WholeApp` only when they can come from anywhere in the app. Read the analyzed IR in `region.analyzed`; express
replacements as `Edit`s on source spans inside `region.def_span`; add a
`Guard` for every app definition outside the method the variant relies
on; set `replayable` only when running the method twice is
unobservable. Decline, with a reason, whatever cannot be shown to
behave as the original does for every caller. Register the pass in
`pass::registered()` and pin it with a `tests/fractal_runtime.rs` case
that runs the variant.
