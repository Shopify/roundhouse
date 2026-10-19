//! The `json-writer` Fractal pass (`emit::fractal::json_writer`): which
//! `to_json` sites it rewrites and which it reports and keeps, that a
//! variant differs from the app's method only inside the rewritten
//! spans and keeps every line where it was, that deselecting restores
//! the app, and, against real Active Support
//! (`tests/support/fractal_json_writer_test.rb`), that every probe gives
//! the original's bytes, encoding, exceptions, effects and warnings, that
//! the writer ran exactly where it should, and that a changed dependency
//! keeps the app's method.
//!
//! The runtime half needs `ruby` with minitest and `activesupport`, as
//! CI's unit job installs; without Active Support it reports a skip.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use roundhouse::analyze::Analyzer;
use roundhouse::emit::fractal::json_writer::JsonWriter;
use roundhouse::emit::fractal::{self, Cx, Pass, Plan, Selection, region};
use roundhouse::ingest::ingest_app;

const WIRE: &str = r#"# frozen_string_literal: true

class Wire
  def initialize(log = [])
    @log = log
  end

  def pair(label, count)
    l = label #: String
    c = count #: Integer
    { label: l, count: c }.to_json
  end

  def local(title, flag)
    t = title #: String?
    f = flag #: bool?
    payload = { title: t,
                flag: f,
                kind: "wire" }
    payload.to_json
  end

  def noted(a, b)
    { first: note(a),
      second: note(b) }.to_json
  end

  def fixed
    { "kind" => "wire", "version" => 2, ok: true, none: nil }.to_json
  end

  def reveal(count) = secret(count)

  # The inner encoding stays part of the outer value.
  def nested(label)
    l = label #: String
    { outer: { inner: l }.to_json }.to_json
  end

  def endless(count) = { count: whole(count) }.to_json

  private def secret(count)
    c = count #: Integer
    { count: c }.to_json
  end

  def note(value)
    @log << value
    raise ArgumentError, "refused #{value}" if value == "boom"

    value #: String
  end

  def whole(value)
    value #: Integer
  end
end
"#;

/// No magic comment: string literals are not frozen here.
const LOOSE: &str = r#"class Loose
  def fixed
    { kind: "loose" }.to_json
  end

  def pair(label)
    l = label #: String
    { label: l }.to_json
  end
end
"#;

/// Encoding sites the pass reports and leaves alone.
const KEPT: &str = r#"class Kept
  def generated(label)
    l = label #: String
    JSON.generate({ label: l })
  end

  def options(label)
    l = label #: String
    { label: l }.to_json(only: :label)
  end

  def safe(label)
    l = label #: String
    payload = { label: l }
    payload&.to_json
  end

  def duplicate(label)
    l = label #: String
    { label: l, label: l }.to_json
  end

  def splat(extra)
    { **extra }.to_json
  end

  def dynamic_key(name)
    { name => 1 }.to_json
  end

  def odd_key
    { "two words" => 1 }.to_json
  end

  def floaty
    { ratio: 1.5 }.to_json
  end

  def untyped(value)
    { value: value }.to_json
  end

  def watched(label)
    l = label #: String
    b = binding
    { label: l }.to_json
  end

  def heredoc(label)
    l = label #: String
    { label: l, text: <<~TEXT }.to_json
      body
    TEXT
  end

  def shared(label)
    l = label #: String
    payload = { label: l }
    remember(payload)
    payload.to_json
  end

  def nested(label)
    l = label #: String
    payload = { label: l }
    if l
      payload.to_json
    end
  end

  def param(payload)
    payload = { label: "x" }
    payload.to_json
  end

  def receiver(card)
    card.to_json
  end

  def implicit(label)
    { label: }.to_json
  end

  def remember(value)
    value
  end
end
"#;

const FILES: &[(&str, &str)] = &[("app/models/wire.rb", WIRE), ("app/models/loose.rb", LOOSE), ("app/models/kept.rb", KEPT)];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("roundhouse-fractal-json-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_dir(name: &str) -> PathBuf {
    let dir = scratch(name).join("app");
    for (path, content) in FILES {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

fn build_plan(dir: &Path, classes: &[&str]) -> Plan {
    let mut app = ingest_app(dir).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    fractal::plan(&app, dir, &classes, &[], &[&JsonWriter]).expect("plan")
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if dir.exists() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(files_in(&path).into_iter().map(|f| format!("{}/{f}", path.file_name().unwrap().to_string_lossy())));
            } else {
                out.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn it_rewrites_fixed_shapes_and_reports_every_site_it_keeps() {
    let dir = app_dir("plan");
    let plan = build_plan(&dir, &["Wire", "Loose", "Kept"]);
    let variants: Vec<&str> = plan.report.variants.iter().map(|v| v.key.split('@').next().unwrap()).collect();
    assert_eq!(variants, ["Loose#fixed", "Loose#pair", "Wire#pair", "Wire#local", "Wire#noted", "Wire#fixed", "Wire#nested", "Wire#endless", "Wire#secret"]);
    assert!(plan.report.variants.iter().all(|v| v.passes == ["json-writer"]));
    assert!(plan.report.variants.iter().all(|v| v.guards == [fractal::Guard::ActiveSupportJson]));

    let declined: Vec<(String, String)> = plan
        .report
        .declined
        .iter()
        .map(|d| (d.at.split(':').nth(1).unwrap().to_string(), d.reason.clone()))
        .collect();
    let line_of = |method: &str| {
        let at = KEPT.find(&format!("def {method}")).unwrap();
        KEPT[..at].matches('\n').count() + 1
    };
    let expect = |method: &str, offset: usize, reason: &str| {
        let line = (line_of(method) + offset).to_string();
        assert!(
            declined.iter().any(|(l, r)| *l == line && r.contains(reason)),
            "{method}: no decline at line {line} containing {reason:?} in {declined:#?}"
        );
    };
    expect("generated", 2, "native generator, and a Ruby writer measured slower");
    expect("options", 2, "called with options");
    expect("safe", 3, "called with `&.`");
    expect("duplicate", 2, "the key `label` appears twice");
    expect("splat", 1, "the hash has a splat");
    expect("dynamic_key", 1, "is not a plain symbol or string literal");
    expect("odd_key", 1, "is not a plain identifier");
    expect("floaty", 1, "it has Float");
    expect("untyped", 1, "the value of `value` (`value`)");
    expect("watched", 3, "the method calls `binding`");
    expect("heredoc", 2, "heredoc");
    expect("shared", 4, "`payload` is written 1 time(s) and read 2 time(s)");
    expect("nested", 4, "must both be statements of the method body");
    expect("param", 2, "`payload` is also a parameter");
    expect("receiver", 1, "`card` is not assigned a hash literal in this method");
    expect("implicit", 1, "is implicit");
    // Every Kept method with a site is reported, none rewritten.
    assert!(plan.report.variants.iter().all(|v| v.owner != "Kept"));
}

/// The variant of each method, out of the generated initializer.
fn variant_source(initializer: &str, key_prefix: &str) -> String {
    let at = initializer.find(&format!("key: '{key_prefix}")).unwrap_or_else(|| panic!("no variant {key_prefix}"));
    let rest = &initializer[at..];
    let open = rest.find("source: <<'").unwrap() + "source: <<'".len();
    let term_end = rest[open..].find('\'').unwrap();
    let terminator = &rest[open..open + term_end];
    let body_start = open + rest[open..].find('\n').unwrap() + 1;
    let body_end = body_start + rest[body_start..].find(&format!("\n{terminator}\n")).unwrap();
    rest[body_start..body_end].to_string()
}

#[test]
fn a_variant_differs_only_inside_the_rewritten_spans_and_keeps_every_line() {
    let dir = app_dir("spans");
    let mut app = ingest_app(&dir).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let selection = Selection::resolve(&app, &["Wire".to_string(), "Loose".to_string()]).unwrap();
    let root = dir.canonicalize().unwrap();
    let path_of = |file: roundhouse::span::FileId| {
        let path = PathBuf::from(&app.sources[file.0 as usize - 1].path);
        path.canonicalize().unwrap_or(path).strip_prefix(&root).ok().map(|p| p.to_string_lossy().into_owned())
    };
    let regions = region::find(&app, &selection, &path_of, &|_| false);
    let plan = fractal::plan(&app, &dir, &["Wire".to_string(), "Loose".to_string()], &[], &[&JsonWriter]).unwrap();
    let initializer = &plan.generated[0].1;
    let cx = Cx { app: &app };
    let mut checked = 0;
    for region in regions.iter().filter(|r| r.admission.is_ok()) {
        let proposal = JsonWriter.run(&cx, region);
        if proposal.edits.is_empty() {
            continue;
        }
        if region.name == "nested" {
            assert_eq!(proposal.edits.len(), 1, "only the outer site is rewritten");
        }
        let text = &app.sources[region.file.0 as usize - 1].text;
        let (start, end) = (region.def_span.start as usize, region.def_span.end as usize);
        let original = &text[start..end];
        // Each edit replaces a `{...}.to_json` call, a `name = {...}`
        // statement, or a `name.to_json` call, and nothing else.
        let mut expected = original.to_string();
        let mut edits = proposal.edits.clone();
        edits.sort_by_key(|e| std::cmp::Reverse(e.span.start));
        for edit in &edits {
            let replaced = &text[edit.span.start as usize..edit.span.end as usize];
            assert!(
                (replaced.starts_with('{') && replaced.ends_with(".to_json"))
                    || (replaced.starts_with("payload = {") && replaced.ends_with('}'))
                    || replaced == "payload.to_json",
                "{}: an edit replaces {replaced:?}",
                region.key()
            );
            assert_eq!(edit.text.matches('\n').count(), replaced.matches('\n').count(), "{}: lines moved", region.key());
            expected.replace_range(edit.span.start as usize - start..edit.span.end as usize - start, &edit.text);
        }
        let source = variant_source(initializer, &format!("{}#{}@", region.owner, region.name));
        assert!(source.contains(&expected), "{}:\n{source}\n---\n{expected}", region.key());
        assert_eq!(expected.lines().count(), original.lines().count(), "{}", region.key());
        checked += 1;
    }
    assert_eq!(checked, 9);
    for (path, content) in FILES {
        assert_eq!(fs::read_to_string(dir.join(path)).unwrap(), *content, "the build edited {path}");
    }
}

#[test]
fn deselecting_the_classes_gives_the_app_back() {
    let dir = app_dir("deselect");
    let out = scratch("deselect-out").join("out");
    let selected = build_plan(&dir, &["Wire"]);
    assert_eq!(fractal::write(&out, &selected).unwrap(), 1);
    assert_eq!(files_in(&out), ["config/initializers/fractal.rb"]);
    let none = build_plan(&dir, &[]);
    assert_eq!(fractal::write(&out, &none).unwrap(), 0);
    assert!(files_in(&out).is_empty(), "{:?}", files_in(&out));
}

#[test]
fn every_probe_matches_the_original_against_active_support() {
    let dir = app_dir("runtime");
    let plan = build_plan(&dir, &["Wire", "Loose"]);
    assert_eq!(plan.report.variants.len(), 9);
    let base = dir.parent().unwrap().to_path_buf();
    let overlay = base.join("overlay");
    fractal::write(&overlay, &plan).unwrap();
    let out = base.join("out");
    assert!(Command::new("cp").arg("-a").arg(&dir).arg(&out).status().unwrap().success());
    assert!(Command::new("cp").arg("-a").arg(format!("{}/.", overlay.display())).arg(&out).status().unwrap().success());
    let root = env!("CARGO_MANIFEST_DIR");
    let output = Command::new("ruby")
        .arg(format!("{root}/tests/support/fractal_json_writer_test.rb"))
        .env("FRACTAL_APP", &out)
        .output()
        .expect("ruby must be on PATH");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    if stdout.contains("SKIP:") {
        eprintln!("{stdout}");
    }
}
