# `ActiveStorage::Previewer.poster` over ffmpeg — the ruby family's
# reopen of the shared definition (runtime/ruby/active_storage.rb),
# which raises. Rails' nested `Previewer::VideoPreviewer` is the
# inheritance target apps subclass; drawing still goes through this
# class-side `poster` with the same ffmpeg command Rails uses.
# Campfire's Dockerfile installs ffmpeg for it, and so does the archive's.
#
# The binary comes from `ActiveStorage.paths[:ffmpeg]` when set (Rails'
# config / test override); otherwise `"ffmpeg"` on PATH.
#
# The `-vf` filter comes from `ActiveStorage.video_preview_vf_filter`
# (Rails' `config.active_storage.video_preview_arguments`), so an app
# that adds `gte(t,5)` (campfire) draws the same frame Rails would.
# Default is Rails' select/keyframe/scene filter.
#
# When a registered previewer defines `TIME_LIMIT` (campfire's
# `TimeLimitedVideoPreviewer`), the draw is wrapped in `Timeout` and a
# stuck ffmpeg is killed — same contract as Rails' instance `capture`
# override, lifted onto the class-side poster this runtime uses.
#
# A tree without ffmpeg raises here, as Rails raises (`Errno::ENOENT`
# out of `IO.popen`): the honest answer is the missing program, not a
# blank poster. `IO.popen` rather than bare `system` so a timed-out
# child can be `Process.kill`'d.
require "timeout"

module ActiveStorage
  class Previewer
    def self.ffmpeg_bin
      path = ActiveStorage.paths[:ffmpeg]
      path.nil? || path.to_s.empty? ? "ffmpeg" : path.to_s
    end

    def self.ffprobe_bin
      path = ActiveStorage.paths[:ffprobe]
      path.nil? || path.to_s.empty? ? "ffprobe" : path.to_s
    end

    # Wall-clock limit from a previewer that defines `TIME_LIMIT`, or nil.
    def self.poster_time_limit
      ActiveStorage.previewers.each do |previewer|
        return previewer::TIME_LIMIT if previewer.const_defined?(:TIME_LIMIT)
      end
      nil
    end

    def self.poster(path)
      out = path + ".poster.png"
      filter = ActiveStorage.video_preview_vf_filter
      bin = ffmpeg_bin
      argv = [bin, "-y", "-loglevel", "error", "-i", path, "-vf", filter, "-frames:v", "1", "-f", "image2", out]
      limit = poster_time_limit
      ok = false
      begin
        if limit.nil?
          ok = system(*argv)
        else
          IO.popen(argv, err: File::NULL) do |io|
            begin
              Timeout.timeout(limit) { IO.copy_stream(io, File::NULL) }
            rescue Timeout::Error
              Process.kill(:KILL, io.pid) if io.pid
              raise ActiveStorage::PreviewError,
                    "#{bin} took longer than #{limit} seconds"
            end
          end
          ok = $?.success?
        end
      rescue Errno::ENOENT
        raise "ActiveStorage::Previewer: ffmpeg could not draw a poster for " + path +
              " — is ffmpeg installed? (campfire's Dockerfile installs it)"
      end
      if !ok || !File.exist?(out)
        raise "ActiveStorage::Previewer: ffmpeg could not draw a poster for " + path +
              " — is ffmpeg installed? (campfire's Dockerfile installs it)"
      end
      png = File.binread(out)
      File.delete(out)
      png
    end

    # Rails' `ActiveStorage::Analyzer::VideoAnalyzer`, its width and
    # height: ffprobe on the first video stream. campfire posts a video
    # with a poster only when its size is known (`too_many_pixels_to_
    # preview?` reads both), so a video with no dimensions was a video
    # with no poster. The bytes go to a file under the storage root
    # because ffprobe reads a path, and the answer comes back through
    # `-o`, for the same `system`-only reason as `poster`. No ffprobe, or
    # a stream it cannot read, answers `[0, 0]` — the metadata Rails
    # leaves empty when its analyzer fails. Rotation (Rails swaps width
    # and height for a 90/270 degree stream) is not read.
    PROBES = [0]

    def self.video_dimensions(data)
      PROBES[0] = PROBES[0] + 1
      service = ActiveStorage::Blob.service
      service.ensure_dir(service.root)
      path = service.root + "/.probe-" + Process.pid.to_s + "-" + PROBES[0].to_s
      out = path + ".txt"
      File.binwrite(path, data)
      ok = system(ffprobe_bin, "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height",
                  "-of", "csv=s=x:p=0", "-o", out, path)
      dims = [0, 0]
      if ok && File.exist?(out)
        parts = File.read(out).strip.split("x")
        dims = [parts[0].to_i, parts[1].to_i] if parts.length >= 2
      end
      File.delete(out) if File.exist?(out)
      File.delete(path) if File.exist?(path)
      dims
    end
  end
end
