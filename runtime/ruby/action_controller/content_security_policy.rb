# frozen_string_literal: true
# Request-time policy copies and final-response CSP completion.
# Controller declaration admission remains target-specific.
module ActionController
  class Base
    def current_content_security_policy
      r = request
      return ActionDispatch::ContentSecurityPolicy.new if r.nil?
      existing = r.content_security_policy
      return ActionDispatch::ContentSecurityPolicy.new if existing.nil?
      existing.dup
    end

    def complete_content_security_policy
      return if status == 304
      # Preserve manual headers, including on a 304 response.
      return unless headers["Content-Security-Policy"].nil?
      return unless headers["Content-Security-Policy-Report-Only"].nil?
      r = request
      return if r.nil?
      policy = r.content_security_policy
      # Do not allocate a policy for a request without a declaration.
      return if policy.nil?
      headers["Content-Security-Policy"] = policy.build
      nil
    end
  end
end
