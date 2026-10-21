//! A fractal build against real Ruby: emit changed files through the CLI,
//! apply them to the base app, and compare its answers before and after
//! with Active Record and SQLite (`tests/support/fractal_equivalence.rb`).
//!
//! Needs `ruby` on PATH. Runs when `activerecord` and `sqlite3` are
//! installed, as CI's unit job does, and skips otherwise.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LOCK: &str = "GEM\n  remote: https://rubygems.org/\n  specs:\n    activerecord (8.1.4)\n\nPLATFORMS\n  ruby\n\nDEPENDENCIES\n  activerecord\n\nBUNDLED WITH\n   2.6.9\n";

const SCHEMA: &str = r#"ActiveRecord::Schema.define do
  create_table "orders", force: :cascade do |t|
    t.integer "status"
    t.string "kind"
    t.integer "size"
    t.boolean "archived", default: false
    t.string "type"
  end
end
"#;

const ORDER: &str = r#"# frozen_string_literal: true

class Order < ApplicationRecord
  enum :status, { pending: 0, paid: 1, shipped: 2 }
  enum :kind, { retail: "r", wholesale: "w" }, prefix: true, suffix: :sale
  enum :size, %i[small large], scopes: false

  def shippable?
    paid? && !archived?
  end

  def label
    if pending?
      "waiting"
    elsif shipped?
      "gone"
    else
      kind_retail_sale? ? "retail" : "other"
    end
  end

  def flags = [paid?, !shipped?, large?, kind_wholesale_sale?.to_s]

  private def hidden = small?

  public

  def reveal = hidden
end
"#;

/// An STI subclass that relies on the parent's predicates.
const RUSH: &str = r#"class RushOrder < Order
  def urgent? = paid? || pending?
end
"#;

/// A string-valued enum in a file without the magic comment.
const NOTE: &str = r#"class Note < ApplicationRecord
  self.table_name = "orders"
  enum :kind, { retail: "r", wholesale: "w" }

  def retail_label = retail? ? "R" : "W"
end
"#;

const PROBE: &str = r#"
out = []
Order.statuses.each_key do |status|
  Order.kinds.each_key do |kind|
    Order.sizes.each_key do |size|
      [true, false].each do |archived|
        o = Order.create!(status: status, kind: kind, size: size, archived: archived)
        out << [status, kind, size, archived, o.shippable?, o.label, o.flags, o.reveal]
      end
    end
  end
end
blank = Order.new
out << ["new", blank.shippable?, blank.label, blank.flags, blank.reveal]
rush = RushOrder.create!(status: :paid)
out << ["rush", rush.urgent?, rush.shippable?, rush.label]
out << ["note", Note.new(kind: :retail).retail_label, Note.new(kind: :wholesale).retail_label, Note.new.retail_label]
out
"#;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("roundhouse-fractal-runtime-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_app(dir: &Path) {
    for (path, content) in [
        ("Gemfile.lock", LOCK),
        ("db/schema.rb", SCHEMA),
        ("app/models/application_record.rb", "class ApplicationRecord < ActiveRecord::Base\n  self.abstract_class = true\nend\n"),
        ("app/models/order.rb", ORDER),
        ("app/models/rush_order.rb", RUSH),
        ("app/models/note.rb", NOTE),
        ("probe.rb", PROBE),
    ] {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// What the app at `root` answers, or `None` when Active Record or SQLite
/// is missing.
fn answers(root: &Path) -> Option<String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fractal_equivalence.rb");
    let out = Command::new("ruby").arg(&script).arg(root).output().expect("ruby on PATH");
    if out.status.code() == Some(2) {
        eprintln!("skipping: {}", String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    assert!(out.status.success(), "{} failed:\n{}", root.display(), String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8(out.stdout).unwrap())
}

#[test]
fn the_applied_overlay_answers_as_the_original_app_does() {
    let base = scratch("enum");
    let app = base.join("app");
    let out = base.join("out");
    write_app(&app);
    let build = Command::new(env!("CARGO_BIN_EXE_fractal"))
        .args(["emit", "--file", "app/models/order.rb", "--file", "app/models/rush_order.rb", "--file", "app/models/note.rb"])
        .arg(&app)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&build.stderr);
    assert!(build.status.success(), "{stderr}");

    // The build did rewrite: otherwise the comparison proves nothing.
    let order = fs::read_to_string(out.join("app/models/order.rb")).unwrap();
    assert!(order.contains("(status_for_database == 1) && !archived?"), "{order}");
    assert!(order.contains("(kind_for_database == \"r\") ? \"retail\""), "{order}");
    assert!(order.contains("private def hidden = (size_for_database == 0)"), "{order}");
    assert!(!out.join("app/models/rush_order.rb").exists(), "unchanged files stay in the base app");
    assert!(fs::read_to_string(out.join("app/models/note.rb")).unwrap().contains("(kind_for_database == \"r\".freeze) ? \"R\""));

    let Some(original) = answers(&app) else { return };
    let apply = Command::new(env!("CARGO_BIN_EXE_fractal")).arg("apply").arg(&out).arg(&app).output().unwrap();
    assert!(apply.status.success(), "{}", String::from_utf8_lossy(&apply.stderr));
    assert!(!app.join(roundhouse::emit::fractal::MANIFEST).exists());
    assert_eq!(fs::read_to_string(app.join("app/models/rush_order.rb")).unwrap(), RUSH);
    assert_eq!(original, answers(&app).unwrap());
}
