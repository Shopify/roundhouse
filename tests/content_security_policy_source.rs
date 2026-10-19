//! Source-consumer typing for the controller `content_security_policy` macro.
//! The macro is rewritten into a `before_action` block during ingest, so a
//! `roundhouse check` / LSP / MCP run that analyzes WITHOUT the post-analyze
//! lowering must still type the generated callback: the policy the block
//! receives, the `frame_src` directive it calls, and the `if:`/`unless:`
//! guards. These tests assert the policy receiver carries the exact
//! `ActionDispatch::ContentSecurityPolicy` type, and that a type error inside
//! an otherwise-admitted callback body or guard is reported and located -
//! not hidden by the former Unknown-item diagnostics gap.

use std::collections::HashMap;
use std::path::PathBuf;

use roundhouse::analyze::{diagnose, Analyzer};
use roundhouse::diagnostic::{Diagnostic, DiagnosticKind, Severity};
use roundhouse::dialect::ControllerBodyItem;
use roundhouse::expr::{Expr, ExprNode};
use roundhouse::ingest::{ingest_app_from_tree, prism};
use roundhouse::ty::Ty;
use roundhouse::{App, ClassId, Symbol};

/// A one-controller app whose `ApplicationController` body is `controller`,
/// plus a stable `index` action and a route to it. Mirrors the helper in
/// `tests/content_security_policy.rs` (a sibling crate test, not importable).
fn tree(controller: &str) -> HashMap<PathBuf, Vec<u8>> {
    let source = format!(
        "class ApplicationController < ActionController::Base\n{controller}\n  def index\n    render plain: 'stable'\n  end\nend\n"
    );
    HashMap::from([
        (
            PathBuf::from("db/schema.rb"),
            b"ActiveRecord::Schema.define do\nend\n".to_vec(),
        ),
        (
            PathBuf::from("app/controllers/application_controller.rb"),
            source.into_bytes(),
        ),
        (
            PathBuf::from("config/routes.rb"),
            b"Rails.application.routes.draw do\n  get '/policy', to: 'application#index'\nend\n".to_vec(),
        ),
    ])
}

/// Ingest + analyze (no lowering - the `check`/LSP/MCP path), returning the
/// analyzed app and the combined ingest + walker diagnostics.
fn analyze(controller: &str) -> (App, Vec<Diagnostic>) {
    let (result, mut diagnostics) = prism::scope(|| ingest_app_from_tree(tree(controller)));
    let mut app = result.expect("supported content_security_policy declaration");
    Analyzer::new(&app).analyze(&mut app);
    diagnostics.extend(diagnose(&app));
    (app, diagnostics)
}

/// The receiver expression of the first `frame_src` call anywhere in `expr`.
fn frame_src_receiver(expr: &Expr) -> Option<&Expr> {
    if let ExprNode::Send { recv: Some(recv), method, .. } = &*expr.node {
        if method.as_str() == "frame_src" {
            return Some(recv);
        }
    }
    let mut found: Option<&Expr> = None;
    expr.node.for_each_child(&mut |child| {
        if found.is_none() {
            found = frame_src_receiver(child);
        }
    });
    found
}

/// The receiver of the `frame_src` call inside a recognized CSP callback
/// (an `Unknown` controller body item), across every controller.
fn callback_frame_src_receiver(app: &App) -> Option<&Expr> {
    app.controllers.iter().find_map(|controller| {
        controller.body.iter().find_map(|item| match item {
            ControllerBodyItem::Unknown { expr, .. } => frame_src_receiver(expr),
            _ => None,
        })
    })
}

fn errors(diagnostics: &[Diagnostic]) -> Vec<&Diagnostic> {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

#[test]
fn callback_policy_receiver_is_the_typed_content_security_policy() {
    let (app, _diagnostics) = analyze(
        "content_security_policy do |policy|\n    policy.frame_src :self, 'https://embed.example'\n  end",
    );
    let receiver = callback_frame_src_receiver(&app)
        .expect("frame_src call present in the recognized CSP callback IR");
    match receiver.ty.as_ref() {
        Some(Ty::Class { id, args }) => {
            assert_eq!(
                id,
                &ClassId(Symbol::from("ActionDispatch::ContentSecurityPolicy")),
                "policy receiver must be the typed ContentSecurityPolicy value",
            );
            assert!(args.is_empty(), "policy type carries no type arguments");
        }
        other => panic!("expected ContentSecurityPolicy class receiver, got {other:?}"),
    }
}

#[test]
fn content_security_policy_responding_guard_and_body_type_without_errors() {
    // A guard that reads `params` and responds via `head`, plus a body that
    // calls `frame_src`: every call resolves, so no error is reported.
    let (_app, diagnostics) = analyze(
        "content_security_policy(if: -> { head :forbidden unless params[:apply] }) do |policy|\n    policy.frame_src :self, 'https://embed.example'\n  end",
    );
    assert!(
        errors(&diagnostics).is_empty(),
        "a resolvable CSP callback and guard must not report errors: {:?}",
        errors(&diagnostics),
    );
}

#[test]
fn content_security_policy_body_type_error_is_located() {
    // `1 + 'nope'` is isolable local work the block admits; its operands are
    // incompatible, so the analyzer must locate the error inside the callback.
    let (_app, diagnostics) = analyze(
        "content_security_policy do |policy|\n    policy.frame_src :self, (1 + 'nope')\n  end",
    );
    assert!(
        errors(&diagnostics).iter().any(|d| {
            matches!(d.kind, DiagnosticKind::IncompatibleBinop { .. }) && !d.span.is_synthetic()
        }),
        "a type error inside the CSP callback body must be reported and located: {diagnostics:?}",
    );
}

#[test]
fn content_security_policy_guard_type_error_is_located() {
    // The same incompatible arithmetic inside the `if:` guard lambda.
    let (_app, diagnostics) = analyze(
        "content_security_policy(if: -> { 1 + 'nope' }) do |policy|\n    policy.frame_src :self\n  end",
    );
    assert!(
        errors(&diagnostics).iter().any(|d| {
            matches!(d.kind, DiagnosticKind::IncompatibleBinop { .. }) && !d.span.is_synthetic()
        }),
        "a type error inside the CSP guard must be reported and located: {diagnostics:?}",
    );
}
