//! The output: an overlay of the files a build generates, to be placed on
//! the app's root. A fractal build never edits an app file, so nothing
//! else belongs in it, and an app the size of a monolith is not copied.

use std::fs;
use std::path::{Path, PathBuf};

use super::variant::INITIALIZER;

/// Every path a fractal build can write, relative to the app root. A
/// rebuild replaces these and nothing else.
pub const OWNED: &[&str] = &[INITIALIZER];

/// Write `generated` under `out`. `out` may be absent, empty, or hold
/// only files an earlier build wrote, which are removed first: a rebuild
/// that no longer generates a file (a class deselected) must not leave
/// the old one behind. Anything else in `out` is refused, so `-o` can
/// never point at the app itself.
pub fn write(out: &Path, generated: &[(String, String)]) -> Result<usize, String> {
    if let Some((path, _)) = generated.iter().find(|(path, _)| !OWNED.contains(&path.as_str())) {
        return Err(format!("internal: {path} is not a path a fractal build owns"));
    }
    if out.exists() {
        let mut found = Vec::new();
        walk(out, out, &mut found, false)?;
        let foreign: Vec<&PathBuf> = found.iter().filter(|rel| !OWNED.iter().any(|owned| Path::new(owned) == *rel)).collect();
        if let Some(first) = foreign.first() {
            return Err(format!(
                "output {} holds {} file(s) a fractal build did not write ({}, ...); use an empty directory or an earlier fractal output",
                out.display(),
                foreign.len(),
                first.display()
            ));
        }
        for rel in &found {
            let path = out.join(rel);
            fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
            remove_empty_parents(out, &path);
        }
    }
    fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    for (rel, content) in generated {
        let dest = out.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    Ok(generated.len())
}

/// Copy only the overlay's files onto an existing app. Files absent from
/// the overlay stay untouched; an empty overlay changes nothing.
pub fn apply(overlay: &Path, app: &Path) -> Result<usize, String> {
    for dir in [overlay, app] {
        let meta = fs::symlink_metadata(dir).map_err(|e| format!("stat {}: {e}", dir.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} must be a directory, not a file or symlink", dir.display()));
        }
    }
    let overlay = overlay.canonicalize().map_err(|e| format!("resolve {}: {e}", overlay.display()))?;
    let app = app.canonicalize().map_err(|e| format!("resolve {}: {e}", app.display()))?;
    if app.starts_with(&overlay) {
        return Err("the app must not be the overlay directory or inside it".into());
    }
    let mut files = Vec::new();
    walk(&overlay, &overlay, &mut files, true)?;
    // Check every path before copying, so a bad entry cannot leave a
    // predictably partial apply. I/O errors can still interrupt copying.
    for rel in &files {
        let dest = app.join(rel);
        if dest.starts_with(&overlay) {
            return Err(format!("destination {} is inside the overlay", dest.display()));
        }
        let mut path = app.clone();
        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    let expected = if components.peek().is_some() { meta.is_dir() } else { meta.is_file() };
                    if !expected {
                        return Err(format!("destination {} has the wrong type or is a symlink", path.display()));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(format!("stat {}: {e}", path.display())),
            }
        }
    }
    for rel in &files {
        let dest = app.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        fs::copy(overlay.join(rel), &dest).map_err(|e| format!("copy {}: {e}", dest.display()))?;
    }
    Ok(files.len())
}

/// Every file and symlink under `dir`, relative to `root`. Applying an
/// overlay permits only regular files, never links or special files.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>, regular_only: bool) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let path: PathBuf = entry.map_err(|e| format!("read {}: {e}", dir.display()))?.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(root, &path, out, regular_only)?;
        } else {
            if regular_only && !meta.is_file() {
                return Err(format!("overlay {} is not a regular file", path.display()));
            }
            let rel = path.strip_prefix(root).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push(rel.to_path_buf());
        }
    }
    Ok(())
}

/// Remove the directories a removed file leaves empty, up to `root`.
fn remove_empty_parents(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}
