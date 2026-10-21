//! `--target fractal`: compile a Rails app by rewriting its own source,
//! one class at a time.
//!
//! A person names the classes to work on (`--class Order`). Roundhouse
//! ingests and analyzes those classes and what its passes say they need
//! ([`scope`]), so it knows what Rails cannot see from one call site
//! (types, effects, which definitions exist), and passes use that
//! application knowledge to rewrite the selected classes' source in
//! place. The output is the compiled app: a complete copy of the app's
//! tree with the rewritten files in place of the originals ([`tree`]).
//! It is what gets tested and shipped. Nothing in it installs code at
//! boot, checks assumptions at load, or keeps the original code around:
//! the rewritten code is the only code there is, so a rewrite of a class
//! body (boot work) and of a method body (runtime work) are the same kind
//! of edit.
//!
//! The pieces, in the order a build runs them:
//!
//! - [`scope`]: which files to ingest, decided from a Rubydex index of
//!   the app's constants before anything is ingested.
//! - [`select`]: Rubydex finds every body of each selected class,
//!   reopenings included, minus the bodies of other classes nested in it.
//! - [`region`]: each `def` in a selected body, with the analyzer's
//!   facts about it.
//! - [`pass`]: the rewrites. Each reads one file of the selection (a
//!   [`Unit`]) and proposes edits inside its selected bodies.
//! - [`rewrite`]: applies the proposals, refusing any that leave the
//!   selection, overlap, move a line, or break the parse.
//! - [`tree`]: writes the compiled tree.
//!
//! Every assumption a rewrite rests on is checked at build time; there is
//! no runtime fallback. A rewrite that cannot be shown equivalent from
//! what the build knows is declined and reported. See
//! `docs/pipeline/fractal.md`.

mod definitions;
pub mod enum_predicates;
pub mod pass;
pub mod region;
pub mod rewrite;
pub mod scope;
pub mod select;
pub mod tree;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::App;
use crate::span::FileId;

pub use pass::{Cx, Edit, Pass, Proposal, Unit};
pub use region::{Analyzed, Knowledge, Region};
pub use rewrite::{Decline, Rewrite};
pub use scope::{Ingest, Reach, Scope, ScopeCx};
pub use select::Selection;
pub use tree::MANIFEST;

/// What a build decided, for the CLI's report and `--report PATH`.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub classes: Vec<String>,
    /// Every body the selection covers, as `Class path:line`.
    pub bodies: Vec<String>,
    pub passes: Vec<String>,
    pub methods: Vec<MethodReport>,
    /// Files of the selection no pass may rewrite, with why.
    pub skipped: Vec<Skipped>,
    /// Every edit applied, in file order.
    pub rewrites: Vec<Rewrite>,
    /// The files the compiled tree replaces, relative to the app root.
    pub files: Vec<String>,
    pub declined: Vec<Decline>,
}

#[derive(Debug, Serialize)]
pub struct MethodReport {
    pub key: String,
    pub knowledge: Knowledge,
}

#[derive(Debug, Serialize)]
pub struct Skipped {
    pub file: String,
    pub reason: String,
}

impl Report {
    /// The human-readable summary the CLI prints.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.classes.is_empty() {
            out.push_str("fractal: no --class selected; the compiled tree is the app unchanged\n");
            return out;
        }
        for body in &self.bodies {
            out.push_str(&format!("fractal: selected {body}\n"));
        }
        let typed = self.methods.iter().filter(|m| m.knowledge.copies > 0 && m.knowledge.typed == m.knowledge.expressions).count();
        out.push_str(&format!(
            "fractal: {} method(s) in the selection ({typed} fully typed); {} pass(es): {}\n",
            self.methods.len(),
            self.passes.len(),
            if self.passes.is_empty() { "none registered".to_string() } else { self.passes.join(", ") },
        ));
        for s in &self.skipped {
            out.push_str(&format!("  skipped       {}: {}\n", s.file, s.reason));
        }
        out.push_str(&format!("fractal: {} rewrite(s) in {} file(s)\n", self.rewrites.len(), self.files.len()));
        for r in &self.rewrites {
            out.push_str(&format!("  rewrote       {} [{}]: {} -> {}\n", r.at, r.pass, r.from, r.to));
        }
        for d in &self.declined {
            out.push_str(&format!("  declined      {} [{}]: {}\n", d.at, d.pass, d.reason));
        }
        out
    }
}

/// A build's decisions and the files it rewrites.
pub struct Plan {
    pub report: Report,
    /// (path relative to the app root, rewritten text).
    pub rewritten: Vec<(String, String)>,
}

impl Plan {
    /// The [`MANIFEST`] the compiled tree carries.
    pub fn manifest(&self) -> String {
        #[derive(Serialize)]
        struct Manifest<'a> {
            classes: &'a [String],
            passes: &'a [String],
            files: &'a [String],
        }
        let m = Manifest { classes: &self.report.classes, passes: &self.report.passes, files: &self.report.files };
        serde_json::to_string_pretty(&m).unwrap_or_default() + "\n"
    }
}

/// Decide a build: select, run `passes` over each file of the selection,
/// and apply what they propose. `input` is the app root ingest read;
/// `untrusted` names files ingest could only partly read (a `--survey`
/// gap or a syntax error), which keep the app's code.
pub fn plan(app: &App, input: &Path, classes: &[String], untrusted: &[String], passes: &[&dyn Pass]) -> Result<Plan, String> {
    let mut report = Report {
        classes: classes.to_vec(),
        passes: passes.iter().map(|p| p.name().to_string()).collect(),
        ..Report::default()
    };
    if classes.is_empty() {
        return Ok(Plan { report, rewritten: Vec::new() });
    }
    let selection = Selection::resolve(app, classes)?;
    let source = |file: FileId| (file.0 as usize).checked_sub(1).and_then(|i| app.sources.get(i));
    let root = input.canonicalize().map_err(|e| format!("input {}: {e}", input.display()))?;
    let roots = [input.to_path_buf(), root.clone()];
    let path_of = |file: FileId| -> Option<String> {
        let path = PathBuf::from(&source(file)?.path);
        let candidates = [path.clone(), path.canonicalize().unwrap_or(path)];
        candidates.iter().find_map(|p| roots.iter().find_map(|root| p.strip_prefix(root).ok())).map(|rel| rel.to_string_lossy().replace('\\', "/"))
    };
    let is_untrusted = |path: &str| untrusted.iter().any(|u| is_suffix_path(path, u) || is_suffix_path(u, path));
    for (name, span) in &selection.bodies {
        if let Some(src) = source(span.file) {
            let (line, _) = src.line_col(span.start);
            report.bodies.push(format!("{name} {}:{line}", path_of(span.file).unwrap_or_else(|| src.path.clone())));
        }
    }
    report.bodies.sort();

    let mut files: Vec<FileId> = selection.bodies.iter().map(|(_, span)| span.file).collect();
    files.sort_by_key(|f| f.0);
    files.dedup();
    let cx = Cx::new(app, &root);
    let mut rewritten = Vec::new();
    for file in files {
        let Some(src) = source(file) else { continue };
        let display = path_of(file).unwrap_or_else(|| src.path.clone());
        let methods = region::find(app, &selection, file, &display);
        report.methods.extend(methods.iter().map(|m| MethodReport { key: m.key(), knowledge: m.knowledge.clone() }));
        let skip = if path_of(file).is_none() {
            Some("the file is outside the app root".to_string())
        } else if is_untrusted(&src.path) {
            Some("ingest could not read the file faithfully (a --survey gap or a syntax error)".to_string())
        } else if std::fs::read(root.join(&display)).ok().as_deref() != Some(src.text.as_bytes()) {
            Some("the file on disk is not the UTF-8 text ingest read".to_string())
        } else {
            None
        };
        if let Some(reason) = skip {
            report.skipped.push(Skipped { file: display, reason });
            continue;
        }
        if passes.is_empty() {
            continue;
        }
        let bodies = selection.bodies.iter().filter(|(_, span)| span.file == file).cloned().collect();
        let unit = Unit { file, path: display.clone(), text: &src.text, bodies, methods, selection: &selection };
        let proposals = passes.iter().map(|p| (p.name(), p.run(&cx, &unit))).collect();
        let outcome = rewrite::apply(&unit, proposals);
        report.rewrites.extend(outcome.rewrites);
        report.declined.extend(outcome.declined);
        if let Some(text) = outcome.text {
            report.files.push(display.clone());
            rewritten.push((display, text));
        }
    }
    report.methods.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(Plan { report, rewritten })
}

/// Write the compiled tree of the app at `input` to `out`. Returns how
/// many files were copied and how many of them were rewritten.
pub fn write(input: &Path, out: &Path, plan: &Plan) -> Result<tree::Written, String> {
    tree::write(input, out, &plan.rewritten, &plan.manifest())
}

/// Is `rel` the tail of `path` at a path boundary?
fn is_suffix_path(path: &str, rel: &str) -> bool {
    path == rel || path.ends_with(&format!("/{rel}"))
}
