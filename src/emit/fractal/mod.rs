//! `--target fractal`: speed up one class at a time.
//!
//! Rails keeps running the app. The person running the build names the
//! classes to speed up (`--class Current`); every other file, and every
//! expression in a selected class that cannot be shown safe, stays the
//! app's own text. Deselecting a class and rebuilding puts its original
//! back, so the class is the unit a person switches on and off, while the
//! expression (one query chain) is the unit that is rewritten.
//!
//! - **Selection** comes from Rubydex: every `class`/`module` body whose
//!   resolved name is the selected one, reopenings included, minus the
//!   bodies of other classes nested inside it. A method or action is
//!   selected when its name is written inside one of those bodies; a
//!   method a concern contributes lives in the concern's file and is not.
//! - **Rewriting** is [`query`]: static Active Record chains become
//!   statements Rails builds once, each spliced in at the source span of
//!   the expression it replaces, with the original kept as the fallback.
//! - **Runtime** is one generated file, `config/initializers/fractal.rb`:
//!   the `Fractal` module (`fractal.rb`) and one `Fractal.define` per
//!   rewritten site. It counts how often each site took the fast path.
//! - **No class selected** is the identity: the app as source control
//!   holds it, the control a fractal build is compared against.
//!
//! This target shares no code with `--target futamura`, deliberately. Its
//! rewriter started as a copy of futamura's query specialization and is
//! expected to diverge; neither target's output has to stay compatible
//! with the other's.

pub mod query;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::App;
use crate::dialect::{ControllerBodyItem, ModelBodyItem};
use crate::span::Span;

use query::Specializer;

const RUNTIME: &str = include_str!("fractal.rb");
pub const INITIALIZER: &str = "config/initializers/fractal.rb";

/// What a fractal emit did, for the CLI's report.
#[derive(Default)]
pub struct Report {
    /// `Class file:line` for every definition selected.
    pub definitions: Vec<String>,
    pub specialized: Vec<String>,
    pub residue: Vec<String>,
}

/// The source ranges `classes` select, and the nested ranges inside them
/// that belong to other classes.
pub struct Selection {
    pub bodies: Vec<(String, Span)>,
    pub excluded: Vec<Span>,
}

impl Selection {
    /// Every definition of each class, by Rubydex. Errors on a class the
    /// app does not define: a misspelt `--class` must not quietly build the
    /// identity.
    pub fn resolve(app: &App, classes: &[String]) -> Result<Selection, String> {
        let resolver = app.const_resolver.for_sources(&app.sources);
        let mut bodies = Vec::new();
        for class in classes {
            let name = class.trim_start_matches("::");
            let found = resolver.class_bodies(name);
            if found.is_empty() {
                return Err(format!("--class {class}: no class or module of that name is defined in the app's sources"));
            }
            bodies.extend(found.into_iter().map(|span| (name.to_string(), span)));
        }
        let selected: Vec<&str> = bodies.iter().map(|(n, _)| n.as_str()).collect();
        let excluded = resolver
            .all_class_bodies()
            .into_iter()
            .filter(|(name, span)| {
                !selected.contains(&name.as_ref())
                    && bodies.iter().any(|(_, body)| contains(body, *span) && *body != *span)
            })
            .map(|(_, span)| span)
            .collect();
        Ok(Selection { bodies, excluded })
    }

    /// Is a member whose name is written at `at` part of the selection?
    /// The innermost class body around it decides: a selected class
    /// nested inside an unselected one inside a selected one (`--class A
    /// --class C` over `A::B::C`) is selected, and `B`'s own members are
    /// not.
    pub fn selects(&self, at: Span) -> bool {
        if at.is_synthetic() {
            return false;
        }
        let innermost = |spans: &mut dyn Iterator<Item = &Span>| spans.filter(|s| contains(s, at)).map(|s| s.len()).min();
        match (innermost(&mut self.bodies.iter().map(|(_, s)| s)), innermost(&mut self.excluded.iter())) {
            (Some(selected), Some(excluded)) => selected < excluded,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

fn contains(outer: &Span, inner: Span) -> bool {
    outer.file == inner.file && outer.start <= inner.start && inner.end <= outer.end
}

/// Rewrite the selected classes' query sites in `files` (the identity
/// tree from [`files`]) and add the initializer when any site was rewritten.
/// `untrusted` names files ingest could not read faithfully (a recorded
/// survey gap, a syntax error): their IR holds placeholders, so no site in
/// them is rewritten.
pub fn specialize(
    app: &App,
    files: &mut Vec<(String, String)>,
    classes: &[String],
    untrusted: &[String],
) -> Result<Report, String> {
    let mut report = Report::default();
    if classes.is_empty() {
        return Ok(report);
    }
    let selection = Selection::resolve(app, classes)?;
    for (name, span) in &selection.bodies {
        let source = &app.sources[span.file.0 as usize - 1];
        let (line, _) = source.line_col(span.start);
        report.definitions.push(format!("{name} {}:{line}", source.path));
    }

    let spec = Specializer::new(app);
    let mut sites = Vec::new();
    let mut residue = Vec::new();
    for c in &app.controllers {
        for item in &c.body {
            if let ControllerBodyItem::Action { action, .. } = item
                && selection.selects(action.name_span)
            {
                spec.method_body(&action.body, &mut sites, &mut residue);
            }
        }
    }
    for m in &app.models {
        for item in &m.body {
            if let ModelBodyItem::Method { method, .. } = item
                && selection.selects(method.name_span)
            {
                spec.method_body(&method.body, &mut sites, &mut residue);
            }
        }
    }
    for lc in &app.library_classes {
        for method in &lc.methods {
            if selection.selects(method.name_span) {
                spec.method_body(&method.body, &mut sites, &mut residue);
            }
        }
    }

    let location = |span: Span| {
        let source = &app.sources[span.file.0 as usize - 1];
        let (line, col) = source.line_col(span.start);
        format!("{}:{line}:{col}", source.path)
    };
    // Group by file; splice from the end so earlier offsets stay valid.
    let mut by_file: BTreeMap<u32, Vec<query::Site>> = BTreeMap::new();
    for site in sites {
        by_file.entry(site.span.file.0).or_default().push(site);
    }
    let mut defines = Vec::new();
    for (file, mut sites) in by_file {
        let source = &app.sources[file as usize - 1];
        if untrusted.iter().any(|u| is_suffix_path(&source.path, u) || is_suffix_path(u, &source.path)) {
            report.residue.extend(
                sites.iter().map(|s| format!("{}: the file has an ingest gap or a syntax error", location(s.span))),
            );
            continue;
        }
        let Some(content) = emitted_file(files, &source.path) else {
            report.residue.extend(sites.iter().map(|s| format!("{}: file not in the emitted tree", location(s.span))));
            continue;
        };
        // Spans index the text ingest parsed; splice only into the same text.
        if *content != source.text {
            report.residue.extend(sites.iter().map(|s| format!("{}: source text differs from the file", location(s.span))));
            continue;
        }
        sites.sort_by_key(|s| std::cmp::Reverse(s.span.start));
        let mut last_start = u32::MAX;
        for site in sites {
            if site.span.end > last_start {
                report.residue.push(format!("{}: overlaps another site", location(site.span)));
                continue;
            }
            let (start, end) = (site.span.start as usize, site.span.end as usize);
            let call = site.call.replace("{orig}", &content[start..end]);
            content.replace_range(start..end, &call);
            last_start = site.span.start;
            report.specialized.push(location(site.span));
            defines.push(site.define);
        }
    }
    report.residue.extend(residue.into_iter().map(|r| format!("{}: {}", location(r.span), r.reason)));

    if !defines.is_empty() {
        defines.sort();
        let mut init = RUNTIME.to_string();
        init.push('\n');
        let static_scopes = spec.static_association_scopes();
        if !static_scopes.is_empty() {
            let names = static_scopes.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", ");
            init.push_str(&format!("Fractal::Preload.static_scopes({names})\n\n"));
        }
        init.push_str("Rails.application.config.to_prepare do\n  Fractal.prepare\n");
        for d in &defines {
            init.push_str("  ");
            init.push_str(d);
            init.push('\n');
        }
        init.push_str("end\n");
        if files.iter().any(|(p, _)| p == INITIALIZER) {
            return Err(format!("{INITIALIZER} already exists in the app"));
        }
        files.push((INITIALIZER.to_string(), init));
        files.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(report)
}

/// The emitted file a source path is: the longest emitted path that is
/// its tail, so an engine's `components/x/app/models/user.rb` is not
/// matched with the root `app/models/user.rb`.
fn emitted_file<'f>(files: &'f mut [(String, String)], source_path: &str) -> Option<&'f mut String> {
    files
        .iter_mut()
        .filter(|(p, _)| is_suffix_path(source_path, p))
        .max_by_key(|f| f.0.len())
        .map(|(_, content)| content)
}

/// Is `rel` the tail of `path` at a path boundary?
fn is_suffix_path(path: &str, rel: &str) -> bool {
    path == rel || path.ends_with(&format!("/{rel}"))
}

/// The identity tree: the app as source control holds it. It keeps
/// dotfiles (`.ruby-version` decides which Ruby runs the app) and the
/// `.keep` files that hold Rails' empty directories open, and leaves out
/// run-time state, named by the paths a generated Rails app's `.gitignore`
/// excludes. Binary files travel separately, as `App::binary_assets`.
pub fn files(app_root: &Path) -> Result<Vec<(String, String)>, String> {
    const UNWALKED: [&str; 2] = [".git", "node_modules"];
    const STATE: [&str; 5] = ["log/", "tmp/", "storage/", "app/assets/builds/", "public/assets/"];
    let generated_state = |rel: &str| {
        (STATE.iter().any(|dir| rel.starts_with(dir)) && !rel.ends_with("/.keep"))
            || rel.starts_with(".bundle")
            || rel == "config/master.key"
    };
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("read entry: {e}"))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("strip prefix: {e}"))?
                .to_string_lossy()
                .replace('\\', "/");
            if entry.file_type().map_err(|e| format!("stat: {e}"))?.is_dir() {
                if !UNWALKED.contains(&rel.as_str()) {
                    walk(root, &path, out)?;
                }
            } else {
                out.push((rel, path));
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(app_root, app_root, &mut paths)?;
    let mut files: Vec<(String, String)> = paths
        .into_iter()
        .filter(|(rel, _)| !generated_state(rel))
        .filter_map(|(rel, path)| fs::read_to_string(&path).ok().map(|content| (rel, content)))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_source_is_matched_with_its_own_file_not_the_roots() {
        let mut files = vec![
            ("app/models/user.rb".to_string(), "root".to_string()),
            ("components/x/app/models/user.rb".to_string(), "engine".to_string()),
        ];
        assert_eq!(emitted_file(&mut files, "myapp/components/x/app/models/user.rb").unwrap(), "engine");
        assert_eq!(emitted_file(&mut files, "myapp/app/models/user.rb").unwrap(), "root");
        assert!(emitted_file(&mut files, "myapp/app/models/account.rb").is_none());
    }
}
