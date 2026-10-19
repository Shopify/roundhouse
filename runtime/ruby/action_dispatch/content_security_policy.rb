# frozen_string_literal: true
# Shared policy storage for global literal policies, cached script hashes, and
# controller frame-src callbacks. Validate strings only when building a header.
module ActionDispatch
  class ContentSecurityPolicy
    # Rails validates source strings only when it builds the header.
    class InvalidDirectiveError < StandardError
    end

    MAPPINGS = {
      self: "'self'",
      unsafe_eval: "'unsafe-eval'",
      wasm_unsafe_eval: "'wasm-unsafe-eval'",
      unsafe_hashes: "'unsafe-hashes'",
      unsafe_inline: "'unsafe-inline'",
      none: "'none'",
      http: "http:",
      https: "https:",
      data: "data:",
      mediastream: "mediastream:",
      allow_duplicates: "'allow-duplicates'",
      blob: "blob:",
      filesystem: "filesystem:",
      report_sample: "'report-sample'",
      script: "'script'",
      strict_dynamic: "'strict-dynamic'",
      ws: "ws:",
      wss: "wss:",
    }.freeze

    def initialize
      @directives = {}
    end

    # Replace sources, or remove the directive when the first source is falsy.
    def frame_src(*sources)
      set_sources("frame-src", sources)
    end

    def directives
      self
    end

    def [](name)
      @directives[name]
    end

    def []=(name, sources)
      @directives[name] = sources
      nil
    end

    def set_directive(name, sources)
      set_sources(name, sources)
      nil
    end

    def worker_src(source)
      set_sources("worker-src", [source])
      nil
    end

    # Each callback gets its own mutable source arrays and strings.
    def dup
      other = ActionDispatch::ContentSecurityPolicy.new
      other.replace_directives(@directives)
      other
    end

    def build
      parts = []
      @directives.each do |directive, sources|
        validate(directive, sources)
        parts << "#{directive} #{sources.join(' ')}"
      end
      parts.join("; ")
    end

    # Used by dup on the new policy instance.
    def replace_directives(source_directives)
      source_directives.each do |directive, sources|
        @directives[directive] = sources.map { |source| source.dup }
      end
      nil
    end

    private
      def set_sources(name, sources)
        result = if sources.first
          @directives[name] = apply_mappings(sources)
        else
          @directives.delete(name)
        end
        result
      end

      def apply_mappings(sources)
        sources.map do |source|
          if source.is_a?(Symbol)
            apply_mapping(source)
          elsif source.is_a?(String)
            if hash_source?(source)
              "'#{source}'"
            else
              source
            end
          else
            raise ArgumentError, "Invalid content security policy source: a frame-src source must be a String or a supported Symbol"
          end
        end
      end

      def apply_mapping(source)
        mapped = MAPPINGS[source]
        raise ArgumentError, "Unknown content security policy source mapping: #{source.inspect}" if mapped.nil?
        mapped
      end

      def hash_source?(source)
        source.start_with?("sha256-") || source.start_with?("sha384-") || source.start_with?("sha512-")
      end

      def validate(directive, sources)
        sources.each do |source|
          if invalid_source?(source)
            raise InvalidDirectiveError, "Invalid Content Security Policy #{directive}: \"#{source}\". Directive values must not contain whitespace or semicolons. Please use multiple arguments or other directive methods instead."
          end
        end
        nil
      end

      def invalid_source?(source)
        source.match?(/[[:space:];]/)
      end
  end
end
