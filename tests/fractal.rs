//! `--target fractal`: query specialization scoped to the classes a person
//! names (`emit::fractal`).
//!
//! The contract is the one the plan states: removing a class from the
//! selection puts its original back (no `--class` is the identity), only
//! the selected class's own definitions are touched, and a site that
//! cannot be proven safe is left as written, with the reason reported.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use roundhouse::analyze::Analyzer;
use roundhouse::emit::fractal::{self, INITIALIZER};
use roundhouse::ingest::ingest_app_from_tree;

const SCHEMA: &str = r#"ActiveRecord::Schema.define do
  create_table "accounts", force: :cascade do |t|
    t.string "name"
  end
  create_table "rooms", force: :cascade do |t|
    t.string "name"
  end
end
"#;

const CURRENT: &str = r#"class Current < ActiveSupport::CurrentAttributes
  attribute :user

  def account
    Account.first
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
    Room.find_by(name: @room_name)
  end

  def named(name)
    Account.find_by(name: name.strip)
  end

  def latest(n)
    Room.order(:id).last(n)
  end

  def built_then_reassigned
    rel = Account.where(name: @name)
    @name = "other"
    rel.first
  end

  def shadowed(name)
    rel = Account.where(name: name)
    [1].map { |name| rel.first }
  end

  def built_and_kept
    rel = Room.where(name: @room_name)
    rel.first
  end
end
"#;

/// `--class Outer --class Outer::Middle::Inner`: the innermost body decides.
const NESTED: &str = r#"class Outer
  def outer_account
    Account.first
  end

  class Middle
    def middle_account
      Account.first
    end

    class Inner
      def inner_account
        Account.first
      end
    end
  end
end
"#;

const ROOMS_CONTROLLER: &str = r#"class RoomsController < ApplicationController
  def index
    @first = Account.first
  end
end
"#;

const ACCOUNT: &str = "class Account < ApplicationRecord\nend\n";

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("db/schema.rb", SCHEMA),
        ("app/models/account.rb", ACCOUNT),
        ("app/models/room.rb", "class Room < ApplicationRecord\nend\n"),
        ("app/models/current.rb", CURRENT),
        ("app/models/current/reopened.rb", CURRENT_REOPENED),
        ("app/controllers/rooms_controller.rb", ROOMS_CONTROLLER),
    ]
}

fn emit(files: &[(&str, &str)], classes: &[&str]) -> Result<(fractal::Report, Vec<(String, String)>), String> {
    let tree: HashMap<PathBuf, Vec<u8>> = files.iter().map(|(p, c)| (PathBuf::from(p), c.as_bytes().to_vec())).collect();
    let mut app = ingest_app_from_tree(tree).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let mut out: Vec<(String, String)> = files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect();
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    let report = fractal::specialize(&app, &mut out, &classes, &[])?;
    Ok((report, out))
}

fn file<'f>(files: &'f [(String, String)], path: &str) -> &'f str {
    &files.iter().find(|(p, _)| p == path).unwrap_or_else(|| panic!("{path} not emitted")).1
}

/// `text` with each site key's file-id suffix (`_f12`) removed, so the
/// assertions do not depend on the order ingest numbers files in.
fn unfiled(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("_f") {
        let (head, tail) = rest.split_at(i);
        let digits = tail[2..].chars().take_while(|c| c.is_ascii_digit()).count();
        let ends_key = tail[2 + digits..].starts_with([',', ')']);
        out.push_str(head);
        if digits > 0 && ends_key {
            rest = &tail[2 + digits..];
        } else {
            out.push_str("_f");
            rest = &tail[2..];
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn only_the_selected_class_is_rewritten() {
    let (report, out) = emit(&files(), &["Current"]).unwrap();
    // Both definitions of Current are selected, by Rubydex.
    assert_eq!(report.definitions.len(), 2, "{:?}", report.definitions);
    let current = unfiled(file(&out, "app/models/current.rb"));
    assert!(current.contains("    Fractal.records(:models_current_5_5, []) { Account.first }\n"), "{current}");
    // A class nested inside Current is another class: untouched.
    assert!(current.contains("    def account\n      Account.first\n    end\n  end\nend\n"), "{current}");
    // The same chain in a class nobody selected is untouched.
    assert_eq!(file(&out, "app/controllers/rooms_controller.rb"), ROOMS_CONTROLLER);
    // The reopening's plain-read bind is admitted.
    let reopened = unfiled(file(&out, "app/models/current/reopened.rb"));
    assert!(reopened.contains("Fractal.records(:models_current_reopened_3_5, [@room_name]) { Room.find_by(name: @room_name) }"), "{reopened}");
    // A relation built in a local, whose values nothing in the method
    // changes, is rewritten at its terminal.
    assert!(reopened.contains("Fractal.records(:models_current_reopened_27_5, [@room_name]) { rel.first }"), "{reopened}");
    let init = unfiled(file(&out, INITIALIZER));
    assert!(init.contains("Rails.application.config.to_prepare do\n  Fractal.prepare\n"), "{init}");
    assert!(init.contains("Fractal.define(:models_current_5_5, Account, one: true, via: :first) { |p| Account.all.send(:ordered_relation).limit(1) }"), "{init}");
    assert!(init.contains("Fractal.define(:models_current_reopened_3_5, Room, one: true, via: :find_by) { |p| Room.all.where(name: p.bind).limit(1) }"), "{init}");
    assert_eq!(report.specialized.len(), 3, "{:?}", report.specialized);
}

/// Keys carry the file id: two files whose stems coincide must not share
/// a statement.
#[test]
fn site_keys_differ_across_files_with_the_same_stem() {
    let mut f = files();
    f.push(("app/models/current_x.rb", "class Current
  def a
    Account.first
  end
end
"));
    f.push(("app/models/current/x.rb", "class Current
  def a
    Account.first
  end
end
"));
    let (_, out) = emit(&f, &["Current"]).unwrap();
    let init = file(&out, INITIALIZER);
    let keys: Vec<&str> = init
        .lines()
        .filter_map(|l| l.trim().strip_prefix("Fractal.define(:"))
        .map(|l| l.split(',').next().unwrap())
        .collect();
    assert!(keys.iter().filter(|k| k.starts_with("models_current_x_3_5_f")).count() == 2, "{keys:?}");
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), keys.len(), "{keys:?}");
}

#[test]
fn values_the_method_reassigns_or_a_block_shadows_are_refused_through_a_local() {
    let (report, out) = emit(&files(), &["Current"]).unwrap();
    let reopened = file(&out, "app/models/current/reopened.rb");
    assert!(reopened.contains("    @name = \"other\"\n    rel.first\n"), "{reopened}");
    assert!(reopened.contains("    [1].map { |name| rel.first }\n"), "{reopened}");
    let refused = |line: &str| {
        report.residue.iter().any(|r| r.contains(&format!("reopened.rb:{line}:")) && r.contains("the relation was built earlier"))
    };
    assert!(refused("17") && refused("22"), "{:?}", report.residue);
}

#[test]
fn the_innermost_class_body_decides_membership() {
    let mut f = files();
    f.push(("app/models/outer.rb", NESTED));
    let (report, out) = emit(&f, &["Outer", "Outer::Middle::Inner"]).unwrap();
    let outer = file(&out, "app/models/outer.rb");
    assert!(!outer.contains("def outer_account\n    Account.first"), "{outer}");
    assert!(outer.contains("def middle_account\n      Account.first\n"), "{outer}");
    assert!(!outer.contains("def inner_account\n        Account.first"), "{outer}");
    assert_eq!(report.definitions.iter().filter(|d| d.starts_with("Outer")).count(), 2, "{:?}", report.definitions);
}

/// A file ingest could not read faithfully keeps its text, and none of
/// its sites is rewritten.
#[test]
fn sites_in_untrusted_files_are_left_as_written() {
    let tree: HashMap<PathBuf, Vec<u8>> = files().iter().map(|(p, c)| (PathBuf::from(p), c.as_bytes().to_vec())).collect();
    let mut app = ingest_app_from_tree(tree).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let mut out: Vec<(String, String)> = files().iter().map(|(p, c)| (p.to_string(), c.to_string())).collect();
    let report = fractal::specialize(&app, &mut out, &["Current".to_string()], &["app/models/current.rb".to_string()]).unwrap();
    assert_eq!(file(&out, "app/models/current.rb"), CURRENT);
    assert!(report.residue.iter().any(|r| r.contains("current.rb:5:") && r.contains("ingest gap")), "{:?}", report.residue);
}

#[test]
fn strict_admission_leaves_unsafe_sites_as_written_and_says_why() {
    let (report, out) = emit(&files(), &["Current"]).unwrap();
    let reopened = file(&out, "app/models/current/reopened.rb");
    // `name.strip` is evaluated again when the fallback runs.
    assert!(reopened.contains("    Account.find_by(name: name.strip)\n"), "{reopened}");
    assert!(report.residue.iter().any(|r| r.contains("reopened.rb:7:") && r.contains("may not give the same value twice")), "{:?}", report.residue);
    // A request-time LIMIT cannot be a placeholder on Active Record 8.1.
    assert!(reopened.contains("    Room.order(:id).last(n)\n"), "{reopened}");
    assert!(report.residue.iter().any(|r| r.contains("reopened.rb:11:") && r.contains("LIMIT")), "{:?}", report.residue);
}

#[test]
fn an_app_defined_finder_is_not_bypassed() {
    let mut f = files();
    let account = "class Account < ApplicationRecord\n  def self.first\n    super || create!(name: \"default\")\n  end\nend\n";
    f.retain(|(p, _)| *p != "app/models/account.rb");
    f.push(("app/models/account.rb", account));
    let (report, out) = emit(&f, &["Current"]).unwrap();
    assert!(file(&out, "app/models/current.rb").contains("  def account\n    Account.first\n  end\n"));
    assert!(report.residue.iter().any(|r| r.contains("Account.first is defined by the app")), "{:?}", report.residue);
}

#[test]
fn no_class_is_the_identity_and_an_unknown_class_is_an_error() {
    let (report, out) = emit(&files(), &[]).unwrap();
    assert!(report.specialized.is_empty());
    for (path, content) in files() {
        assert_eq!(file(&out, path), content, "{path}");
    }
    assert!(!out.iter().any(|(p, _)| p == INITIALIZER));
    let err = emit(&files(), &["Curent"]).err().expect("a misspelt class must not build the identity");
    assert!(err.contains("Curent"), "{err}");
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("roundhouse-fractal-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_tree(root: &Path) {
    for (rel, content) in files() {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fs::write(root.join("Gemfile"), "source \"https://rubygems.org\"\ngem \"rails\"\n").unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::write(root.join("config/routes.rb"), "Rails.application.routes.draw do\nend\n").unwrap();
}

fn tree_of(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push((path.strip_prefix(root).unwrap().to_path_buf(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Select, then deselect: the second build is the app again, file for file.
#[test]
fn cli_selecting_and_deselecting_a_class_round_trips() {
    let src = scratch("src");
    write_tree(&src);
    let selected = scratch("selected");
    let run = Command::new(env!("CARGO_BIN_EXE_roundhouse"))
        .args(["--target", "fractal", "--class", "Current", "-o"])
        .arg(&selected)
        .arg(&src)
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("fractal: selected Current"), "{stderr}");
    assert!(selected.join(INITIALIZER).exists());

    let identity = scratch("identity");
    let run = Command::new(env!("CARGO_BIN_EXE_roundhouse"))
        .args(["--target", "fractal", "-o"])
        .arg(&identity)
        .arg(&src)
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(tree_of(&identity), tree_of(&src));

    let bad = Command::new(env!("CARGO_BIN_EXE_roundhouse"))
        .args(["--target", "rust", "--class", "Current"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!bad.status.success());
    for dir in [src, selected, identity] {
        fs::remove_dir_all(dir).unwrap();
    }
}
