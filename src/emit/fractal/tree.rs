//! The output: the compiled app. A complete copy of the app's tree with
//! the rewritten files in place of their originals, to be tested and
//! shipped as the program. Nothing in it installs anything at boot: the
//! rewritten code is the only code there is.

use std::fs;
use std::path::{Path, PathBuf};

/// Written at the root of every compiled tree: which files the build
/// rewrote and with which passes. It also marks a directory as a fractal
/// build's own, so a rebuild may replace it.
pub const MANIFEST: &str = ".roundhouse-fractal.json";

/// Directories of the app not copied into the compiled tree: version
/// control, and per-machine state the app regenerates (caches keyed by
/// path, such as Bootsnap's, would be stale in a new location anyway).
const NOT_COPIED: &[&str] = &[".git", "tmp", "log", "node_modules"];

/// What [`write`] did.
pub struct Written {
    pub copied: usize,
    pub rewritten: usize,
}

/// Write the compiled tree of the app at `input` to `out`: every file of
/// the app (except [`NOT_COPIED`] at the root) with `rewritten` (relative
/// path, new text) in place of the originals, and the [`MANIFEST`].
///
/// `out` may be absent, empty, or an earlier fractal build (it has a
/// manifest), which is replaced. Anything else is refused, as is an `out`
/// that is the app or contains it. An `out` inside the app is left out
/// of the copy.
pub fn write(input: &Path, out: &Path, rewritten: &[(String, String)], manifest: &str) -> Result<Written, String> {
    let input = input.canonicalize().map_err(|e| format!("input {}: {e}", input.display()))?;
    let out_abs = absolute(out)?;
    if input.starts_with(&out_abs) {
        return Err(format!("output {} is the app or contains it", out.display()));
    }
    if out_abs.exists() {
        let empty = fs::read_dir(&out_abs).map_err(|e| format!("read {}: {e}", out.display()))?.next().is_none();
        if !empty && !out_abs.join(MANIFEST).is_file() {
            return Err(format!(
                "output {} is not empty and is not an earlier fractal build (no {MANIFEST}); use an empty directory",
                out.display()
            ));
        }
        fs::remove_dir_all(&out_abs).map_err(|e| format!("remove {}: {e}", out.display()))?;
    }
    fs::create_dir_all(&out_abs).map_err(|e| format!("create {}: {e}", out.display()))?;
    let out_abs = out_abs.canonicalize().map_err(|e| format!("output {}: {e}", out.display()))?;
    let mut copied = 0;
    copy_dir(&input, &input, &out_abs, &mut copied)?;
    for (rel, text) in rewritten {
        let dest = out_abs.join(rel);
        if !dest.starts_with(&out_abs) || !dest.is_file() {
            return Err(format!("internal: rewritten file {rel} is not a file of the app"));
        }
        fs::write(&dest, text).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    fs::write(out_abs.join(MANIFEST), manifest).map_err(|e| format!("write {MANIFEST}: {e}"))?;
    Ok(Written { copied, rewritten: rewritten.len() })
}

fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return path.canonicalize().map_err(|e| format!("{}: {e}", path.display()));
    }
    // Canonicalize the deepest existing ancestor and append the rest.
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if current.as_os_str().is_empty() {
            current = std::env::current_dir().map_err(|e| e.to_string())?;
            break;
        }
        if current.exists() {
            current = current.canonicalize().map_err(|e| format!("{}: {e}", current.display()))?;
            break;
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return Err(format!("output {}: cannot resolve", path.display())),
        }
    }
    for name in missing.into_iter().rev() {
        current.push(name);
    }
    Ok(current)
}

/// Copy `dir` (under `root`) into `dest`, keeping symlinks as symlinks
/// and skipping [`NOT_COPIED`] at the root and `dest` itself.
fn copy_dir(root: &Path, dir: &Path, dest: &Path, copied: &mut usize) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let path = entry.path();
        // An output inside the app is not part of the app.
        if path == dest {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|e| format!("{}: {e}", path.display()))?;
        if dir == root && rel.to_str().is_some_and(|name| NOT_COPIED.contains(&name)) {
            continue;
        }
        let target = dest.join(rel);
        let kind = entry.file_type().map_err(|e| format!("stat {}: {e}", path.display()))?;
        if kind.is_symlink() {
            let link = fs::read_link(&path).map_err(|e| format!("readlink {}: {e}", path.display()))?;
            symlink(&link, &target)?;
            *copied += 1;
        } else if kind.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
            copy_dir(root, &path, dest, copied)?;
            // A directory that only held the output is not part of the app.
            if dest.starts_with(&path) {
                let _ = fs::remove_dir(&target);
            }
        } else if kind.is_file() {
            fs::copy(&path, &target).map_err(|e| format!("copy {}: {e}", path.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(link: &Path, target: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(link, target).map_err(|e| format!("symlink {}: {e}", target.display()))
}

#[cfg(not(unix))]
fn symlink(link: &Path, target: &Path) -> Result<(), String> {
    let _ = (link, target);
    Err(format!("symlink {}: unsupported on this platform", target.display()))
}
