//! `--target fractal` (`emit::fractal`): the baseline every Fractal
//! transformation attaches to.
//!
//! What it promises with no transformation registered: the output (an
//! overlay of generated files) is empty, the selection is exactly the
//! named classes' own bodies, and every method in it is reported with
//! what the analyzer knows and whether a variant could be attached. With
//! a pass (here a test pass that swaps literal text) it promises that the
//! overlay holds only the initializer, no app file is touched, and the
//! variant carries the method's nesting, line and magic comment.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use roundhouse::analyze::Analyzer;
use roundhouse::emit::fractal::{self, Cx, Edit, Guard, INITIALIZER, Pass, Plan, Proposal, Region, Scope};
use roundhouse::ingest::ingest_app;
use roundhouse::span::Span;

const SCHEMA: &str = r#"ActiveRecord::Schema.define do
  create_table "accounts", force: :cascade do |t|
    t.string "name"
  end
end
"#;

const CURRENT: &str = r#"# frozen_string_literal: true

class Current < ActiveSupport::CurrentAttributes
  attribute :user

  def account
    Account.first
  end

  def label
    "original"
  end

  private def secret
    "original"
  end

  def self.build
    new
  end

  class << self
    def other
      "original"
    end
  end

  class Settings
    def account
      Account.first
    end
  end
end
"#;

/// A reopening in another file is the same class.
const CURRENT_REOPENED: &str = r#"class Current
  def room
    @room
  end
end
"#;

/// Shapes a variant cannot be attached to, next to ones it can.
const SHAPES: &str = r#"class Shapes
  def plain
    "original"
  end

  [1].each do
    def in_a_block; end
  end

  memoize def wrapped
    "original"
  end

  def located
    __dir__
  end

  def defines
    def inner; end
  end
end
"#;

/// Definitions on another object. Ingest records them as a `--survey`
/// gap, so the file is untrusted too; the method's own reason wins.
const FOREIGN: &str = r#"class Foreign
  def plain
    "original"
  end

  def OTHER.elsewhere; end

  class << OTHER
    def also_elsewhere; end
  end
end
"#;

/// `--class Outer --class Outer::Middle::Inner`: the innermost body decides.
const NESTED: &str = r#"class Outer
  def outer_label
    "original"
  end

  class Middle
    def middle_label
      "original"
    end

    class Inner
      def inner_label
        "original"
      end
    end
  end
end
"#;

const REPORT: &str = r#"# frozen_string_literal: true

module Admin
  NAME = "-report"

  class Report
    def title
      "original" + NAME
    end
  end
end
"#;

fn base_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("db/schema.rb", SCHEMA),
        ("app/models/account.rb", "class Account < ApplicationRecord\nend\n"),
        ("app/models/current.rb", CURRENT),
        ("app/models/current/reopened.rb", CURRENT_REOPENED),
        ("app/models/shapes.rb", SHAPES),
        ("app/models/outer.rb", NESTED),
        ("app/models/admin/report.rb", REPORT),
    ]
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("roundhouse-fractal-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = scratch(name).join("app");
    for (path, content) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

fn build_plan(dir: &Path, classes: &[&str], passes: &[&dyn Pass]) -> Result<Plan, String> {
    let mut app = ingest_app(dir).map_err(|e| e.to_string())?;
    Analyzer::new(&app).analyze(&mut app);
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    fractal::plan(&app, dir, &classes, &[], passes)
}

fn region<'r>(plan: &'r Plan, key_prefix: &str) -> &'r fractal::RegionReport {
    plan.report
        .regions
        .iter()
        .find(|r| r.key.starts_with(key_prefix))
        .unwrap_or_else(|| panic!("no region {key_prefix} in {:?}", plan.report.regions.iter().map(|r| &r.key).collect::<Vec<_>>()))
}

fn keys(plan: &Plan) -> Vec<String> {
    plan.report.regions.iter().map(|r| r.key.split('@').next().unwrap().to_string()).collect()
}

/// Every file under `dir`, relative to it.
fn files_in(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Swaps every occurrence of `from` inside the method's definition for
/// `to`. A test fixture that makes a variant observable, not an
/// optimization.
struct Swap {
    from: &'static str,
    to: &'static str,
    replayable: bool,
    guards: Vec<Guard>,
}

impl Swap {
    fn new(from: &'static str, to: &'static str) -> Swap {
        Swap { from, to, replayable: true, guards: Vec::new() }
    }
}

impl Pass for Swap {
    fn name(&self) -> &'static str {
        "test-swap"
    }

    fn scope(&self) -> Scope {
        Scope::Classes(Vec::new())
    }

    fn run(&self, cx: &Cx<'_>, region: &Region<'_>) -> Proposal {
        let text = &cx.app.sources[region.file.0 as usize - 1].text;
        let (start, end) = (region.def_span.start as usize, region.def_span.end as usize);
        let edits = text[start..end]
            .match_indices(self.from)
            .map(|(at, _)| Edit {
                span: Span { file: region.file, start: (start + at) as u32, end: (start + at + self.from.len()) as u32 },
                text: self.to.to_string(),
            })
            .collect();
        Proposal { edits, guards: self.guards.clone(), declined: Vec::new(), replayable: self.replayable }
    }
}

/// Proposes whatever edits it is given, for the driver's refusals.
struct Fixed(Box<dyn Fn(&Region<'_>) -> Vec<Edit>>);

impl Pass for Fixed {
    fn name(&self) -> &'static str {
        "test-fixed"
    }

    fn scope(&self) -> Scope {
        Scope::Classes(Vec::new())
    }

    fn run(&self, _cx: &Cx<'_>, region: &Region<'_>) -> Proposal {
        Proposal { edits: (self.0)(region), ..Proposal::default() }
    }
}

#[test]
fn no_class_and_no_pass_both_emit_nothing() {
    let dir = app_dir("identity", &base_files());
    for (name, classes) in [("none", vec![]), ("selected", vec!["Current", "Shapes"])] {
        let plan = build_plan(&dir, &classes, &[]).unwrap();
        assert!(plan.generated.is_empty(), "{name}: nothing to generate without a pass");
        assert!(plan.report.variants.is_empty());
        let out = scratch(&format!("identity-out-{name}")).join("out");
        assert_eq!(fractal::write(&out, &plan).unwrap(), 0);
        assert!(files_in(&out).is_empty(), "{name}: every file is the app's own");
    }
}

#[test]
fn the_selection_is_every_body_of_the_class_and_nothing_nested_in_it() {
    let dir = app_dir("selection", &base_files());
    let plan = build_plan(&dir, &["Current"], &[]).unwrap();
    assert_eq!(
        keys(&plan),
        ["Current#account", "Current#label", "Current#secret", "Current.build", "Current.other", "Current#room"],
    );
    assert_eq!(plan.report.bodies, ["Current app/models/current.rb:3", "Current app/models/current/reopened.rb:1"]);
}

#[test]
fn the_innermost_class_body_decides() {
    let dir = app_dir("nesting", &base_files());
    let plan = build_plan(&dir, &["Outer", "Outer::Middle::Inner"], &[]).unwrap();
    assert_eq!(keys(&plan), ["Outer#outer_label", "Outer::Middle::Inner#inner_label"]);
}

#[test]
fn a_class_the_app_does_not_define_is_an_error() {
    let dir = app_dir("unknown", &base_files());
    let err = build_plan(&dir, &["Curent"], &[]).err().expect("a misspelt class must not build the identity");
    assert!(err.contains("--class Curent"), "{err}");
}

#[test]
fn methods_a_variant_cannot_replace_are_reported_with_why() {
    let dir = app_dir("shapes", &base_files());
    let plan = build_plan(&dir, &["Shapes"], &[]).unwrap();
    let reason = |key: &str| region(&plan, key).reason.clone();
    assert_eq!(reason("Shapes#plain"), None);
    assert!(reason("Shapes#in_a_block").unwrap().contains("`each`"));
    assert!(reason("Shapes#wrapped").unwrap().contains("`memoize`"));
    assert!(reason("Shapes#located").unwrap().contains("`__dir__`"));
    assert!(reason("Shapes#defines").unwrap().contains("defines another method"));
}

#[test]
fn definitions_on_other_objects_are_reported_under_survey() {
    let mut files = base_files();
    files.push(("app/models/foreign.rb", FOREIGN));
    let dir = app_dir("foreign", &files);
    roundhouse::ingest::survey::activate();
    let app = ingest_app(&dir);
    let gaps = roundhouse::ingest::survey::drain();
    let mut app = app.unwrap();
    Analyzer::new(&app).analyze(&mut app);
    let untrusted: Vec<String> = gaps
        .iter()
        .filter_map(|g| match g {
            roundhouse::ingest::IngestError::Unsupported { file, .. } => Some(file.clone()),
            _ => None,
        })
        .collect();
    assert!(!untrusted.is_empty());
    let plan = fractal::plan(&app, &dir, &["Foreign".into()], &untrusted, &[]).unwrap();
    let reason = |key: &str| region(&plan, key).reason.clone().unwrap();
    assert!(reason("Foreign#plain").contains("ingest could not read"));
    assert!(reason("Foreign#elsewhere").contains("receiver other than the class"));
    assert!(reason("Foreign#also_elsewhere").contains("class << <expression>"));
}

#[test]
fn private_and_singleton_definitions_are_admitted() {
    let dir = app_dir("visibility", &base_files());
    let plan = build_plan(&dir, &["Current"], &[]).unwrap();
    for key in ["Current#secret", "Current.build", "Current.other"] {
        assert!(region(&plan, key).admitted, "{key}");
    }
}

#[test]
fn knowledge_reports_types_effects_and_whether_a_method_may_be_replayed() {
    let dir = app_dir("knowledge", &base_files());
    let plan = build_plan(&dir, &["Current"], &[]).unwrap();
    let label = &region(&plan, "Current#label").knowledge;
    assert_eq!((label.copies, label.typed, label.expressions), (1, 1, 1));
    assert!(label.replayable(), "{label:?}");
    // `Account.first` reads the database.
    let account = &region(&plan, "Current#account").knowledge;
    assert!(!account.no_recorded_effects && !account.replayable(), "{account:?}");
    // An ivar read the analyzer cannot type.
    let room = &region(&plan, "Current#room").knowledge;
    assert!(room.typed < room.expressions && !room.replayable(), "{room:?}");

    let swap = Swap::new("\"original\"", "\"variant\"");
    let plan = build_plan(&dir, &["Current"], &[&swap]).unwrap();
    let replayable: Vec<_> = plan.report.variants.iter().map(|v| (v.method.as_str(), v.replayable)).collect();
    assert!(replayable.contains(&("label", true)), "{replayable:?}");
}

#[test]
fn a_variant_keeps_nesting_magic_comment_and_line_and_the_app_keeps_its_files() {
    let dir = app_dir("variant", &base_files());
    let swap = Swap::new("\"original\"", "\"variant\"");
    let plan = build_plan(&dir, &["Admin::Report"], &[&swap]).unwrap();
    assert_eq!(plan.report.variants.len(), 1);
    let v = &plan.report.variants[0];
    assert_eq!((v.key.as_str(), v.line, v.passes.as_slice()), ("Admin::Report#title@app/models/admin/report.rb:7", 7, &["test-swap".to_string()][..]));

    let (path, init) = &plan.generated[0];
    assert_eq!(path, INITIALIZER);
    let source = "# frozen_string_literal: true\nmodule Admin\nclass Report\ndef title\n      \"variant\" + NAME\n    end\nend\nend\n";
    assert!(init.contains(source), "{init}");
    // Line 7 minus the three prelude lines: `def` evaluates at its own line.
    assert!(init.contains("eval_line: 4,"), "{init}");
    assert!(init.contains(&format!("file_size: {}, file_crc: {},", REPORT.len(), fractal::variant::crc32(REPORT.as_bytes()))), "{init}");
    assert!(init.starts_with(fractal::RUNTIME));
    assert!(init.ends_with("Fractal.attach(Rails.application)\n"));

    let out = scratch("variant-out").join("out");
    fractal::write(&out, &plan).unwrap();
    assert_eq!(files_in(&out), [INITIALIZER]);
    assert_eq!(fs::read_to_string(out.join(INITIALIZER)).unwrap(), *init);
    assert_eq!(fs::read_to_string(dir.join("app/models/admin/report.rb")).unwrap(), REPORT);
}

#[test]
fn a_method_no_pass_changed_keeps_the_apps_definition() {
    let dir = app_dir("unchanged", &base_files());
    let swap = Swap::new("\"absent\"", "\"variant\"");
    let plan = build_plan(&dir, &["Current"], &[&swap]).unwrap();
    assert!(plan.report.variants.is_empty() && plan.generated.is_empty());
}

#[test]
fn guards_from_passes_are_registered_and_the_verify_flag_needs_both_pass_and_analyzer() {
    let dir = app_dir("guards", &base_files());
    let guard = Guard::DefinedAt {
        constant: "Current".into(),
        method: "label".into(),
        singleton: false,
        file: "app/models/current.rb".into(),
        line: 10,
    };
    let swap = Swap { replayable: true, guards: vec![guard], ..Swap::new("Account.first", "Account.take") };
    let plan = build_plan(&dir, &["Current"], &[&swap]).unwrap();
    let init = &plan.generated[0].1;
    assert!(
        init.contains("guards: [{ kind: :defined_at, constant: 'Current', method: 'label', singleton: false, file: 'app/models/current.rb', line: 10 }]"),
        "{init}"
    );
    // The pass says replayable; the analyzer sees a database read.
    assert!(!plan.report.variants[0].replayable);
}

#[test]
fn edits_outside_the_definition_or_overlapping_are_refused() {
    let dir = app_dir("refusals", &base_files());
    let outside = Fixed(Box::new(|r: &Region<'_>| vec![Edit { span: Span { end: r.def_span.end + 1, ..r.def_span }, text: String::new() }]));
    let overlapping = Fixed(Box::new(|r: &Region<'_>| {
        let at = r.def_span.start;
        vec![
            Edit { span: Span { start: at, end: at + 3, ..r.def_span }, text: "def".into() },
            Edit { span: Span { start: at + 2, end: at + 4, ..r.def_span }, text: "f ".into() },
        ]
    }));
    for pass in [&outside, &overlapping] {
        let plan = build_plan(&dir, &["Outer"], &[pass]).unwrap();
        assert!(plan.report.variants.is_empty());
        assert_eq!(plan.report.declined.len(), 1, "{:?}", plan.report.declined);
        assert_eq!(plan.report.declined[0].pass, "test-fixed");
    }
}

#[test]
fn a_file_with_an_ingest_gap_keeps_the_apps_code() {
    let dir = app_dir("untrusted", &base_files());
    let mut app = ingest_app(&dir).unwrap();
    Analyzer::new(&app).analyze(&mut app);
    let swap = Swap::new("\"original\"", "\"variant\"");
    let plan = fractal::plan(&app, &dir, &["Outer".into()], &["app/models/outer.rb".into()], &[&swap]).unwrap();
    assert!(plan.report.variants.is_empty());
    assert!(region(&plan, "Outer#outer_label").reason.as_deref().unwrap().contains("ingest could not read"));
}

#[test]
fn an_app_that_defines_fractal_is_refused_once_there_is_a_variant() {
    let mut files = base_files();
    files.push(("app/models/fractal.rb", "class Fractal\nend\n"));
    let dir = app_dir("collision", &files);
    let swap = Swap::new("\"original\"", "\"variant\"");
    let err = build_plan(&dir, &["Outer"], &[&swap]).err().unwrap();
    assert!(err.contains("defines `Fractal`"), "{err}");
}

#[test]
fn a_rebuild_replaces_its_own_output_and_refuses_anything_else() {
    let dir = app_dir("rebuild", &base_files());
    let out = scratch("rebuild-out").join("out");
    let swap = Swap::new("\"original\"", "\"variant\"");
    fractal::write(&out, &build_plan(&dir, &["Outer"], &[&swap]).unwrap()).unwrap();
    assert_eq!(files_in(&out), [INITIALIZER]);
    // Deselected: the initializer goes, and the directories it left empty.
    fractal::write(&out, &build_plan(&dir, &[], &[]).unwrap()).unwrap();
    assert!(files_in(&out).is_empty());
    assert!(!out.join("config").exists());

    fs::write(out.join("stale.rb"), "").unwrap();
    let err = fractal::write(&out, &build_plan(&dir, &[], &[]).unwrap()).err().unwrap();
    assert!(err.contains("did not write (stale.rb"), "{err}");
    // Pointed at the app itself.
    let err = fractal::write(&dir, &build_plan(&dir, &[], &[]).unwrap()).err().unwrap();
    assert!(err.contains("a fractal build did not write"), "{err}");
    assert_eq!(fs::read_to_string(dir.join("app/models/current.rb")).unwrap(), CURRENT);
}

#[test]
fn the_cli_builds_reports_and_rejects_fractal_flags_elsewhere() {
    let dir = app_dir("cli", &base_files());
    let base = scratch("cli-out");
    let out = base.join("out");
    let report = base.join("report.json");
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_roundhouse")).args(args).output().unwrap();

    let ok = run(&["--target", "fractal", "--class", "Current", "--report", report.to_str().unwrap(), dir.to_str().unwrap(), "-o", out.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&ok.stderr);
    assert!(ok.status.success(), "{stderr}");
    assert!(stderr.contains("fractal: 6 method(s) in the selection, 6 admitted"), "{stderr}");
    assert!(stderr.contains("0 pass(es): none registered"), "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(json["classes"][0], "Current");
    assert_eq!(json["regions"].as_array().unwrap().len(), 6);
    assert!(files_in(&out).is_empty(), "no variant, so nothing to write");
    assert!(stderr.contains("emitted 0 file(s)"), "{stderr}");

    let misplaced = run(&["--target", "ruby", "--class", "Current", dir.to_str().unwrap()]);
    assert!(!misplaced.status.success());
    assert!(String::from_utf8_lossy(&misplaced.stderr).contains("only meaningful with --target fractal"));
}

// Scope: what a build ingests.

/// A pass that only declares a scope, for the driver's decision.
struct Scoped(fn() -> Scope);

impl Pass for Scoped {
    fn name(&self) -> &'static str {
        "test-scoped"
    }

    fn scope(&self) -> Scope {
        (self.0)()
    }

    fn run(&self, _cx: &Cx<'_>, _region: &Region<'_>) -> Proposal {
        Proposal::default()
    }
}

/// A file full ingest stops at: a migration verb the schema fold does
/// not take.
const UNREAD: &str = r#"class Unread < ActiveRecord::Migration[7.0]
  def change
    change_table :accounts do |t|
      t.string :slug
    end
  end
end
"#;

fn scoped_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("app/models/base_widget.rb", "class BaseWidget\n  def kind\n    \"base\"\n  end\nend\n"),
        ("app/models/concerns/labelled.rb", "module Labelled\n  def tag\n    \"tag\"\n  end\nend\n"),
        ("app/models/widget.rb", "class Widget < BaseWidget\n  include Labelled\n\n  def label\n    \"original\"\n  end\nend\n"),
        ("app/models/widget/reopened.rb", "class Widget\n  def other\n    \"original\"\n  end\nend\n"),
        ("app/models/unrelated.rb", "class Unrelated\n  def label\n    \"original\"\n  end\nend\n"),
        ("db/migrate/20240101000000_unread.rb", UNREAD),
        ("config/application.rb", "module App\n  class Application < Rails::Application\n  end\nend\n"),
    ]
}

fn decide(dir: &Path, classes: &[&str], passes: &[&dyn Pass], whole_app: bool) -> Result<fractal::Ingest, String> {
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    fractal::scope::decide(&dir.canonicalize().unwrap(), &classes, passes, whole_app)
}

/// The scope's files, relative to the app root.
fn scope_files(ingest: &fractal::Ingest, dir: &Path) -> Vec<String> {
    let fractal::Ingest::Files { files, .. } = ingest else { panic!("expected a scoped ingest") };
    let root = dir.canonicalize().unwrap();
    files.iter().map(|f| f.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect()
}

#[test]
fn a_build_ingests_only_the_selected_classes_and_the_apps_shape_by_default() {
    let dir = app_dir("scope-default", &scoped_files());
    let classes_only = Scoped(|| Scope::Classes(Vec::new()));
    let ingest = decide(&dir, &["Widget"], &[&classes_only], false).unwrap();
    assert_eq!(scope_files(&ingest, &dir), ["app/models/widget/reopened.rb", "app/models/widget.rb", "config/application.rb"]);
    let fractal::Ingest::Files { indexed, .. } = ingest else { unreachable!() };
    assert_eq!(indexed, 6, "Rubydex reads the app's Ruby, not db/");
    // No pass registered is the same: classes are the unit.
    assert_eq!(scope_files(&decide(&dir, &["Widget"], &[], false).unwrap(), &dir).len(), 3);
}

#[test]
fn ancestors_bring_in_the_files_defining_superclasses_and_mixins() {
    let dir = app_dir("scope-ancestors", &scoped_files());
    let pass = Scoped(|| Scope::Classes(vec![fractal::Reach::Ancestors]));
    let files = scope_files(&decide(&dir, &["Widget"], &[&pass], false).unwrap(), &dir);
    assert!(files.contains(&"app/models/base_widget.rb".to_string()), "{files:?}");
    assert!(files.contains(&"app/models/concerns/labelled.rb".to_string()), "{files:?}");
    assert!(!files.iter().any(|f| f.contains("unrelated") || f.contains("migrate")), "{files:?}");
}

#[test]
fn schema_signatures_and_custom_reaches_add_their_files() {
    let mut files = scoped_files();
    files.push(("db/schema.rb", SCHEMA));
    files.push(("sig/widget.rbs", "class Widget\n  def label: () -> String\nend\n"));
    let dir = app_dir("scope-reaches", &files);
    let pass = Scoped(|| {
        Scope::Classes(vec![
            fractal::Reach::Schema,
            fractal::Reach::Signatures,
            fractal::Reach::Custom(|cx, files| {
                files.extend(cx.index.files["Unrelated"].iter().map(PathBuf::from));
            }),
        ])
    });
    let scope = scope_files(&decide(&dir, &["Widget"], &[&pass], false).unwrap(), &dir);
    for expected in ["db/schema.rb", "sig/widget.rbs", "app/models/unrelated.rb"] {
        assert!(scope.contains(&expected.to_string()), "{expected} missing from {scope:?}");
    }
}

#[test]
fn a_pass_that_needs_the_whole_app_or_whole_app_reads_everything() {
    let dir = app_dir("scope-whole", &scoped_files());
    let whole = Scoped(|| Scope::WholeApp);
    let classes_only = Scoped(|| Scope::Classes(Vec::new()));
    match decide(&dir, &["Widget"], &[&classes_only, &whole], false).unwrap() {
        fractal::Ingest::WholeApp { reason } => assert_eq!(reason, "pass `test-scoped` needs the whole app"),
        fractal::Ingest::Files { .. } => panic!("a whole-app pass must widen the build"),
    }
    match decide(&dir, &["Widget"], &[&classes_only], true).unwrap() {
        fractal::Ingest::WholeApp { reason } => assert_eq!(reason, "--whole-app"),
        fractal::Ingest::Files { .. } => panic!("--whole-app must read everything"),
    }
}

#[test]
fn a_class_no_source_defines_is_an_error_before_anything_is_ingested() {
    let dir = app_dir("scope-missing", &scoped_files());
    let err = decide(&dir, &["Missing"], &[], false).err().unwrap();
    assert!(err.contains("--class Missing: no class or module of that name"), "{err}");
}

/// Ingest through the scope sees only its files, so a file whole-app
/// ingest stops at is never read, and the selected class plans exactly
/// as it does in a whole-app build.
#[test]
fn a_scoped_ingest_reads_only_its_files_and_plans_the_selection_the_same() {
    let dir = app_dir("scope-ingest", &scoped_files());
    assert!(ingest_app(&dir).is_err(), "the migration stops a whole-app ingest");
    let root = dir.canonicalize().unwrap();
    let fractal::Ingest::Files { files, .. } = decide(&dir, &["Widget"], &[], false).unwrap() else { unreachable!() };
    let fs = roundhouse::vfs::FsVfs::new();
    let vfs = roundhouse::vfs::ScopedVfs::new(&fs, files);
    let mut app = roundhouse::ingest::app::ingest_app_with_vfs(&vfs, &root).unwrap();
    let read: Vec<&str> = app.sources.iter().map(|s| s.path.as_str()).filter(|p| p.ends_with(".rb")).collect();
    assert!(read.iter().all(|p| !p.contains("unrelated") && !p.contains("migrate") && !p.contains("base_widget")), "{read:?}");
    Analyzer::new(&app).analyze(&mut app);
    let swap = Swap::new("original", "variant");
    let plan = fractal::plan(&app, &root, &["Widget".to_string()], &[], &[&swap]).unwrap();
    assert_eq!(keys(&plan), ["Widget#label", "Widget#other"]);
    assert_eq!(plan.report.variants.len(), 2);
}

/// A component: an in-repository gem with its own `app/`, which ingest
/// takes for an app root because its `lib/` declares a `Rails::Engine`.
#[test]
fn a_component_class_brings_its_engine_declaration_into_scope() {
    let mut files = scoped_files();
    files.retain(|(p, _)| !p.starts_with("db/"));
    files.push(("Gemfile.lock", "PATH\n  remote: components/billing\n  specs:\n    billing (0.1.0)\n\nGEM\n  remote: https://rubygems.org/\n  specs:\n\nDEPENDENCIES\n  billing!\n"));
    files.push(("components/billing/billing.gemspec", "Gem::Specification.new { |s| s.name = \"billing\" }\n"));
    files.push(("components/billing/lib/billing/engine.rb", "module Billing\n  class Engine < Rails::Engine\n  end\nend\n"));
    files.push(("components/billing/app/models/billing/invoice.rb", "module Billing\n  class Invoice\n    def label\n      \"original\"\n    end\n  end\nend\n"));
    let dir = app_dir("scope-component", &files);
    let ingest = decide(&dir, &["Billing::Invoice"], &[], false).unwrap();
    let scope = scope_files(&ingest, &dir);
    assert!(scope.contains(&"components/billing/lib/billing/engine.rb".to_string()), "{scope:?}");
    let fractal::Ingest::Files { files, .. } = ingest else { unreachable!() };
    let root = dir.canonicalize().unwrap();
    let fs = roundhouse::vfs::FsVfs::new();
    let mut app = roundhouse::ingest::app::ingest_app_with_vfs(&roundhouse::vfs::ScopedVfs::new(&fs, files), &root).unwrap();
    Analyzer::new(&app).analyze(&mut app);
    let plan = fractal::plan(&app, &root, &["Billing::Invoice".to_string()], &[], &[]).unwrap();
    assert_eq!(keys(&plan), ["Billing::Invoice#label"]);
}

#[test]
fn the_cli_scopes_by_default_and_reads_everything_with_whole_app() {
    let mut files = base_files();
    files.push(("app/models/unrelated.rb", "class Unrelated\n  def label\n    \"original\"\n  end\nend\n"));
    let dir = app_dir("scope-cli", &files);
    let out = scratch("scope-cli-out").join("out");
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_roundhouse")).args(args).output().unwrap();

    let scoped = run(&["--target", "fractal", "--class", "Shapes", dir.to_str().unwrap(), "-o", out.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&scoped.stderr);
    assert!(scoped.status.success(), "{stderr}");
    assert!(stderr.contains("fractal: ingesting 1 file(s) the selection and passes need"), "{stderr}");

    let whole = run(&["--target", "fractal", "--whole-app", "--class", "Shapes", dir.to_str().unwrap(), "-o", out.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&whole.stderr);
    assert!(whole.status.success(), "{stderr}");
    assert!(stderr.contains("fractal: ingesting the whole app (--whole-app)"), "{stderr}");

    let misplaced = run(&["--target", "ruby", "--whole-app", dir.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&misplaced.stderr).contains("only meaningful with --target fractal"));
}
