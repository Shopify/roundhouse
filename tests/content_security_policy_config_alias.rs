//! Indirect global Content-Security-Policy configuration.
//!
//! A proven Rails configuration alias — a local bound from
//! `Rails.application.config` / `Rails.configuration`, or the parameter a
//! `tap` / `yield_self` / `configure` block yields — must refuse an
//! unsupported CSP setting with a located error and keep the survey gap, just
//! like the direct `Rails.application.config.content_security_policy` form.
//! Unrelated objects, reassigned aliases, and shadowed block parameters must
//! stay free of any CSP refusal. See
//! `src/ingest/content_security_policy_config.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use roundhouse::ingest::{ingest_app_from_tree, prism, survey};

/// A minimal Rails tree whose only Content-Security-Policy statement is the
/// supplied initializer; the controller declares none.
fn tree(initializer: &str) -> HashMap<PathBuf, Vec<u8>> {
    HashMap::from([
        (PathBuf::from("db/schema.rb"), b"ActiveRecord::Schema.define do\nend\n".to_vec()),
        (
            PathBuf::from("app/controllers/application_controller.rb"),
            b"class ApplicationController < ActionController::Base\n  def index\n    render plain: 'stable'\n  end\nend\n"
                .to_vec(),
        ),
        (
            PathBuf::from("config/routes.rb"),
            b"Rails.application.routes.draw do\n  get '/policy', to: 'application#index'\nend\n".to_vec(),
        ),
        (PathBuf::from("config/initializers/policy.rb"), initializer.as_bytes().to_vec()),
    ])
}

#[test]
fn indirect_content_security_policy_configuration_is_located_and_ledgered() {
    for initializer in [
        // `tap` forwards the configuration to its block parameter.
        "Rails.application.config.tap do |config|\n  config.content_security_policy do |policy|\n    policy.script_src :self\n  end\nend\n",
        // A local alias bound directly from `Rails.application.config`.
        "config = Rails.application.config\nconfig.content_security_policy_report_only = true\n",
        // Compound assignment through the proven alias.
        "config = Rails.application.config\nconfig.content_security_policy_report_only ||= true\n",
        "config = Rails.application.config\nconfig ||= SomeLibrary.config\nconfig.content_security_policy_report_only = true\n",
        "settings = nil\nsettings ||= Rails.application.config\nsettings.content_security_policy { |policy| policy.script_src :self }\n",
        "settings = false\nsettings ||= Rails.configuration\nsettings.content_security_policy_report_only = true\n",
        "settings = true\nsettings &&= Rails.application.config\nsettings.content_security_policy_report_only = true\n",
        "settings = Rails.application.config\nsettings &&= Rails.configuration\nsettings.content_security_policy_nonce_directives = %w[script-src]\n",
        "settings = nil\nsettings = settings || Rails.application.config\nsettings.content_security_policy_report_only = true\n",
        "settings = true\nsettings = settings && Rails.application.config\nsettings.content_security_policy_report_only = true\n",
        "config = Rails.application.config.tap { |value| value }\nconfig.content_security_policy_report_only = true\n",
        "Rails.application.config.tap { it.content_security_policy_report_only = true }\n",
        "Rails.application.config.then { _1.content_security_policy_report_only = true }\n",
        "Rails.application.config.tap do |settings = nil|\n  settings.content_security_policy_report_only = true\nend\n",
        // `yield_self` on an alias bound from `Rails.configuration`.
        "settings = Rails.configuration\nsettings.yield_self do |config|\n  config.content_security_policy_nonce_directives = %w[script-src]\nend\n",
        // `configure` yields the application; `app.config` is the configuration.
        "Rails.application.configure do |app|\n  app.config.content_security_policy_nonce_generator = ->(request) { 'nonce' }\nend\n",
    ] {
        let files = tree(initializer);
        let (result, _) = prism::scope(|| ingest_app_from_tree(files.clone()));
        let Err(error) = result else {
            panic!("indirect configuration silently consumed: {initializer}");
        };
        let text = error.to_string();
        assert!(text.contains("content_security_policy"), "unnamed construct: {text}");
        assert!(
            text.contains("config/initializers/policy.rb:"),
            "unlocated error: {text}"
        );

        survey::activate();
        let _ = prism::scope(|| ingest_app_from_tree(files));
        let gaps = survey::drain();
        assert!(
            gaps.iter().any(|gap| gap.to_string().contains("content_security_policy")),
            "missing survey gap: {initializer}"
        );
    }
}

#[test]
fn content_security_policy_alias_false_positives_are_not_refused() {
    for initializer in [
        // An unrelated object that merely exposes `config` / `content_security_policy`.
        "config = SomeLibrary.config\nconfig.content_security_policy do |policy|\n  policy.script_src :self\nend\n",
        // The alias is reassigned to a non-configuration object before use.
        "config = Rails.application.config\nconfig = ActiveSupport::OrderedOptions.new\nconfig.content_security_policy_report_only = true\n",
        // A block parameter shadows the alias with an unrelated value.
        "config = Rails.application.config\n[1].each do |config|\n  config.content_security_policy_report_only = true\nend\n",
    ] {
        let files = tree(initializer);
        let (result, _) = prism::scope(|| ingest_app_from_tree(files.clone()));
        result.expect("an unrelated initializer must not trigger CSP refusal");

        survey::activate();
        let _ = prism::scope(|| ingest_app_from_tree(files));
        let gaps = survey::drain();
        assert!(
            !gaps.iter().any(|gap| gap.to_string().contains("content_security_policy")),
            "false CSP survey gap: {initializer}"
        );
    }
}
