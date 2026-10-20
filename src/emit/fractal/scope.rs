//! What a fractal build ingests.
//!
//! The unit is the class. By default a build ingests only the files that
//! define the `--class` names, plus what each registered pass says it
//! needs to know about them ([`Pass::scope`]). Rubydex answers where
//! those files are before anything is ingested: it indexes the app's Ruby
//! for constants alone, which is far cheaper than ingesting and analyzing
//! it. A pass whose knowledge can come from anywhere in the app says so
//! with [`Scope::WholeApp`], and `--whole-app` asks for everything
//! whatever the passes need.
//!
//! Outside the scope, files read as absent. What they would have told
//! the analyzer (a column's type, a method another file defines) is then
//! unknown, and a pass that needed it declines: a narrower scope can
//! produce fewer variants, never a different one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::analyze::{DefinitionIndex, definition_index};
use crate::span::SourceFile;

use super::pass::Pass;

/// What a pass needs ingested.
pub enum Scope {
    /// Application knowledge from anywhere: ingest and analyze the whole
    /// app, as a build with `--whole-app` does.
    WholeApp,
    /// The files defining the selected classes, plus what each [`Reach`]
    /// adds.
    Classes(Vec<Reach>),
}

/// One kind of file a [`Scope::Classes`] pass also needs.
#[derive(Clone, Copy)]
pub enum Reach {
    /// The files defining each selected class's ancestors that the app
    /// defines itself: superclasses and included, prepended and extended
    /// modules, transitively. What a method inherits, and what an
    /// inherited `attr_reader`'s signature says, comes from these.
    Ancestors,
    /// Every `sig/**/*.rbs` sidecar.
    Signatures,
    /// The schema (`db/schema.rb`, else `db/structure.sql`), for column
    /// types.
    Schema,
    /// Anything else: the function adds paths (absolute, under
    /// [`ScopeCx::root`]) from the Rubydex index.
    Custom(fn(&ScopeCx<'_>, &mut BTreeSet<PathBuf>)),
}

/// What a [`Reach::Custom`] function can read.
pub struct ScopeCx<'a> {
    /// The app root, canonical.
    pub root: &'a Path,
    pub classes: &'a [String],
    pub index: &'a DefinitionIndex,
}

/// The build's decision.
pub enum Ingest {
    /// Ingest the whole app, and why.
    WholeApp { reason: String },
    /// Ingest these files (absolute) and nothing else. `indexed` is how
    /// many Ruby files Rubydex read to decide.
    Files { files: BTreeSet<PathBuf>, indexed: usize },
}

/// Files ingest reads to learn the app's shape (its gems, inflections,
/// packages and autoload settings), kept in every scope.
const APP_SHAPE: &[&str] =
    &["Gemfile.lock", "roundhouse.yml", "packwerk.yml", "packs.yml", "config/application.rb", "config/initializers/inflections.rb"];

/// Directories the Rubydex index skips at any depth: no app class is
/// defined in them, and in a large app they are most of the files.
const UNINDEXED: &[&str] = &["node_modules", "vendor", "tmp", "log", "coverage", "public", "test", "spec", "db", "sorbet", "sig"];

/// Decide what a build with `classes` and `passes` ingests from `root`
/// (canonical). `whole_app` is `--whole-app`.
pub fn decide(root: &Path, classes: &[String], passes: &[&dyn Pass], whole_app: bool) -> Result<Ingest, String> {
    if whole_app {
        return Ok(Ingest::WholeApp { reason: "--whole-app".to_string() });
    }
    let mut reaches = Vec::new();
    for pass in passes {
        match pass.scope() {
            Scope::WholeApp => return Ok(Ingest::WholeApp { reason: format!("pass `{}` needs the whole app", pass.name()) }),
            Scope::Classes(r) => reaches.extend(r),
        }
    }
    let mut files = BTreeSet::new();
    let mut indexed = 0;
    if !classes.is_empty() {
        let sources = ruby_sources(root);
        indexed = sources.len();
        let index = definition_index(&sources);
        drop(sources);
        let names: Vec<&str> = classes.iter().map(|c| c.trim_start_matches("::")).collect();
        for (class, name) in classes.iter().zip(&names) {
            let defined = index.files.get(*name).filter(|paths| !paths.is_empty());
            let Some(paths) = defined else {
                return Err(format!("--class {class}: no class or module of that name is defined in the app's sources"));
            };
            files.extend(paths.iter().map(PathBuf::from));
        }
        let cx = ScopeCx { root, classes, index: &index };
        for reach in reaches {
            match reach {
                Reach::Ancestors => {
                    for name in &names {
                        for ancestor in index.ancestors.get(*name).into_iter().flatten() {
                            files.extend(index.files.get(ancestor).into_iter().flatten().map(PathBuf::from));
                        }
                    }
                }
                Reach::Signatures => walk(&root.join("sig"), &|_| false, &mut |path| {
                    if path.extension().is_some_and(|e| e == "rbs") {
                        files.insert(path.to_path_buf());
                    }
                }),
                Reach::Schema => {
                    let schema = ["db/schema.rb", "db/structure.sql"].iter().map(|p| root.join(p)).find(|p| p.is_file());
                    files.extend(schema);
                }
                Reach::Custom(widen) => widen(&cx, &mut files),
            }
        }
    }
    files.extend(layout_files(root, &files));
    files.extend(APP_SHAPE.iter().map(|p| root.join(p)).filter(|p| p.is_file()));
    Ok(Ingest::Files { files, indexed })
}

/// What tells ingest that a directory holding a kept file is an app root:
/// a Packwerk `package.yml`, or the `Rails::Engine` subclass in the `lib/`
/// of an in-repository gem (a component with its own `app/`).
fn layout_files(root: &Path, files: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();
    for file in files {
        dirs.extend(file.ancestors().skip(1).take_while(|dir| dir.starts_with(root) && *dir != root).map(Path::to_path_buf));
    }
    let mut out = BTreeSet::new();
    for dir in dirs {
        let package = dir.join("package.yml");
        if package.is_file() {
            out.insert(package);
        }
        let is_gem = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "gemspec"));
        if is_gem && dir.join("app").is_dir() {
            walk(&dir.join("lib"), &|_| false, &mut |path| {
                let declares = path.extension().is_some_and(|e| e == "rb")
                    && std::fs::read_to_string(path).is_ok_and(|text| text.contains("Rails::Engine"));
                if declares {
                    out.insert(path.to_path_buf());
                }
            });
        }
    }
    out
}

/// Every `.rb` under `root` outside [`UNINDEXED`] and hidden directories,
/// with absolute paths, for the Rubydex index.
fn ruby_sources(root: &Path) -> Vec<SourceFile> {
    let mut paths = Vec::new();
    walk(root, &|name| UNINDEXED.contains(&name), &mut |p| {
        if p.extension().is_some_and(|e| e == "rb") {
            paths.push(p.to_path_buf());
        }
    });
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            Some(SourceFile { path: path.to_string_lossy().into_owned(), text: String::from_utf8_lossy(&bytes).into_owned() })
        })
        .collect()
}

/// Visit every file under `dir`, not following symlinked directories and
/// skipping hidden directories and the ones `skip` names.
fn walk(dir: &Path, skip: &dyn Fn(&str) -> bool, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.') && !skip(&name) {
                walk(&path, skip, visit);
            }
        } else if kind.is_file() {
            visit(&path);
        }
    }
}
