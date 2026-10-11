# frozen_string_literal: true
#
# THE LEDGER. Every entry below stands for something the compiler does not
# model yet, spliced into the emitted campfire app ahead of `app/models` so
# `scripts/campfire-walk` can get past it and report what lies BEHIND it.
#
# Rules, and they are the whole point of keeping this file in the repo:
#
#   * One `# GAP:` comment per entry, naming what is missing and why the
#     compiler cannot produce it yet. The count is reported by the walk.
#   * A gap closed in the compiler is an entry DELETED here. The diff of this
#     file over time is the progress report.
#   * Never resolve a compiler bug by adding a stub. A stub is for something
#     deliberately out of scope (a gem we do not intend to transpile, a Rails
#     subsystem parked by design) or for a gap already ledgered elsewhere —
#     it buys the walk another few steps, it does not fix anything.
#   * `scripts/campfire-walk --no-stubs` walks without this file, which is
#     how you check whether an entry is still earning its place.
#
# TWO LOAD POINTS, and getting them backwards is a silent no-op:
#   * a stub for a MISSING constant has to exist BEFORE the app loads, so it
#     is written at the top level of this file (spliced ahead of `app/models`);
#   * a patch that OVERRIDES emitted app code has to run AFTER the app loads,
#     or the app's own definition simply replaces it. Those go in
#     `WALK_LATE_PATCHES`, which the driver calls once `main.rb` is loaded.
#
# Nothing here is compiler output and nothing here ships: it exists so a
# single walk reports the FULL list of walls instead of one per session.
#
# 2026-10-11 tip: every remaining late patch was deleted — emit already
# covers FirstRun autosave/`memberships_grant_to`, `has_one_attached`
# readers, `belongs_to` default lambdas, and `RoomsHelper` via
# `Current.controller.room`. Association `Array#pluck` is `assoc_pluck`
# (#818); Hash/untyped `Array#pluck` is #793. GAP count is zero; the
# empty `WALK_LATE_PATCHES` keeps the suite/walk splice shape until the
# drivers drop `walk_stubs` entirely.

# Patches applied after the emitted app has loaded; see the header.
WALK_LATE_PATCHES = []

# ActionText is MODELLED now — `has_rich_text`, `ActionText::RichText`,
# `ActionText::Content` and `rich_text_area` all lower (src/lower/
# rich_text.rs + runtime/ruby/action_text.rb), so `Content`,
# `Attachment.tag_name` and `Attachable` have left this file.
#
# The classes campfire REOPENS under the ActionText namespace
# (`lib/rails_ext/{filter,filters,actiontext_opengraph_embeds}.rb`) are
# emitted from the app's own source now — they used to be dropped, and
# the stand-ins that stood here with them are gone.

# RETIRED: `ActionDispatch::Request#user_agent` / `#remote_ip`. Both runtime
# Request classes read them off the env now, and this stub was answering
# CONSTANTS over the top of working code — `reject_banned_ip` saw 127.0.0.1
# no matter what the caller sent, so campfire's ban tests could not fail and
# could not pass. A stub that outlives its gap does not go quiet; it lies.

# RETIRED, all six on 2026-08-23, and the diff of this file over that
# session IS the progress report the header promises. Each was a
# LOAD-time wall — a class body, an include, a superclass — so none was
# a route that 500s; each was a tree that did not boot, which is why
# they had to fall before any benchmark could name a subject.
#
#   * `PlatformAgent` — the gem is a Gemfile line after all
#     (`project.rs::RUNTIME_GEMS`). It needed ActiveSupport's
#     `Module#delegate` and nothing else; supplying that one method is
#     thirty lines (`runtime/spinel/module_delegate.rb`).
#   * `ActiveModel::Model` — synthesized for a LIBRARY class by
#     `lower::active_model_model`, the twin of the tableless-model path
#     that already did it for anything under `app/models/`.
#   * `ActionView::Helpers::SanitizeHelper` — its members qualify to
#     `ActionView::ViewHelpers.<name>`, and `strip_tags` is real now
#     (24 of 25 probes agree with `Rails::HTML5::FullSanitizer`).
#   * `Sound::Image` — a superclass EXPRESSION is carried now: the
#     anonymous `Struct.new(...)` gets a name.
#   * `Enumerable#index_by` — grounded to `ActiveSupport.index_by(list)`.
#   * `Turbo::Streams::StreamName` — `runtime/turbo_streams.rb`, the
#     third end of the `--unsigned` wire the other two already shared.
#
# And a seventh, a LATE PATCH rather than a constant: the
# `Message.with_attachment_details` override. The macro-generated
# preload scopes it stood in for (`with_attached_attachment`,
# `with_rich_text_body_and_embeds`) are in the scope registry now, so
# the emitted method threads its relation instead of dropping it — the
# bug that made `/rooms/1` serve room 5's messages.
#
# THE DELETIONS ARE NOT OPTIONAL BOOKKEEPING. A stub that outlives its
# gap is not inert: `class Image < Struct.new(...)` here, against the
# emit's `class Image < Sound::ImageStruct`, is a superclass mismatch
# TypeError at splice time — the whole file fails to load and the suite
# reports 0 of 240 rather than one broken test.

# RETIRED: the `sentry-ruby` gem. `project.rs::RUNTIME_GEMS` declares it
# in the emitted Gemfile now, from the `Sentry` constant `app/` names,
# and the CRuby tree's `gem_facades.rb` guarded-requires the real gem —
# which loads. A gem the app depends on is a Gemfile line, not a stub.
