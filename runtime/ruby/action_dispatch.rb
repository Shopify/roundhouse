# frozen_string_literal: true
require_relative "action_dispatch/router"
# Whether a render answers `Vary: Accept` (ruby-family dispatchers).
require_relative "action_dispatch/mime_negotiation"
require_relative "action_dispatch/flash"
require_relative "action_dispatch/session"
require_relative "action_dispatch/exceptions"
# X-Request-Id and X-Runtime (ActionDispatch::RequestId, Rack::Runtime),
# stamped by both ruby-family dispatchers around every app response.
require_relative "action_dispatch/request_id"
# ActionDispatch::ContentSecurityPolicy — the typed policy a controller
# CSP declaration builds and the response path serializes into the
# `Content-Security-Policy` header. Standalone (no dependency on the
# Request object), so it rides this aggregator, which both ruby-family
# dispatchers load before action_controller.
require_relative "action_dispatch/content_security_policy"
