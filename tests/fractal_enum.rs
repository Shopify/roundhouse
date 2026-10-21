//! `inline-enum-predicates` at build time: which calls it rewrites, to
//! what, and everything it declines because the build cannot show the
//! rewrite equivalent. `tests/fractal_runtime.rs` runs the result.

use std::fs;
use std::path::{Path, PathBuf};

use roundhouse::analyze::Analyzer;
use roundhouse::emit::fractal::{self, Pass, Plan};

const ORDER_RB: &str = "app/models/order.rb";

const LOCK: &str = "GEM\n  remote: https://rubygems.org/\n  specs:\n    activerecord (8.1.4)\n\nPLATFORMS\n  ruby\n\nDEPENDENCIES\n  activerecord\n\nBUNDLED WITH\n   2.6.9\n";

const SCHEMA: &str = r#"ActiveRecord::Schema.define do
  create_table "orders", force: :cascade do |t|
    t.integer "status"
    t.string "kind"
    t.boolean "archived"
  end
end
"#;

const ORDER: &str = r#"class Order < ApplicationRecord
  enum :status, { pending: 0, paid: 1 }
  enum :kind, { retail: "r", wholesale: "w" }, prefix: true

  def shippable?
    paid? && !archived?
  end

  def describe
    return "kind" if kind_retail?
    !pending? ? "moving" : "waiting"
  end

  private def secret = paid?

  def in_a_block
    [1].map { paid? }
  end

  def guarded
    defined?(paid?)
  end

  def self.on_the_class
    paid?
  end

  def shadowed
    status_for_database = 3
    paid?
  end

  def with_receiver(other)
    other.paid? || self.paid?
  end
end
"#;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("roundhouse-fractal-enum-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_dir(name: &str, extra: &[(&str, &str)]) -> PathBuf {
    let dir = scratch(name).join("app");
    let mut files: Vec<(&str, &str)> = vec![
        ("Gemfile.lock", LOCK),
        ("db/schema.rb", SCHEMA),
        ("app/models/application_record.rb", "class ApplicationRecord < ActiveRecord::Base\n  self.abstract_class = true\nend\n"),
        ("app/models/order.rb", ORDER),
    ];
    for (path, content) in extra {
        files.retain(|(p, _)| p != path);
        files.push((path, content));
    }
    for (path, content) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

/// Plan as the CLI does: ingest only what the scope names, so a file
/// elsewhere in the app that ingest cannot read does not stop the build.
fn build(dir: &Path, files: &[&str]) -> Plan {
    let root = dir.canonicalize().unwrap();
    let compiled: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    let pass = fractal::enum_predicates::EnumPredicates::default();
    let passes: [&dyn Pass; 1] = [&pass];
    let fractal::Ingest::Files { files, .. } = fractal::scope::decide(&root, &compiled, &passes, false).unwrap() else {
        panic!("expected a scoped ingest")
    };
    let fs = roundhouse::vfs::FsVfs::new();
    let mut app = roundhouse::ingest::app::ingest_app_with_vfs(&roundhouse::vfs::ScopedVfs::new(&fs, files), &root).unwrap();
    Analyzer::new(&app).analyze(&mut app);
    fractal::plan(&app, &root, &compiled, &[], &passes).unwrap()
}

fn rewrites(plan: &Plan) -> Vec<(String, String)> {
    plan.report.rewrites.iter().map(|r| (r.from.clone(), r.to.clone())).collect()
}

fn declined(plan: &Plan) -> Vec<String> {
    plan.report.declined.iter().map(|d| format!("{}: {}", d.at, d.reason)).collect()
}

#[test]
fn calls_in_the_classs_own_instance_methods_become_rails_comparison() {
    let dir = app_dir("rewrites", &[]);
    let plan = build(&dir, &[ORDER_RB]);
    let pair = |from: &str, to: &str| (from.to_string(), to.to_string());
    assert_eq!(
        rewrites(&plan),
        [
            pair("paid?", "(status_for_database == 1)"),
            pair("kind_retail?", "(kind_for_database == \"r\".freeze)"),
            pair("pending?", "(status_for_database == 0)"),
            pair("paid?", "(status_for_database == 1)"),
        ]
    );
    let text = &plan.rewritten[0].1;
    assert!(text.contains("!(status_for_database == 0) ? \"moving\""), "{text}");
    assert!(text.contains("private def secret = (status_for_database == 1)"), "{text}");
    // Left alone: in a block, under defined?, on the class, with a receiver.
    for kept in ["[1].map { paid? }", "defined?(paid?)", "def self.on_the_class\n    paid?", "other.paid? || self.paid?"] {
        assert!(text.contains(kept), "{kept} in {text}");
    }
    assert!(declined(&plan).iter().any(|d| d.contains("order.rb:30:5: the method has a local variable `status_for_database`")), "{:?}", declined(&plan));
    assert_eq!(text.lines().count(), ORDER.lines().count());
}

#[test]
fn a_frozen_file_compares_against_its_own_literal() {
    let frozen = format!("# frozen_string_literal: true\n\n{ORDER}");
    let dir = app_dir("frozen", &[("app/models/order.rb", &frozen)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(rewrites(&plan).iter().any(|(_, to)| to == "(kind_for_database == \"r\")"), "{:?}", rewrites(&plan));
}

#[test]
fn a_file_not_compiled_is_not_rewritten() {
    let dir = app_dir("unselected", &[("app/models/note.rb", "class Note < ApplicationRecord\nend\n")]);
    let plan = build(&dir, &["app/models/note.rb"]);
    assert!(plan.rewritten.is_empty() && plan.report.declined.is_empty());
}

#[test]
fn an_app_definition_of_the_predicate_or_its_reader_declines_it() {
    let concern = "module Paying\n  def paid?\n    true\n  end\nend\n";
    let dir = app_dir("defined", &[("app/models/concerns/paying.rb", concern)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(!rewrites(&plan).iter().any(|(from, _)| from == "paid?"), "{:?}", rewrites(&plan));
    assert!(declined(&plan).iter().any(|d| d.contains("the app defines `paid?` itself (app/models/concerns/paying.rb:2)")), "{:?}", declined(&plan));
    // The other predicates are unaffected.
    assert!(rewrites(&plan).iter().any(|(from, _)| from == "pending?"));

    let reader = "class Audit\n  attr_reader :status_for_database\nend\n";
    let dir = app_dir("reader", &[("app/models/audit.rb", reader)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(!rewrites(&plan).iter().any(|(_, to)| to.contains("status_for_database")), "{:?}", rewrites(&plan));
    assert!(declined(&plan).iter().any(|d| d.contains("the app defines `status_for_database` itself")), "{:?}", declined(&plan));
}

#[test]
fn another_enum_producing_the_same_predicate_declines_it() {
    let refund = "class Refund < ApplicationRecord\n  enum :state, [:paid, :void]\nend\n";
    let dir = app_dir("duplicate", &[("app/models/refund.rb", refund)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(declined(&plan).iter().any(|d| d.contains("another enum in the app also defines `paid?`")), "{:?}", declined(&plan));
    assert!(!rewrites(&plan).iter().any(|(from, _)| from == "paid?"));
}

#[test]
fn an_unreadable_enum_below_the_class_declines_everything_it_could_shadow() {
    let sub = "class RushOrder < Order\n  enum :speed, SPEEDS\nend\n";
    let dir = app_dir("subclass", &[("app/models/rush_order.rb", sub)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(plan.rewritten.is_empty(), "{:?}", rewrites(&plan));
    assert!(declined(&plan).iter().any(|d| d.contains("declares an enum this pass cannot read (app/models/rush_order.rb)")), "{:?}", declined(&plan));
    // The same enum in an unrelated class does not matter.
    let other = "class Shipment < ApplicationRecord\n  enum :speed, SPEEDS\nend\n";
    let dir = app_dir("unrelated", &[("app/models/shipment.rb", other)]);
    assert_eq!(plan_rewrite_count(&dir), 4);
}

fn plan_rewrite_count(dir: &Path) -> usize {
    build(dir, &[ORDER_RB]).report.rewrites.len()
}

#[test]
fn computed_method_names_in_the_class_decline_it() {
    let order = ORDER.replace("  def shippable?", "  %w[a b].each { |n| define_method(\"#{n}?\") { true } }\n\n  def shippable?");
    let dir = app_dir("computed", &[("app/models/order.rb", &order)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(plan.rewritten.is_empty(), "{:?}", rewrites(&plan));
    assert!(declined(&plan).iter().any(|d| d.contains("defines methods with computed names (app/models/order.rb:5)")), "{:?}", declined(&plan));
}

#[test]
fn declarations_it_cannot_reproduce_are_declined_with_why() {
    let order = "class Order < ApplicationRecord\n  enum :status, { pending: :p, paid: :q }\n  enum kind: { retail: 0 }\n\n  def check\n    paid? || retail?\n  end\nend\n";
    let dir = app_dir("unreadable", &[("app/models/order.rb", order)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(plan.rewritten.is_empty());
    let declined = declined(&plan);
    assert!(declined.iter().any(|d| d.contains("order.rb:2:3: `pending` has a symbol value")), "{declined:?}");
    assert!(declined.iter().any(|d| d.contains("order.rb:3:3: the keyword form")), "{declined:?}");
}

#[test]
fn rails_older_than_7_1_or_unpinned_declines_the_pass() {
    let dir = app_dir("old-rails", &[("Gemfile.lock", &LOCK.replace("8.1.4", "7.0.8"))]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(plan.rewritten.is_empty());
    assert!(declined(&plan)[0].contains("activerecord 7.0.8 is older than 7.1"), "{:?}", declined(&plan));

    let dir = app_dir("no-lock", &[]);
    fs::remove_file(dir.join("Gemfile.lock")).unwrap();
    let plan = build(&dir, &[ORDER_RB]);
    assert!(plan.rewritten.is_empty());
    assert!(declined(&plan)[0].contains("Gemfile.lock pins no activerecord"), "{:?}", declined(&plan));
}

#[test]
fn nested_namespace_subclasses_are_visible_to_shadowing_checks() {
    let sub = "class RushOrder < Order\n  enum :speed, SPEEDS\nend\n";
    for namespace in ["public", "log", "test", "spec", "tmp", "coverage", "sorbet", "sig"] {
        let path = format!("app/models/{namespace}/rush_order.rb");
        let dir = app_dir(&format!("nested-{namespace}"), &[(&path, sub)]);
        let plan = build(&dir, &[ORDER_RB]);
        assert!(plan.rewritten.is_empty(), "{namespace}: {:?}", rewrites(&plan));
        assert!(declined(&plan).iter().any(|d| d.contains(&path)), "{namespace}: {:?}", declined(&plan));
    }
}

#[test]
fn nested_predicate_definitions_are_visible_but_root_test_files_are_not() {
    let predicate = "module Public\n  def paid? = true\nend\n";
    let dir = app_dir("nested-definition", &[("app/controllers/public/check.rb", predicate)]);
    let plan = build(&dir, &[ORDER_RB]);
    assert!(!rewrites(&plan).iter().any(|(from, _)| from == "paid?"));
    assert!(rewrites(&plan).iter().any(|(from, _)| from == "pending?"));
    for excluded in ["test", "spec", "tmp", "log", "public", "coverage", "sig", "sorbet", "node_modules", "app/node_modules"] {
        let path = format!("{excluded}/check.rb");
        let dir = app_dir(&format!("excluded-{}", excluded.replace('/', "-")), &[(&path, predicate)]);
        assert_eq!(plan_rewrite_count(&dir), 4, "{excluded}");
    }
}

#[test]
fn a_rebuild_does_not_index_its_previous_sparse_overlay() {
    let dir = app_dir("rebuild-overlay", &[]);
    let first = build(&dir, &[ORDER_RB]);
    let out = dir.join("out/fractal");
    fractal::write(&dir, &out, &first).unwrap();
    let second = build(&dir, &[ORDER_RB]);
    assert_eq!(rewrites(&first), rewrites(&second));
    assert_eq!(first.report.declined, second.report.declined);
}
