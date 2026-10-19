use super::emit_and_run;

fn overlay() -> emit_and_run::Overlay {
    emit_and_run::real_blog()
        .write("app/controllers/application_controller.rb", r#"class ApplicationController < ActionController::Base
  layout false
end
"#)
        .write("config/routes.rb", r#"Rails.application.routes.draw do
  get "/direct", to: "direct#index"
  get "/child", to: "child#index"
  get "/inherited", to: "inherited#index"
end
"#)
        .write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  self.content_security_policy do |policy|
    policy.frame_src :self, "https://embed.example"
  end
  def index
    render plain: "stable body"
  end
end
"#)
        .write("app/controllers/parent_controller.rb", r##"class ParentController < ApplicationController
  content_security_policy do |policy|
    response.headers["X-Csp-Order"] = "#{response.headers["X-Csp-Order"]}P"
    policy.frame_src :none
  end
  def index
    render plain: "stable body"
  end
end
"##)
        .write("app/controllers/child_controller.rb", r##"class ChildController < ParentController
  content_security_policy do |policy|
    response.headers["X-Csp-Order"] = "#{response.headers["X-Csp-Order"]}C"
    policy.frame_src "https://child.example"
  end
end
"##)
        .write("app/controllers/inherited_controller.rb", "class InheritedController < ParentController\nend\n")
}

const REQUEST_HELPER: &str = r#"
def request_csp(path, query = "", extra = {})
  Main.run_rack({
    "REQUEST_METHOD" => "GET", "PATH_INFO" => path, "QUERY_STRING" => query,
    "HTTP_HOST" => "localhost", "rack.input" => StringIO.new("")
  }.merge(extra))
end

def header_csp(headers, name)
  pair = headers.find { |key, _| key.downcase == name.downcase }
  pair && pair[1]
end
"#;

#[test]
fn content_security_policy_direct_and_inherited_requests() {
    let script = format!("{REQUEST_HELPER}{}", r#"
status, headers, = request_csp("/direct")
raise "direct status #{status}" unless status == 200
raise "direct policy #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'self' https://embed.example"
status, headers, = request_csp("/child")
raise "child status #{status}" unless status == 200
raise "child policy #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src https://child.example"
raise "child callback order #{headers.inspect}" unless header_csp(headers, "X-Csp-Order") == "PC"
status, headers, = request_csp("/inherited")
raise "inherited status #{status}" unless status == 200
raise "inherited policy #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'none'"
raise "repeated inherited callback #{headers.inspect}" unless header_csp(headers, "X-Csp-Order") == "P"
puts "CSP direct and inherited requests passed"
"#);
    overlay().run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_sources_follow_request_configuration() {
    let app = overlay()
        .write("lib/embed_provider.rb", r#"class EmbedProvider
  def self.configured?
    !ENV["CSP_TEST_PROVIDERS"].nil?
  end
  def self.csp_frame_sources
    value = ENV["CSP_TEST_PROVIDERS"].to_s
    if value.empty?
      [:none]
    else
      value.split(",")
    end
  end
end
"#)
        .write("app/controllers/direct_controller.rb", r##"class DirectController < ApplicationController
  content_security_policy if: -> { EmbedProvider.configured? }, unless: :skip_policy? do |policy|
    policy.frame_src(*frame_sources)
  end
  def index
    render plain: "stable body"
  end
  private
  def skip_policy?
    params[:skip] == "yes"
  end
  def frame_sources
    response.headers["X-Sources"] = "#{response.headers["X-Sources"]}S"
    EmbedProvider.csp_frame_sources
  end
end
"##);
    let script = format!("{REQUEST_HELPER}{}", r#"
ENV.delete("CSP_TEST_PROVIDERS")
_, headers, = request_csp("/direct")
raise "absent table #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
raise "unguarded sources #{headers.inspect}" if header_csp(headers, "X-Sources")
ENV["CSP_TEST_PROVIDERS"] = "https://one.example,https://two.example"
_, headers, = request_csp("/direct")
raise headers.inspect unless header_csp(headers, "Content-Security-Policy") == "frame-src https://one.example https://two.example"
raise "source evaluated twice" unless header_csp(headers, "X-Sources") == "S"
ENV["CSP_TEST_PROVIDERS"] = ""
_, headers, = request_csp("/direct")
raise "empty table #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'none'"
ENV["CSP_TEST_PROVIDERS"] = "https://changed.example"
_, headers, = request_csp("/direct")
raise "stale table #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src https://changed.example"
_, headers, = request_csp("/direct", "skip=yes")
raise "unless guard #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
ENV.delete("CSP_TEST_PROVIDERS")
_, headers, = request_csp("/direct")
raise "request policy leaked #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_scopes_and_halted_callbacks_preserve_restrictions() {
    let app = overlay()
        .write("app/controllers/child_controller.rb", r##"class ChildController < ParentController
  content_security_policy only: :index, except: :index, if: :record_guard? do |policy|
    policy.frame_src
  end
  content_security_policy only: [], if: :record_guard? do |policy|
    policy.frame_src
  end
  content_security_policy only: :other, if: -> { record_guard? } do |policy|
    policy.frame_src
  end
  content_security_policy except: :index, unless: :record_guard? do |policy|
    policy.frame_src
  end
  private
  def record_guard?
    response.headers["X-Guard"] = "#{response.headers["X-Guard"]}G"
    true
  end
end
"##)
        .write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  before_action do
    if params[:redirect] == "yes"
      redirect_to "/child"
    else
      render plain: "halted"
    end
  end
  content_security_policy do |policy|
    response.headers["X-Late"] = "ran"
    policy.frame_src :self
  end
  def index
    render plain: "action"
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
_, headers, = request_csp("/child")
raise "scope removed inherited restriction #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'none'"
raise "guard order #{headers.inspect}" unless header_csp(headers, "X-Guard") == "G"
[["", 200], ["redirect=yes", 302]].each do |query, expected|
  status, headers, body = request_csp("/direct", query)
  raise "halt status #{status}" unless status == expected
  raise "late CSP callback #{headers.inspect}" if header_csp(headers, "X-Late") || header_csp(headers, "Content-Security-Policy")
  raise "action ran #{body.inspect}" if body.join == "action"
end
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_disabled_blocks_and_failed_assignment() {
    let app = overlay()
        .write("app/controllers/child_controller.rb", r#"class ChildController < ParentController
  content_security_policy false do |policy|
    response.headers["X-Disabled"] = "block ran"
    policy.frame_src :self
  end
end
"#)
        .write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  content_security_policy do |policy|
    policy.frame_src :none
  end
  content_security_policy do |policy|
    policy.frame_src :self
    raise ArgumentError, "policy callback failed"
  end
  def index
    render plain: "unreachable"
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
_, headers, = request_csp("/child")
raise "disabled block skipped #{headers.inspect}" unless header_csp(headers, "X-Disabled") == "block ran"
raise "disabled policy remains #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
require File.expand_path("app/controllers/direct_controller", Dir.pwd)
controller = DirectController.new
controller.request = ActionDispatch::Request.new({})
begin
  controller.process_action(:index)
  raise "callback did not fail"
rescue ArgumentError => error
  raise error unless error.message == "policy callback failed"
end
raise "failed callback assigned copy" unless controller.request.content_security_policy.build == "frame-src 'none'"
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_policy_copy_mapping_and_error_timing() {
    overlay().run_ruby(r#"
policy = ActionDispatch::ContentSecurityPolicy.new
policy.frame_src(:self, :none, :https, :unsafe_eval, :wasm_unsafe_eval, :unsafe_hashes,
  :unsafe_inline, :http, :data, :mediastream, :allow_duplicates, :blob, :filesystem,
  :report_sample, :script, :strict_dynamic, :ws, :wss)
expected = "frame-src 'self' 'none' https: 'unsafe-eval' 'wasm-unsafe-eval' 'unsafe-hashes' 'unsafe-inline' http: data: mediastream: 'allow-duplicates' blob: filesystem: 'report-sample' 'script' 'strict-dynamic' ws: wss:"
raise policy.build unless policy.build == expected
original = +"https://one.example"
policy.frame_src(original)
copy = policy.dup
original.replace("https://mutated.example")
raise "copy shared string" unless copy.build == "frame-src https://one.example"
copy.frame_src(:none)
raise "directive appended" unless copy.build == "frame-src 'none'"
raise "copy changed original" unless policy.build == "frame-src https://mutated.example"
copy.frame_src
raise "empty sources did not remove directive" unless copy.build == ""
[nil, false].each do |source|
  copy.frame_src(:self)
  copy.frame_src(source, :none)
  raise "first falsy source did not remove directive" unless copy.build == ""
end
copy.frame_src(:none)
begin
  copy.frame_src(:self, :unknown_csp_symbol)
  raise "unknown symbol accepted"
rescue ArgumentError
end
raise "failed mapping changed policy" unless copy.build == "frame-src 'none'"
begin
  copy.frame_src(:self, 42)
  raise "invalid source type accepted"
rescue ArgumentError
end
raise "invalid source changed policy" unless copy.build == "frame-src 'none'"
["https://a.example https://b.example", "https://a.example; frame-src *", "https://a.example\n", "https://a.example\u00a0"].each do |invalid|
  copy.frame_src(invalid)
  begin
    copy.build
    raise "invalid string accepted"
  rescue ActionDispatch::ContentSecurityPolicy::InvalidDirectiveError
  end
  copy.frame_src(:none)
  raise "replacement built stale invalid value" unless copy.build == "frame-src 'none'"
end
copy.frame_src("sha256-abc", "sha384-def", "sha512-ghi")
raise copy.build unless copy.build == "frame-src 'sha256-abc' 'sha384-def' 'sha512-ghi'"
"#).assert_passes();
}

#[test]
fn content_security_policy_completion_follows_conditional_get_and_manual_headers() {
    let app = overlay()
        .write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  content_security_policy do |policy|
    policy.frame_src params[:source].to_s
  end
  def index
    if params[:manual] == "enforce"
      response.headers["cOnTeNt-SeCuRiTy-PoLiCy"] = "manual enforce"
    end
    if params[:manual] == "report"
      response.headers["cOnTeNt-SeCuRiTy-PoLiCy-RePoRt-OnLy"] = "manual report"
    end
    render plain: "stable body"
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
status, headers, body = request_csp("/direct", "source=https://embed.example")
raise "first status #{status}" unless status == 200
raise "first body #{body.inspect}" unless body.join == "stable body"
raise "first CSP #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src https://embed.example"
etag = header_csp(headers, "ETag")
raise "missing etag" unless etag
status, headers, body = request_csp("/direct", "source=invalid%20source", "HTTP_IF_NONE_MATCH" => etag)
raise "conditional status #{status}" unless status == 304
raise "304 body #{body.inspect}" unless body.join == ""
raise "automatic 304 CSP #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
[["enforce", "Content-Security-Policy"], ["report", "Content-Security-Policy-Report-Only"]].each do |mode, name|
  status, headers, = request_csp("/direct", "source=invalid%20source&manual=#{mode}")
  raise "manual status #{status}" unless status == 200
  raise "manual header lost #{headers.inspect}" unless header_csp(headers, name) == "manual #{mode}"
  if mode == "report"
    raise "automatic header with manual report-only #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
  end
end
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_literal_declarations_and_integration_helper() {
    overlay()
        .write("app/controllers/application_controller.rb", r#"class ApplicationController < ActionController::Base
  layout false
  content_security_policy do |policy|
    response.headers["X-Csp-Order"] = "A"
    policy.frame_src :none
  end
end
"#)
        .write("app/controllers/child_controller.rb", r#"class ChildController < ParentController
  content_security_policy true
  content_security_policy false
end
"#)
        .write("app/controllers/inherited_controller.rb", r#"class InheritedController < ParentController
  content_security_policy true
  content_security_policy only: ["index"], except: ["other"], unless: -> { false } do |policy|
    policy.frame_src "invalid source"
    policy.frame_src false
    policy.frame_src :https
  end
end
"#)
        .write("test/controllers/csp_controller_test.rb", r#"require "test_helper"

class CspControllerTest < ActionDispatch::IntegrationTest
  test "controller policy reaches the integration response" do
    get "/direct"
    assert_response :success
    assert_equal "frame-src 'self' https://embed.example", response.headers["Content-Security-Policy"]
    get "/child"
    assert_nil response.headers["Content-Security-Policy"]
    get "/inherited"
    assert_equal "frame-src https:", response.headers["Content-Security-Policy"]
    assert_equal "AP", response.headers["X-Csp-Order"]
  end
end
"#)
        .run_test("test/controllers/csp_controller_test.rb")
        .assert_passes();
}

#[test]
fn content_security_policy_indirect_render_halts_later_callbacks() {
    let app = overlay().write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  content_security_policy do |policy|
    policy.frame_src(*frame_sources)
  end
  before_action do
    response.headers["X-Late"] = "ran"
  end
  def index
    render plain: "action"
  end
  private
  def frame_sources
    render plain: "source response"
    [:none]
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
status, headers, body = request_csp("/direct")
raise "source response lost #{body.inspect}" unless status == 200 && body.join == "source response"
raise "late callback ran #{headers.inspect}" if header_csp(headers, "X-Late")
raise "successful policy block not assigned #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'none'"
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_removal_replaces_the_inherited_directive() {
    let app = overlay().write("app/controllers/child_controller.rb", r#"class ChildController < ParentController
  content_security_policy do |policy|
    if policy.frame_src(:self)
      policy.frame_src
    end
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
_, headers, = request_csp("/child")
raise "directive removal lost policy ownership #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == ""
_, headers, = request_csp("/inherited")
raise "removal changed parent policy #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'none'"
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_callback_locals_do_not_share_frames_or_capture_application_names() {
    let app = overlay()
        .write("app/controllers/application_controller.rb", r#"class ApplicationController < ActionController::Base
  layout false
  before_action do
    current_content_security_policy = "application local"
    request = "application local"
    response = "application local"
    allowed = false
  end
  content_security_policy if: :allowed do |policy|
    value = "parent"
    policy.frame_src value
  end
  after_action do
    policy__csp0 ||= "application local"
    response.headers["X-Application-Local"] = policy__csp0.to_s
  end
  def allowed
    true
  end
end
"#)
        .write("app/controllers/direct_controller.rb", r##"class DirectController < ApplicationController
  content_security_policy do |policy|
    value = value || "child"
    policy.frame_src "https://#{value}.example"
  end
  def index
    render plain: "stable"
  end
end
"##);
    let script = format!("{REQUEST_HELPER}{}", r#"
_, headers, = request_csp("/direct")
raise "callback locals leaked #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src https://child.example"
raise "generated local captured application local #{headers.inspect}" unless header_csp(headers, "X-Application-Local") == "application local"
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_responding_guards_halt_only_executed_callbacks() {
    let app = overlay().write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  content_security_policy if: -> { head :forbidden; params[:apply] == "yes" } do |policy|
    policy.frame_src :none
  end
  def index
    response.headers["X-Action"] = "ran"
    head :ok unless performed?
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
status, headers, = request_csp("/direct", "apply=yes")
raise "guard status #{status}" unless status == 403
raise "guard did not halt action #{headers.inspect}" if header_csp(headers, "X-Action")
raise "executed callback missing policy #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "frame-src 'none'"
status, headers, = request_csp("/direct", "apply=no")
raise "false guard response #{status}" unless status == 403
raise "skipped callback changed halt behavior #{headers.inspect}" unless header_csp(headers, "X-Action") == "ran"
raise "skipped policy installed #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
"#);
    app.run_ruby(&script).assert_passes();
}

fn global_policy_overlay() -> emit_and_run::Overlay {
    overlay().write("config/initializers/content_security_policy.rb", r#"Rails.application.config.content_security_policy do |policy|
  policy.default_src :self
  policy.script_src :self
end
"#)
}

#[test]
fn content_security_policy_global_defaults_survive_controller_copies_and_explicit_clear() {
    let app = global_policy_overlay().write("app/controllers/child_controller.rb", r#"class ChildController < ParentController
  content_security_policy false
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
_, headers, = request_csp("/direct")
raise headers.inspect unless header_csp(headers, "Content-Security-Policy") == "default-src 'self'; script-src 'self'; frame-src 'self' https://embed.example"
_, headers, = request_csp("/child")
raise "global policy revived after clear #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
_, headers, = request_csp("/direct")
raise "clear leaked to next request #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "default-src 'self'; script-src 'self'; frame-src 'self' https://embed.example"
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_cached_concern_response_keeps_script_hashes_without_yield() {
    let app = global_policy_overlay()
        .write("app/controllers/concerns/cached_policy.rb", r#"module CachedPolicy
  extend ActiveSupport::Concern
  included do
    content_security_policy if: -> { request.content_security_policy } do |policy|
      sources = policy.directives["script-src"]
      if sources
        policy.directives["script-src"] = sources - ["'self'"] + ["'sha256-cached'"]
      end
      policy.worker_src "blob:"
    end
    around_action :serve_cached
  end
  def serve_cached
    render plain: "cached response"
  end
end
"#)
        .write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  include CachedPolicy
  def index
    response.headers["X-Action"] = "ran"
    render plain: "uncached"
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
status, headers, body = request_csp("/direct")
raise "cached response lost #{body.inspect}" unless status == 200 && body.join == "cached response"
raise "around filter yielded #{headers.inspect}" if header_csp(headers, "X-Action")
raise "cached script hashes lost #{headers.inspect}" unless header_csp(headers, "Content-Security-Policy") == "default-src 'self'; script-src 'sha256-cached'; worker-src blob:"
etag = header_csp(headers, "ETag")
status, headers, = request_csp("/direct", "", "HTTP_IF_NONE_MATCH" => etag)
raise "cached final status #{status}" unless status == 304
raise "automatic CSP on cached 304 #{headers.inspect}" if header_csp(headers, "Content-Security-Policy")
"#);
    app.run_ruby(&script).assert_passes();
}

#[test]
fn content_security_policy_global_guard_and_final_header_precedence_are_preserved() {
    let app = global_policy_overlay()
        .write("config/initializers/content_security_policy.rb", r#"unless ENV["DISABLE_CONTENT_SECURITY_POLICY"] == "true"
  Rails.application.config.content_security_policy do |policy|
    policy.script_src "invalid source"
  end
end
"#)
        .write("app/controllers/direct_controller.rb", r#"class DirectController < ApplicationController
  def index
    if params[:manual] == "yes"
      response.headers["Content-Security-Policy-Report-Only"] = "manual report"
    end
    render plain: "stable"
  end
end
"#);
    let script = format!("{REQUEST_HELPER}{}", r#"
ENV["DISABLE_CONTENT_SECURITY_POLICY"] = "true"
status, headers, = request_csp("/direct")
raise headers.inspect unless status == 200 && !header_csp(headers, "Content-Security-Policy")
etag = header_csp(headers, "ETag")
ENV["DISABLE_CONTENT_SECURITY_POLICY"] = "false"
status, headers, = request_csp("/direct", "manual=yes")
raise headers.inspect unless status == 200 && header_csp(headers, "Content-Security-Policy-Report-Only") == "manual report"
raise headers.inspect if header_csp(headers, "Content-Security-Policy")
status, headers, = request_csp("/direct", "", "HTTP_IF_NONE_MATCH" => etag)
raise "global policy built before final status #{status}" unless status == 304
raise headers.inspect if header_csp(headers, "Content-Security-Policy")
ENV.delete("DISABLE_CONTENT_SECURITY_POLICY")
"#);
    app.run_ruby(&script).assert_passes();
}
