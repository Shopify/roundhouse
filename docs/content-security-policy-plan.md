# Controller Content Security Policy implementation plan

Status: implemented and exercised on `BuildTarget::Ruby`. Unsupported forms and other targets retain explicit errors. See the implementation record below.

## Goal

Support Writebook's controller-defined Content Security Policy (CSP) in emitted Ruby applications:

```ruby
content_security_policy if: -> { EmbedProvider.configured? } do |policy|
  policy.frame_src(*EmbedProvider.csp_frame_sources)
end
```

Evaluate the condition and sources for each request. Send the correct `Content-Security-Policy` header on the actual response.

Do not remove the survey gap until emitted execution proves this behavior. Do not change Writebook to avoid the unsupported declaration.

## Evidence and reference behavior

The requested Ruby survey completed ingest. It recorded one CSP gap, then stopped with 18 type errors.

The run also reported nine unconsumed initializer statements. Broad gap attribution changed 151 diagnostics to notes.

These counts describe the observed run. They are not acceptance counts for this change.

A temporary application with a constant frame source produced the same gap. Its emitted response returned `200 OK` without CSP.

Writebook's global CSP initializer contains only comments. Its controller declaration supplies the active policy.

Writebook pins Rails `8.2.0.alpha` at commit `3a4961048ad251b50991ae83135d760a8a9e8ae3`.

Use these pinned sources as the behavior reference:

- [Controller declaration and callbacks](https://github.com/rails/rails/blob/3a4961048ad251b50991ae83135d760a8a9e8ae3/actionpack/lib/action_controller/metal/content_security_policy.rb).
- [Policy storage, serialization, and middleware](https://github.com/rails/rails/blob/3a4961048ad251b50991ae83135d760a8a9e8ae3/actionpack/lib/action_dispatch/http/content_security_policy.rb).
- [Callback scope normalization](https://github.com/rails/rails/blob/3a4961048ad251b50991ae83135d760a8a9e8ae3/actionpack/lib/abstract_controller/callbacks.rb).

The local Rails checkout differs from the pin for directive-level `Proc` results. Do not assume complete source equivalence.

A direct local Rails-source run confirmed directive replacement, copy isolation, directive removal, and invalid-string rejection.

Full Writebook Rails execution currently lacks locked gems. Bundler reports missing dependencies. Compiler investigation does not require those gems.

## Scope and admission

The acceptance target is `BuildTarget::Ruby`. Shared packaging alone does not establish support for another target.

Required accepted behavior:

- Direct declarations on controllers, including `ApplicationController`.
- Inherited declarations and inherited routed actions.
- Request-time `if:` and `unless:` guards using supported symbols or zero-argument lambdas.
- Literal `only:` and `except:` action scopes.
- The `frame_src` directive with strings, supported symbols, and splatted source arrays.
- Literal enabled/disabled policy declarations, including `content_security_policy false`.
- Repeated declarations and directive replacement.
- Manual-header precedence and automatic-header suppression on `304` responses.

Accept another directive only when its typed runtime implementation and behavior checks exist. Do not infer support from its name.

The following forms remain unsupported:

- Global policies outside the validated literal canonical initializer profile.
- Nonce generators and nonce directive configuration.
- Report-only configuration and declarations.
- Directive-level `Proc` sources.
- Concern declarations outside the existing directive-mutation profile, and custom class macros.
- Block forms, conditions, enabled arguments, or options whose semantics the compiler cannot preserve.
- Targets without verified policy storage and response completion.

Report unsupported forms at their source locations during normal compilation. Survey mode must retain their gaps.

Do not rely only on `report_unrecognized_controller_macros`: it returns immediately outside survey mode.

Do not rely on the initializer ledger: unconsumed statements normally produce warnings. Some assignments become consumed configuration readers.

Check CSP configuration before generic configuration consumption can hide an unsupported setting. Identify framework calls, not matching comments or string contents.

Use a CSP-specific target admission rule. `report_ruby_only_sends` also permits JRuby, Spinel, and Roda.

Do not treat the existing `roundhouse-nonce` value as a secure nonce implementation.

## Design

### 1. Shared declaration lowering

1. Add `src/ingest/content_security_policy.rs` and register its pass in `src/ingest/app.rs`.
2. Run source synthesis before `sources::drain`.
3. Use an isolated Prism diagnostic scope for any generated source.
4. Validate the complete declaration before replacing its source representation.
5. Convert accepted declarations into dispatcher-only block callbacks.
6. Preserve declaration order, inherited order, guards, source spans, and callback halt behavior.
7. Keep dynamic source expressions in the callback body. Evaluate each expression once when Rails evaluates it.
8. Reject unsupported forms without partially installing a callback or removing the original gap.

Reuse the existing block-filter path in `build_filter_preamble`. Do not install an ordinary private named filter through `install_before_filter`.

The current private-filter path copies bodies into a controller's own actions. It can then remove the private methods.

This path can omit a child's callback when the child inherits its routed action. It can also repeat an inherited callback.

The relevant paths are in `src/lower/controller_to_library/mod.rs` and `src/lower/controller_to_library/process_action.rs`.

CSP callbacks must remain outside action bodies. Each applicable declaration must execute exactly once per routed request.

Generated local names must not capture application locals. Reject unsupported parameter rebinding, shadowing, or nonlocal control flow unless lowering preserves it.

Do not expand concern or custom class-macro forms accidentally. Detect and report them until their order and source handling have support.

### 2. Exact callback scopes

1. Distinguish an absent `only:` option from `only: []`.
2. Convert an absent scope into an unrestricted action predicate.
3. Convert `only: []` into a predicate that matches no actions.
4. Combine nonempty `only:` and `except:` scopes using the pinned Rails callback rules.
5. Preserve the relative evaluation order of action scopes and runtime guards.
6. Do not evaluate a guard that Rails would skip through short-circuit evaluation.

Do not pass both scope vectors unchanged into existing scope selection.

`filter_applies` and `include_check` currently choose `only` when it is nonempty. They then ignore `except`.

For example, this callback must not run for `index`:

```ruby
content_security_policy only: :index, except: :index do |policy|
  policy.frame_src
end
```

If a parent supplied `frame-src 'none'`, the excluded child callback must retain that restriction.

Normalize CSP scopes into explicit dispatcher conditions. Do not use an empty vector to represent both unrestricted and never-applicable callbacks.

### 3. Typed policy and request ownership

1. Add a typed `ActionDispatch::ContentSecurityPolicy` implementation under `runtime/ruby/`.
2. Add typed request storage for the optional policy.
3. Before each applicable callback, copy the current policy or create an empty policy.
4. Execute the block in the controller context, with its policy argument bound to that copy.
5. Assign the copy to the request only after the block returns successfully.
6. For a disabled declaration, clear the request policy at the point specified by Rails.
7. Preserve block effects when a disabled declaration also supplies a block.

Use explicit methods and typed storage. Do not use `method_missing`, runtime method generation, or type-erasing containers.

Allocate a policy only when needed. Do not allocate an empty policy for every controller request.

Copy mutable directive state where required. Never share mutable policy state between requests or callback copies.

Keep the prior request policy intact if an accepted block raises before assignment.

Directive behavior:

- A setter replaces that directive's source list. It does not append to the prior list.
- An empty argument list removes that directive.
- First-argument `nil` or `false` follows the pinned Rails removal behavior when admitted by the typed argument contract.
- Map supported symbols exactly. For example, `:none` becomes `'none'` and `:self` becomes `'self'`.
- Reject unknown symbols during assignment.
- Reject invalid source types. Do not convert arbitrary objects with `to_s`.
- Validate whitespace and semicolons when the policy builds the header, not when it receives a string.

Build-time validation is observable behavior. A later assignment can replace an invalid string before the policy builds.

A manual header or a `304` response can also prevent automatic policy building.

### 4. Actual request and load paths

1. Register the generated methods and policy types for analysis before post-analysis application lowering.
2. Cover check, LSP, and MCP consumers. These consumers do not run the application lowering sequence.
3. Load the shared runtime through the require chain and provide matching RBS signatures.
4. Add any new runtime stem to `src/project.rs::spinel_files` as required by project assembly.
5. Verify that the emitted application loads the actual policy implementation, not only its signatures.
6. Apply the CSP target admission rule before unsupported output can appear supported.

Ruby loads a separate request class from `runtime/spinel/scaffold/ruby_overlay/runtime/action_dispatch_request.rb`.

Changes only to the portable request initializer do not initialize Ruby's actual request. Use shared methods where possible without replacing either constructor.

Do not copy policy serialization or callback behavior into emitters.

### 5. Final-response header completion

1. Add one shared runtime method to complete automatic CSP header generation.
2. Call it after conditional-GET processing establishes the final response status.
3. Call it before the dispatcher copies headers to the outbound response.
4. Return without building the policy when status is `304`.
5. Return without building when either CSP response header already exists.
6. Return without adding a header when the request has no policy.
7. Otherwise, build the policy and set `Content-Security-Policy`.

The two manual headers are:

```text
Content-Security-Policy
Content-Security-Policy-Report-Only
```

Use the header store's case-insensitive behavior. Preserve manual headers; do not parse, combine, or replace them.

Do not copy Cache-Control merge behavior. CSP has a different precedence contract.

Integrate completion in these response paths:

- `runtime/spinel/scaffold/ruby_overlay/main.rb`.
- `runtime/spinel/scaffold/main.rb`.
- `runtime/spinel/test/test_helper.rb`.

Share behavior in the runtime. These paths contain calls only, not separate CSP implementations.

Updating the Spinel response path does not establish Spinel support. Keep its target refusal until native execution proves the contract.

Do not add an unconditional completion call in `ensure`. Existing exception exits do not establish a Rails mismatch by themselves.

### 6. Test-helper contract

The integration helper currently completes Cache-Control but does not run production's conditional-GET completion at that location.

Add CSP completion to the helper for controller-policy assertions. Do not expand this change into a general conditional-GET helper rewrite.

Use `Main.run_rack` for final-status acceptance checks. This directly exercises production's conditional GET and header completion order.

Existing production-path examples are in `tests/support/conditional_get.rs` and `tests/support/emit_and_run.rs`.

A controller that directly returns `304` is not sufficient proof of conditional-GET ordering.

## Implementation sequence

1. Preserve failing behavior in focused emitted regressions for the declaration and both inheritance directions.
2. Add strict declaration, configuration, and target admission checks.
3. Add the typed policy, request storage, signatures, and load registration.
4. Add shared declaration lowering with dispatcher-only execution and explicit scope predicates.
5. Add response completion calls and integration-helper policy assertions.
6. Run focused behavior checks, then the actual emitted response path.
7. Run runtime typing and relevant compiler regressions.
8. Run the Writebook survey and record the remaining unsupported behavior separately.
9. Update existing user and runtime documentation after runtime proof. Remove temporary reproduction files.

Do not remove a diagnostic as an intermediate substitute for implementation.

## Acceptance checks

Permanent regressions must assert consumer-visible behavior, not generated text or copied method names.

Use `tests/emit_and_run.rs` and its existing support helpers. Require zero error diagnostics and correct emitted Ruby behavior for supported examples.

### Policy behavior

1. With the provider table absent, assert that the response has no automatic CSP header.
2. With configured providers, assert the exact expected `frame-src` sources.
3. With a configured empty table, assert `frame-src 'none'`.
4. Change provider configuration between requests and assert that the next response uses the new table.
5. Assert symbol mappings, repeated-directive replacement, removal, and request isolation.
6. Assert that unknown symbols fail during assignment.
7. Assert that invalid strings fail during building but can be replaced before building.
8. Assert that a failed callback does not assign its incomplete policy copy.
9. Assert disabled-declaration behavior, including block effects when a block is present.

### Callback execution

1. Route to a child that declares CSP and inherits its action. Assert the generated header.
2. Route to a child that inherits both the action and the policy declaration. Assert exactly one callback effect.
3. Assert inherited declaration order with multiple callbacks.
4. Assert `if:`, `unless:`, action scopes, and once-only source evaluation.
5. Assert that overlapping `only:` and `except:` scopes retain an inherited restriction.
6. Assert that `only: []` runs no callback.
7. Assert callback behavior when an earlier callback renders or redirects.

### Actual response behavior

1. Through `Main.run_rack`, request a stable response and record its ETag and CSP header.
2. Repeat the request with a matching `If-None-Match`.
3. Assert status `304` and no automatically generated CSP header.
4. Assert that either manually supplied CSP header prevents automatic building and remains unchanged.
5. Exercise a valid response through the actual emitted program, not only the integration helper.

### Unsupported behavior

1. Assert located errors for unsupported declarations under normal compilation.
2. Assert retained gaps under survey mode.
3. Assert errors for active global, nonce, and report-only configuration.
4. Assert errors for unsupported concern, custom macro, and directive-level `Proc` forms.
5. Assert a target-specific refusal where the complete CSP contract lacks support.
6. Check source-consumer diagnostics without depending on post-analysis application lowering.

### Runtime and compiler checks

1. Run the focused emitted CSP regressions.
2. Run `tests/runtime_src_integration.rs::every_runtime_method_body_is_fully_typed`.
3. Run relevant runtime unit checks and controller/filter regressions.
4. Inspect the lowered callback structure where a failing behavior needs explanation.
5. Run the Writebook Ruby survey with the rebuilt binary.
6. Report only executed target coverage. Do not describe shared packaging as native execution proof.

Use the required shared devenv environment for project commands. Follow [the testing guide](development/testing.md).

## Completion criteria

The supported declaration produces correct headers through the emitted Ruby response path. Both adversarial inheritance cases pass.

Scope exclusions cannot remove an inherited restriction on an excluded action. Each applicable callback executes once.

Unsupported CSP forms remain explicit. Other targets do not acquire a false support claim.

Runtime methods remain fully typed. Source consumers resolve the generated behavior without application post-analysis lowering.

Remove the specific Writebook ingest gap only after these checks pass.

Do not require the old diagnostic counts to remain unchanged. Removing broad CSP attribution can expose additional existing errors.

This change does not claim that Writebook fully transpiles or runs. Report its remaining failures without suppressing them.

## Implementation record — 2026-10-16

The implementation constructs callback IR directly. It does not parse generated Ruby source.
Prism checks the original source for captured locals, block-local declarations, and numbered parameters.
Generated local names use a prefix absent from the source snapshot.
Unsupported declarations retain their original representation and a located error or survey gap.

The typed policy and controller methods live under `runtime/ruby/action_dispatch/`
and `runtime/ruby/action_controller/`. Existing recursive packaging includes these files.
The root require chain loads their implementations.
Both request constructors initialize optional policy storage.

Dispatcher callbacks use the shared explicit controller-body rewrites.
This preserves parameter reads and renders in callbacks.
The shared `Params.wrap` signature keeps parameter keys typed after request wrapping.
Callback method calls keep explicit receivers so application locals cannot capture them.
Only an executed callback runs its halt check, including when its guard responds.
Source analysis registers the runtime RBS contracts before it types callbacks.
Diagnostics inspect callback bodies and guards without application lowering.
Configuration admission follows Rails aliases, including logical assignments and yielded block parameters.

Executed checks:

- Twelve CSP admission, alias, source-consumer, and target refusal tests passed.
- Eleven emitted Ruby CSP tests passed, including both inheritance cases.
- `every_runtime_method_body_is_fully_typed` passed.
- Shared Ruby runtime unit checks passed: 566 runs, 1,995 assertions, no failures, errors, or skips.
- Fourteen compiler and controller suites passed: 402 tests.
  These include ingest, analysis, lowered Ruby output, callback scopes,
  callback halting, source spans, parameter wrapping, and frozen runtime literals.
- Documentation path checks passed.
- Rust emission and `cargo check` passed for real-blog without CSP.
  This checks shared-runtime compatibility, not native CSP support.
- A separate CLI smoke run emitted the Ruby application and exercised `Main.run_rack`.
  It returned the exact policy on `200`, no automatic policy on conditional `304`,
  and no policy when the request guard was false.
- Separate emitted programs confirmed responding-guard termination and callback method/local isolation.
- The CLI refused a global policy reached through `settings ||= Rails.application.config`,
  with its initializer path and line. The earlier binary emitted that input with warnings only.

Controller `frame_src` declarations remain Ruby-only. The existing global-policy and
directive-mutation profiles retain their Ruby-family runtime path.
Historical release snapshots remain unchanged.

### Writebook survey

The rebuilt binary surveyed the local Writebook working tree.
Its HEAD was `6ee6afd2b79b8ea31527510aa8d5d6142b19e217`.
This is not the pinned inventory source recorded in `docs/writebook.md`.

The Ruby survey completed with no CSP gap or CSP diagnostic.
It exited with status `1`: the CLI reported 28 type errors and nine unconsumed initializer statements.
It did not write an emitted Writebook project.
No diagnostics were suppressed to preserve the previous counts.

Remaining error groups:

- Constants: `ActiveSupport::Digest`, `Rails::Html::WhiteListSanitizer`,
  `Loofah::HTML5::SafeList::ACCEPTABLE_ATTRIBUTES`, `Rails::Html::FullSanitizer`,
  and `ActiveModel::Name`.
- The unresolved `ActionText::Markdown::Uploads` include.
- Upload call resolution for `safe_markdown_attribute`, `record`, `owning_book`,
  and `ActiveStorage::Current.set`.
- `Turbo::StreamsChannel.broadcast_render_later_to`, `String#constantize`,
  and a `call` on `untyped` in the Markdown extension.
- Unresolved controller and view instance variables, including `@markdown`,
  `@user`, `@book`, `@page`, `@picture`, `@section`, and `@upload`.
- Unverified anonymous keyword forwarding in `BooksHelper` and `LeavesHelper`.

The initializer warnings include Active Storage authentication hooks.
The survey does not establish complete Writebook execution or deployment safety.

## Main integration — 2026-10-19

The merge retains the global policy and cached script-hash behavior from main.
One typed policy stores both global directives and controller frame sources.
`dup` is the shared deep-copy operation; no separate copy alias remains.
Request setters mark lazy policy state ready, including explicit `nil` assignments.

The canonical literal initializer uses a complete AST reader for admission,
factory generation, and initializer consumption. Unsupported expressions,
unknown directives, and unknown source symbols cannot produce partial policies.
The existing environment disable guard remains request-visible.

Directive-mutation concerns use the same guarded, dispatcher-only lowering.
The response applies default headers before CSP completion.
No dispatcher builds the CSP header before conditional GET establishes the final status.
The former no-header divergence and its gap exception are removed.

New emitted checks cover global defaults with controller callbacks,
explicit policy clearing, cached around responses without a yield,
script hashes, environment disabling, manual headers, and final `304` responses.
A separate emitted CLI smoke run confirmed the combined cached `200` header,
conditional `304` suppression, and manual report-only precedence.
No new native controller-policy execution coverage is claimed.

Merge verification: `bin/rh verify --all` passed with 6,392 tests and 392 skips.
The documentation-test command completed with zero cases. Source analysis reads the bundled
OpenTelemetry span contract, and the CSRF guard regression exercises emitted
request behavior instead of matching generated source text.
