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
  create_table "articles", force: :cascade do |t|
    t.integer "status"
    t.string "kind"
    t.integer "size"
    t.boolean "archived", default: false
    t.string "type"
  end
end
"#;

const ARTICLE: &str = r#"# frozen_string_literal: true

class Article < ApplicationRecord
  enum :status, { draft: 0, published: 1, retracted: 2 }
  enum :kind, { news: "n", opinion: "o" }, prefix: true, suffix: :piece
  enum :size, %i[small large], scopes: false

  def listed?
    published? && !archived?
  end

  def label
    if draft?
      "unpublished"
    elsif retracted?
      "gone"
    else
      kind_news_piece? ? "news" : "other"
    end
  end

  def flags = [published?, !retracted?, large?, kind_opinion_piece?.to_s]

  private def hidden = small?

  public

  def reveal = hidden
end
"#;

/// An STI subclass that relies on the parent's predicates.
const FEATURED: &str = r#"class FeaturedArticle < Article
  def promoted? = published? || draft?
end
"#;

/// A string-valued enum in a file without the magic comment.
const NOTE: &str = r#"class Note < ApplicationRecord
  self.table_name = "articles"
  enum :kind, { news: "n", opinion: "o" }

  def news_label = news? ? "N" : "O"
end
"#;

const PROBE: &str = r#"
out = []
Article.statuses.each_key do |status|
  Article.kinds.each_key do |kind|
    Article.sizes.each_key do |size|
      [true, false].each do |archived|
        a = Article.create!(status: status, kind: kind, size: size, archived: archived)
        out << [status, kind, size, archived, a.listed?, a.label, a.flags, a.reveal]
      end
    end
  end
end
blank = Article.new
out << ["new", blank.listed?, blank.label, blank.flags, blank.reveal]
featured = FeaturedArticle.create!(status: :published)
out << ["featured", featured.promoted?, featured.listed?, featured.label]
out << ["note", Note.new(kind: :news).news_label, Note.new(kind: :opinion).news_label, Note.new.news_label]
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
        ("app/models/article.rb", ARTICLE),
        ("app/models/featured_article.rb", FEATURED),
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
        .args(["emit", "--file", "app/models/article.rb", "--file", "app/models/featured_article.rb", "--file", "app/models/note.rb"])
        .arg(&app)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&build.stderr);
    assert!(build.status.success(), "{stderr}");

    // The build did rewrite: otherwise the comparison proves nothing.
    let article = fs::read_to_string(out.join("app/models/article.rb")).unwrap();
    assert!(article.contains("(status_for_database == 1) && !archived?"), "{article}");
    assert!(article.contains("(kind_for_database == \"n\") ? \"news\""), "{article}");
    assert!(article.contains("private def hidden = (size_for_database == 0)"), "{article}");
    assert!(!out.join("app/models/featured_article.rb").exists(), "unchanged files stay in the base app");
    assert!(fs::read_to_string(out.join("app/models/note.rb")).unwrap().contains("(kind_for_database == \"n\".freeze) ? \"N\""));

    let Some(original) = answers(&app) else { return };
    let apply = Command::new(env!("CARGO_BIN_EXE_fractal")).arg("apply").arg(&out).arg(&app).output().unwrap();
    assert!(apply.status.success(), "{}", String::from_utf8_lossy(&apply.stderr));
    assert!(!app.join(roundhouse::emit::fractal::MANIFEST).exists());
    assert_eq!(fs::read_to_string(app.join("app/models/featured_article.rb")).unwrap(), FEATURED);
    assert_eq!(original, answers(&app).unwrap());
}
