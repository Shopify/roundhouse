# Fractal: specialize a running Rails app, one class at a time

`roundhouse --target fractal --class NAME APP` emits specialized
variants of the methods of the classes it names, as an overlay to place
on the app's root. It never edits an app file, so the overlay holds only
what it generates; every other file is the app's own. Rails keeps
loading and running the app. A variant replaces
one method only when the assumptions it was built under still hold at
load time; otherwise the app's own method runs. Deselecting a class
and rebuilding gives the app back.

The point is application knowledge. Roundhouse ingests and analyzes the
whole app, so at build time it knows things a JIT cannot learn from
inside one method: the types flowing through a body, its effects, which
definitions exist and where, which shapes never vary. A Fractal pass
uses that to remove work the JIT would have to keep, such as framework
machinery that interprets a description fixed at build time, or an
intermediate object nothing outside the method can observe. Inlining
calls for their own sake, devirtualization and constant folding are the
JIT's job, not Fractal's.

One pass is registered, [`json-writer`](#the-json-writer-pass): it
writes a fixed-shape hash's JSON directly where a method builds the
hash only to encode it. Everything below it is the pipeline, the
contract and the runtime that passes attach to.

## Pipeline

All of it lives in [`src/emit/fractal/`](../../src/emit/fractal/).

1. **Ingest and analyze**, without lowering. Files ingest can only
   partly read (a `--survey` gap, a syntax error) are still copied, but
   none of their methods gets a variant.
2. **Select** ([`select.rs`](../../src/emit/fractal/select.rs)). Rubydex
   finds every `class`/`module` body whose resolved name is a `--class`,
   reopenings included. Bodies of other classes nested inside are not
   selected; the innermost body around a definition decides, so `--class
   A --class A::B::C` covers what is written in `A` and in `C` but not in
   `B`. A name the app does not define is an error.
3. **Regions** ([`region.rs`](../../src/emit/fractal/region.rs)). Each
   `def` written directly in a selected body is a region, with the
   analyzer's IR for it (one copy per includer for a concern's methods)
   and a summary of what the analyzer knows: how much of the body is
   typed, whether it records effects or writes state. A region is
   *admitted* when a variant could be attached to it. It is not when the
   method is defined inside a block or condition, as an argument to a
   call that may wrap it (`memoize def ...`; the visibility calls are
   fine), on an object other than the class, or when its body calls
   `__dir__` or `require_relative`, or defines a method.
4. **Passes** ([`pass.rs`](../../src/emit/fractal/pass.rs)). Each pass
   sees every admitted region and may propose edits inside the method's
   definition, the guards they rely on, and whether replaying the method
   is unobservable. `pass::registered()` lists the passes a build
   runs: today, `json-writer`.
5. **Variants** ([`variant.rs`](../../src/emit/fractal/variant.rs)). The
   proposals for one region are combined, in pass order. A proposal
   with an edit outside the definition, or overlapping an earlier edit,
   is dropped and reported. A region nothing edited gets no variant.
6. **Write** ([`overlay.rs`](../../src/emit/fractal/overlay.rs)). The
   output directory gets `config/initializers/fractal.rb` when there is
   at least one variant, and nothing otherwise: a large app is never
   copied. A rebuild first removes what an earlier build wrote there, so
   deselecting a class leaves no stale initializer; a directory holding
   anything else is refused, which keeps `-o` from pointing at the app.
   Placing the overlay on the app's root is the deploy step, and so is
   removing the initializer when a rebuild no longer generates it.

`--report PATH` writes the build's decisions as JSON: the selected
bodies, every region with its admission and knowledge, the variants and
their guards, and every declined site with the pass and reason.

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
- every guard a pass added holds. `defined_at` checks that another
  app method is still the definition at a given file and line, which
  is what a variant that relies on that method's body needs to know.
  `active_support_json` checks what a `json-writer` variant relies on
  (see below).

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
construction and the CLI. `tests/fractal_runtime.rs` builds an app with
a test pass and checks every install, pending and decline rule against
Ruby, with Active Record and Current attributes when they are
installed. `tests/fractal_json_writer.rs` covers the `json-writer`
pass.

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

## The json-writer pass

[`json_writer.rs`](../../src/emit/fractal/json_writer.rs) rewrites a
method that builds a fresh fixed-key hash only to encode it with
Active Support:

```ruby
def to_wire
  { label: label, count: count }.to_json
end
```

Rails' `Hash#to_json` walks the hash into a second one (`jsonify`), has
the json gem generate that, then scans the result again to escape
`<`, `>`, `&` and U+2028/U+2029. The variant evaluates the values,
once each and in source order as the literal did, into temporaries,
checks each one, and when every value is exactly a JSON scalar whose
bytes it knows, returns them written into a string literal: no hash is
built and nothing is traversed twice. Otherwise it evaluates the
original expression over the temporaries, so the encoding is the
app's own.

The checks use only core methods, never a method of the value itself
until it is known to be a String: a String must be an instance of
`String` itself (a subclass may have its own `to_json`), ASCII-only or
valid UTF-8, and contain nothing either the json gem or Active Support
would escape. An Integer, `true`, `false` or `nil` is written as is. A
literal value whose JSON cannot vary is written into the template at
build time. The analyzer's types choose which checks a value gets, and
nothing more: a value they mispredict fails its check.

The hash must not be observable outside the encoding. It is the
literal receiver of `to_json`, or a local assigned that literal once,
as a statement of the method body, and read once, by a later
`to_json` statement. Public `as_json` and `to_json` methods are not
touched, so their callers get the objects they always did. The pass
reports and keeps: `JSON.generate` (the json gem writes small hashes
natively, and a Ruby writer measured slower than it under YJIT and
ZJIT); `to_json` with options or `&.`; non-literal, non-identifier or
duplicate keys (an overwritten value can still have effects); splats
and implicit values; values outside String, Integer, true, false and
nil (or one of those or nil); methods that can observe their locals
(`binding`, `eval`, `local_variables`, ...), because the variant adds
temporaries; and methods with a heredoc. Every line of a variant is
the line it was in the app, so a value that raises does so on its own
line.

`active_support_json`, its guard, waits for Active Support's
`to_json` to load, then requires, before installing: that
`Hash#to_json` is Active Support's; that `ActiveSupport.json_encoder`
is Active Support's `JSONGemEncoder`, unmodified, without options;
that the json gem generates natively; and that the core methods the
checks and the writer call (`String#encoding`, `Integer#to_s`,
`Regexp#match?`, ...) are Ruby's own C methods. Like every guard it is
checked once, when the owner loads, not per call.

It declines when Active Support encodes with the json gem's `Coder`
(Rails 8.1 does with json 2.15.2 and later). The original is then
native from end to end: on Ruby 4.0 the writer was still about twice
as fast for two values, but slower for six, so which shapes win there
needs a cost model the pass does not have yet. A value that needs
escaping costs the checks on top of the original expression.

Assumed and not checked: a String is not given its own `to_json` on
its singleton class, and a String subclass does not redefine
`instance_of?`. The json gem would honour the first, and the check
would call the second.

`scripts/bench-fractal-json-writer` measures the pass: the app's
methods and the variants, each in its own process, on the same Ruby,
JIT, workload and warmup.

## Writing a pass

Implement [`Pass`](../../src/emit/fractal/pass.rs): a name and a `run`
that takes a [`Region`](../../src/emit/fractal/region.rs) and returns a
`Proposal`. Read the analyzed IR in `region.analyzed`; express
replacements as `Edit`s on source spans inside `region.def_span`; add a
`Guard` for every app definition outside the method the variant relies
on; set `replayable` only when running the method twice is
unobservable. Decline, with a reason, whatever cannot be shown to
behave as the original does for every caller. Register the pass in
`pass::registered()` and pin it with a `tests/fractal_runtime.rs` case
that runs the variant.
