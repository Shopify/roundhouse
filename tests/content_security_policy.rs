use std::collections::HashMap;
use std::path::{Path, PathBuf};

use roundhouse::analyze::{Analyzer, diagnose};
use roundhouse::diagnostic::Severity;
use roundhouse::ingest::{ingest_app_from_tree, prism, survey};
use roundhouse::project::BuildTarget;

fn tree(controller: &str, extra: &[(&str, &str)]) -> HashMap<PathBuf, Vec<u8>> {
    let source = format!("class ApplicationController < ActionController::Base\n{controller}\n  def index\n    render plain: 'stable'\n  end\nend\n");
    let mut files = HashMap::from([
        (PathBuf::from("db/schema.rb"), b"ActiveRecord::Schema.define do\nend\n".to_vec()),
        (PathBuf::from("app/controllers/application_controller.rb"), source.into_bytes()),
        (PathBuf::from("config/routes.rb"), b"Rails.application.routes.draw do\n  get '/policy', to: 'application#index'\nend\n".to_vec()),
    ]);
    files.extend(extra.iter().map(|(p, s)| (PathBuf::from(p), s.as_bytes().to_vec())));
    files
}

#[test]
fn content_security_policy_unsupported_declarations_are_located_and_ledgered() {
    for declaration in [
        "content_security_policy { |policy| policy.script_src :self }",
        "content_security_policy { |policy| policy.frame_src -> { 'https://a.example' } }",
        "content_security_policy { |policy, other| policy.frame_src :self }",
        "content_security_policy { |*policies| policies.first.frame_src :self }",
        "content_security_policy { |policy| policy = nil }",
        "content_security_policy { |policy| policy, other = [nil, nil] }",
        "content_security_policy { |policy| policy.frame_src(*[-> { 'https://a.example' }]) }",
        "source = 'https://captured.example'; content_security_policy { |policy| policy.frame_src source }",
        "source = true; content_security_policy if: -> { source } do |policy| policy.frame_src :self end",
        "content_security_policy { |policy| [1].each { |policy| policy.to_s } }",
        "content_security_policy do |policy| [1].each do |item; policy| policy.frame_src :self end end",
        "content_security_policy { _1.frame_src :self }",
        "content_security_policy if: -> { _1 } do |policy| policy.frame_src :self end",
        "ApplicationController.content_security_policy { |policy| policy.frame_src :none }",
        "content_security_policy { |policy| return }",
        "content_security_policy { |policy| break }",
        "content_security_policy { |policy| next }",
        "content_security_policy ENV['CSP_ENABLED'] do |policy| policy.frame_src :self end",
        "content_security_policy true, false do |policy| policy.frame_src :self end",
        "content_security_policy prepend: true do |policy| policy.frame_src :self end",
        "content_security_policy if: ->(controller) { true } do |policy| policy.frame_src :self end",
        "content_security_policy if: -> { return true } do |policy| policy.frame_src :self end",
        "content_security_policy unless: -> { next false } do |policy| policy.frame_src :self end",
        "content_security_policy if: [:enabled?] do |policy| policy.frame_src :self end",
        "content_security_policy only: ENV['ACTION'] do |policy| policy.frame_src :self end",
        "with_options only: :index do; content_security_policy { |policy| policy.frame_src :self }; end",
        "content_security_policy_report_only false",
    ] {
        let files = tree(declaration, &[]);
        let (result, diagnostics) = prism::scope(|| ingest_app_from_tree(files.clone()));
        let error = result.as_ref().err().map(ToString::to_string);
        let located = diagnostics.iter().any(|d| d.severity == Severity::Error && d.span.start > 0);
        assert!(error.is_some() || located, "accepted unsupported declaration: {declaration}");
        if let Some(error) = error {
            assert!(error.contains("app/controllers/application_controller.rb"), "unlocated error: {error}");
            assert!(error.contains(":2") || diagnostics.iter().any(|d| d.span.start > 0), "missing line: {error}");
        }
        survey::activate();
        let (_, _) = prism::scope(|| ingest_app_from_tree(files));
        let gaps = survey::drain();
        assert!(gaps.iter().any(|gap| gap.to_string().contains("content_security_policy")), "missing survey gap: {declaration}: {gaps:?}");
    }
}

#[test]
fn content_security_policy_global_nonce_and_report_only_configuration_are_errors() {
    for statement in [
        "Rails.application.config.content_security_policy do |policy| policy.frame_src :self end",
        "Rails.application.config.content_security_policy_nonce_generator = ->(request) { 'nonce' }",
        "Rails.application.config.content_security_policy_report_only ||= true",
        "Rails.application.config.content_security_policy_report_only &&= false",
        "Rails.application.config.content_security_policy_nonce_directives += %w[style-src]",
        "Rails.application.config.content_security_policy_nonce_directives = %w[script-src]",
        "Rails.application.config.content_security_policy_report_only = true",
        "Rails.application.configure do\n  config.content_security_policy_report_only = false\nend",
        "Rails.application.configure do\n  config.content_security_policy_nonce_directives = nil\nend",
    ] {
        let files = tree("", &[("config/initializers/policy.rb", statement)]);
        let (result, diagnostics) = prism::scope(|| ingest_app_from_tree(files.clone()));
        assert!(result.is_err() || diagnostics.iter().any(|d| d.severity == Severity::Error), "configuration silently consumed: {statement}");
        if let Err(error) = result {
            assert!(error.to_string().contains("config/initializers/policy.rb"), "unlocated configuration error: {error}");
        }
        survey::activate();
        let (_, _) = prism::scope(|| ingest_app_from_tree(files));
        let gaps = survey::drain();
        assert!(gaps.iter().any(|gap| gap.to_string().contains("content_security_policy")), "missing config gap: {statement}");
    }
}

#[test]
fn content_security_policy_concern_and_custom_macro_declarations_remain_unsupported() {
    let cases = [
        tree("include PolicyConcern", &[("app/controllers/concerns/policy_concern.rb", "module PolicyConcern\n  extend ActiveSupport::Concern\n  included do\n    content_security_policy { |policy| policy.frame_src :self }\n  end\nend\n")]),
        tree("def self.install_policy\n  content_security_policy { |policy| policy.frame_src :self }\nend\ninstall_policy", &[]),
        tree("include PolicyConcern", &[("app/controllers/concerns/policy_concern.rb", "module PolicyConcern\n  extend ActiveSupport::Concern\n  included do |base|\n    base.content_security_policy { |policy| policy.frame_src :self }\n  end\nend\n")]),
        tree("def self.install_policy(base)\n  base.content_security_policy { |policy| policy.frame_src :self }\nend\ninstall_policy(self)", &[]),
    ];
    for files in cases {
        let (result, diagnostics) = prism::scope(|| ingest_app_from_tree(files.clone()));
        assert!(result.is_err() || diagnostics.iter().any(|d| d.severity == Severity::Error), "unsupported declaration expanded");
        survey::activate();
        let (_, _) = prism::scope(|| ingest_app_from_tree(files));
        let gaps = survey::drain();
        assert!(gaps.iter().any(|gap| gap.to_string().contains("content_security_policy")), "missing indirect declaration gap: {gaps:?}");
    }
}

#[test]
fn content_security_policy_source_consumers_resolve_callbacks_before_application_lowering() {
    let files = tree("content_security_policy if: -> { true }, only: :index do |policy|\n  policy.frame_src :self, *['https://embed.example']\nend", &[("config/initializers/policy.rb", "# Rails.application.config.content_security_policy_report_only = true\nlabel = 'content_security_policy_nonce_generator'\n")]);
    let (result, mut diagnostics) = prism::scope(|| ingest_app_from_tree(files));
    let mut app = result.expect("supported source consumer declaration");
    Analyzer::new(&app).analyze(&mut app);
    diagnostics.extend(diagnose(&app));
    let errors: Vec<_> = diagnostics.iter().filter(|d| d.severity == Severity::Error).collect();
    assert!(errors.is_empty(), "source consumers report errors: {errors:?}");
}

#[test]
fn content_security_policy_refuses_every_unverified_target() {
    for declaration in [
        "content_security_policy { |policy| policy.frame_src :self }",
        "content_security_policy false",
        "content_security_policy true",
    ] {
        let mut app = ingest_app_from_tree(tree(declaration, &[])).expect("supported declaration");
        roundhouse::session::analyze_and_lower(&mut app);
        for target in [BuildTarget::Jruby, BuildTarget::Spinel, BuildTarget::Roda, BuildTarget::Rust] {
            let (result, diagnostics) = roundhouse::emit::diagnostics::scope(|| roundhouse::project::target_files(&app, Path::new("."), target));
            assert!(result.is_err() || diagnostics.iter().any(|d| d.severity == Severity::Error && d.message.contains("content_security_policy")), "{target:?} falsely accepts CSP: {declaration}");
            assert!(diagnostics.iter().any(|d| d.severity == Severity::Error && d.span.start > 0 && d.message.contains("content_security_policy")), "missing located target refusal for {target:?}: {diagnostics:?}");
        }
    }
}

#[test]
fn content_security_policy_runtime_apis_cannot_bypass_target_admission() {
    for body in [
        "ActionDispatch::ContentSecurityPolicy.new",
        "request.content_security_policy = nil",
        "ActionDispatch::ContentSecurityPolicy::InvalidDirectiveError",
    ] {
        let mut files = tree("", &[]);
        files.insert(
            PathBuf::from("app/controllers/application_controller.rb"),
            format!("class ApplicationController < ActionController::Base\n  def index\n    {body}\n    render plain: 'stable'\n  end\nend\n").into_bytes(),
        );
        let mut app = ingest_app_from_tree(files).expect("ingest policy API");
        roundhouse::session::analyze_and_lower(&mut app);
        let (result, diagnostics) = roundhouse::emit::diagnostics::scope(||
            roundhouse::project::target_files(&app, Path::new("."), BuildTarget::Rust));
        assert!(result.is_err(), "unverified policy API target accepted {body}");
        assert!(diagnostics.iter().any(|diagnostic|
            diagnostic.severity == Severity::Error
                && diagnostic.message.contains("content_security_policy")
                && diagnostic.span.start > 0), "missing located API refusal: {diagnostics:?}");
    }
}

#[test]
fn content_security_policy_canonical_initializer_rejects_partial_policies() {
    for body in [
        "policy.script_src :self, dynamic_source",
        "policy.script_src :self, :unknown_source",
        "policy.not_a_directive :self",
        "policy.script_src -> { 'https://dynamic.example' }",
    ] {
        let initializer = format!("Rails.application.config.content_security_policy do |policy|\n  {body}\nend\n");
        let files = tree("", &[
            ("config/application.rb", "module Blog\n  class Application < Rails::Application\n  end\nend\n"),
            ("config/initializers/content_security_policy.rb", &initializer),
        ]);
        let error = ingest_app_from_tree(files.clone()).expect_err("partial global policy must fail");
        assert!(error.to_string().contains("config/initializers/content_security_policy.rb"), "{error}");
        survey::activate();
        let _ = prism::scope(|| ingest_app_from_tree(files));
        let gaps = survey::drain();
        assert!(gaps.iter().any(|gap| gap.to_string().contains("content_security_policy")), "{gaps:?}");
    }
}
