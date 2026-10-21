//! The pass explorer's view of a build (`fractal::stages`), over an app
//! held in memory the way the browser holds it: stage 0 is the source,
//! each later stage the file after one more pass, with what that pass
//! rewrote and declined. The rewrites themselves are pinned by
//! `tests/fractal_enum.rs`; this pins the view and `Cx::in_memory`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use roundhouse::analyze::Analyzer;
use roundhouse::emit::fractal::{self, Pass, stages::Stages};
use roundhouse::span::SourceFile;

const ARTICLE_RB: &str = "app/models/article.rb";

const ARTICLE: &str = r#"class Article < ApplicationRecord
  enum :status, { draft: 0, published: 1 }

  def listed?
    published? && !draft?
  end
end
"#;

const BASE: &str = "class ApplicationRecord < ActiveRecord::Base\n  primary_abstract_class\nend\n";

fn lock(version: &str) -> String {
    format!("GEM\n  remote: https://rubygems.org/\n  specs:\n    activerecord ({version})\n\nPLATFORMS\n  ruby\n\nDEPENDENCIES\n  activerecord\n\nBUNDLED WITH\n   2.6.9\n")
}

/// Run the registered passes over `files` of the in-memory app `src`,
/// as the wasm's `fractal_stages` export does.
fn stages(src: &[(&str, &str)], files: &[&str], untrusted: &[&str]) -> Stages {
    let sources: Vec<SourceFile> = src.iter().map(|(p, t)| SourceFile { path: p.to_string(), text: t.to_string() }).collect();
    let lockfile = src.iter().find(|(p, _)| *p == "Gemfile.lock").map(|(_, t)| t.to_string());
    let tree: HashMap<PathBuf, Vec<u8>> = src.iter().map(|(p, t)| (PathBuf::from(p), t.as_bytes().to_vec())).collect();
    let mut app = roundhouse::ingest::ingest_app_from_tree(tree).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let passes = fractal::pass::registered();
    let passes: Vec<&dyn Pass> = passes.iter().map(|p| p.as_ref()).collect();
    let cx = fractal::Cx::in_memory(&app, Path::new(""), sources, lockfile.as_deref());
    let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
    let untrusted: Vec<String> = untrusted.iter().map(|f| f.to_string()).collect();
    fractal::stages::stages(&app, &cx, &files, &untrusted, &passes).expect("stages")
}

fn content<'a>(s: &'a Stages, stage: usize, path: &str) -> &'a str {
    &s.stages[stage].files.iter().find(|f| f.path == path).expect("file in stage").content
}

#[test]
fn stage_zero_is_the_source_and_each_pass_adds_its_rewrites() {
    let lock = lock("8.1.4");
    let s = stages(&[(ARTICLE_RB, ARTICLE), ("app/models/application_record.rb", BASE), ("Gemfile.lock", &lock)], &[ARTICLE_RB], &[]);
    assert_eq!(s.stages[0].pass, "source");
    assert_eq!(content(&s, 0, ARTICLE_RB), ARTICLE);

    let enum_stage = s.stages.iter().position(|st| st.pass == "inline-enum-predicates").expect("pass stage");
    let after = content(&s, enum_stage, ARTICLE_RB);
    assert!(after.contains("(status_for_database == 1) && !(status_for_database == 0)"), "{after}");
    assert_eq!(after.lines().count(), ARTICLE.lines().count());
    let rewrites = &s.stages[enum_stage].rewrites;
    assert_eq!(rewrites.iter().map(|r| r.from.as_str()).collect::<Vec<_>>(), ["published?", "draft?"]);
    assert!(rewrites.iter().all(|r| r.pass == "inline-enum-predicates"));
    assert!(s.skipped.is_empty());
}

#[test]
fn the_lockfile_comes_from_memory() {
    let lock = lock("7.0.8");
    let s = stages(&[(ARTICLE_RB, ARTICLE), ("app/models/application_record.rb", BASE), ("Gemfile.lock", &lock)], &[ARTICLE_RB], &[]);
    let last = s.stages.last().unwrap();
    assert!(last.rewrites.is_empty());
    assert_eq!(content(&s, s.stages.len() - 1, ARTICLE_RB), ARTICLE);
    assert!(last.declined.iter().any(|d| d.reason.contains("7.0.8")), "{:?}", last.declined);
}

#[test]
fn app_definitions_count_only_in_program_sources() {
    let lock = lock("8.1.4");
    let helper = "module ArticleHelpers\n  def published? = true\nend\n";
    for (path, shadows) in [
        ("app/models/article_helpers.rb", true),
        ("app/models/public/article_helpers.rb", true),
        ("app/models/test/article_helpers.rb", true),
        ("test/support/article_helpers.rb", false),
        ("node_modules/article_helpers.rb", false),
        ("app/node_modules/article_helpers.rb", false),
        ("app/.cache/article_helpers.rb", false),
    ] {
        let s = stages(
            &[(ARTICLE_RB, ARTICLE), ("app/models/application_record.rb", BASE), (path, helper), ("Gemfile.lock", &lock)],
            &[ARTICLE_RB],
            &[],
        );
        let last = s.stages.last().unwrap();
        assert_eq!(last.rewrites.iter().any(|r| r.from == "published?"), !shadows, "{path}: {:?}", last.declined);
        if shadows {
            assert!(last.declined.iter().any(|d| d.reason.contains("defines `published?` itself")), "{path}: {:?}", last.declined);
        }
    }
}

#[test]
fn an_untrusted_file_is_skipped_and_stays_the_source() {
    let lock = lock("8.1.4");
    let s = stages(&[(ARTICLE_RB, ARTICLE), ("app/models/application_record.rb", BASE), ("Gemfile.lock", &lock)], &[ARTICLE_RB], &[ARTICLE_RB]);
    assert_eq!(s.skipped.len(), 1);
    assert_eq!(s.skipped[0].file, ARTICLE_RB);
    for (i, stage) in s.stages.iter().enumerate() {
        assert_eq!(content(&s, i, ARTICLE_RB), ARTICLE);
        assert!(stage.rewrites.is_empty() && stage.declined.is_empty());
    }
}

#[test]
fn a_file_ingest_did_not_read_is_an_error() {
    let lock = lock("8.1.4");
    let sources: Vec<SourceFile> = vec![SourceFile { path: ARTICLE_RB.into(), text: ARTICLE.into() }];
    let tree: HashMap<PathBuf, Vec<u8>> = [(PathBuf::from(ARTICLE_RB), ARTICLE.as_bytes().to_vec())].into_iter().collect();
    let mut app = roundhouse::ingest::ingest_app_from_tree(tree).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let passes = fractal::pass::registered();
    let passes: Vec<&dyn Pass> = passes.iter().map(|p| p.as_ref()).collect();
    let cx = fractal::Cx::in_memory(&app, Path::new(""), sources, Some(&lock));
    let err = fractal::stages::stages(&app, &cx, &["app/models/missing.rb".to_string()], &[], &passes).err().expect("error");
    assert!(err.contains("app/models/missing.rb"), "{err}");
}
