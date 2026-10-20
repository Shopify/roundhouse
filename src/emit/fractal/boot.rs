//! Boot regions: the work Rails does for a selected class while the app
//! boots in one environment, decided at build time and written out as
//! plain Ruby.
//!
//! A method region (`region`) is a `def` the app wrote. A boot region is
//! what Rails generates for a class at boot. Today that is an Active
//! Record model's attribute methods: Active Support's CodeGenerator builds
//! their source as strings and compiles it on every boot. Which methods a
//! model gets depends on its schema and on every gem and initializer that
//! adds attribute method patterns, so the build does not predict them: it
//! boots the app once in the chosen environment (`boot_snapshot.rb`) and
//! records the methods CodeGenerator compiled for each selected model,
//! with their source. The overlay then holds each body once
//! (`config/fractal/boot/attribute_methods.rb`, one module per Active
//! Support method cache, as CodeGenerator keeps them) and, per model, the
//! names it binds to those bodies (`config/fractal/boot/classes/`). The
//! initializer binds a model's region when Zeitwerk loads it, so the boot
//! compiles none of them.
//!
//! Nothing is checked when a region is bound. The overlay is verified at
//! build time against the app it was built from, and belongs to that
//! build of the app; a region applies only in the environment and Active
//! Record version it was built for (one comparison per boot). A model
//! whose generated methods the snapshot cannot account for in full is
//! declined with the reason and left to Rails.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const RUNTIME: &str = include_str!("boot.rb");
pub const SNAPSHOT: &str = include_str!("boot_snapshot.rb");
pub const INITIALIZER: &str = "config/initializers/fractal_boot.rb";
/// Every region file lives under this directory.
pub const DIR: &str = "config/fractal/boot/";
const BODIES: &str = "config/fractal/boot/attribute_methods.rb";

/// What one boot of the app recorded, as `boot_snapshot.rb` writes it.
#[derive(Debug, Deserialize)]
pub struct Snapshot {
    pub env: String,
    pub active_record: Option<String>,
    pub ruby: String,
    pub classes: BTreeMap<String, SnapshotClass>,
    /// namespace => canonical method name => source CodeGenerator compiled.
    pub bodies: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SnapshotClass {
    pub declined: Option<String>,
    pub file: Option<String>,
    /// (name, namespace, canonical name), sorted by name.
    #[serde(default)]
    pub methods: Vec<(String, String, String)>,
    /// (new name, old name) aliased while Rails defined the attribute
    /// methods, before it marked the aliases generated.
    #[serde(default)]
    pub aliases: Vec<(String, String)>,
    #[serde(default)]
    pub schema_loaded: bool,
    #[serde(default)]
    pub flags: Vec<String>,
}

/// The boot half of a build's report.
#[derive(Debug, Default, Serialize)]
pub struct BootReport {
    pub env: String,
    pub active_record: String,
    pub regions: Vec<BootRegion>,
    pub declined: Vec<BootDecline>,
}

#[derive(Debug, Serialize)]
pub struct BootRegion {
    pub class: String,
    pub file: String,
    pub methods: usize,
}

#[derive(Debug, Serialize)]
pub struct BootDecline {
    pub class: String,
    pub reason: String,
}

/// Boot the app at `root` in `env` under `boot_snapshot.rb` and read what
/// it recorded for `classes`. Runs `ruby` from PATH in the app root with
/// `RAILS_ENV` set, inheriting the rest of the environment, so whatever
/// the app needs to boot there (its bundle, credentials) has to be
/// available to the build.
pub fn snapshot(root: &Path, env: &str, classes: &[String]) -> Result<Snapshot, String> {
    let scratch = std::env::temp_dir().join(format!("roundhouse-fractal-boot-{}-{}", std::process::id(), unique()));
    std::fs::create_dir_all(&scratch).map_err(|e| format!("create {}: {e}", scratch.display()))?;
    let script = scratch.join("boot_snapshot.rb");
    let out = scratch.join("snapshot.json");
    std::fs::write(&script, SNAPSHOT).map_err(|e| format!("write {}: {e}", script.display()))?;
    let output = Command::new("ruby")
        .arg(&script)
        .arg(&out)
        .current_dir(root)
        .env("RAILS_ENV", env)
        .env("FRACTAL_BOOT_CLASSES", classes.join(","))
        .output()
        .map_err(|e| format!("--boot: running ruby: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(format!("--boot {env}: booting the app failed:\n{}", tail.join("\n")));
    }
    let json = std::fs::read_to_string(&out).map_err(|e| format!("read {}: {e}", out.display()))?;
    let _ = std::fs::remove_dir_all(&scratch);
    serde_json::from_str(&json).map_err(|e| format!("--boot {env}: reading the snapshot: {e}"))
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The overlay files and the report for `snapshot`. `classes` is the
/// selection, in the order the report lists it.
pub fn render(snapshot: &Snapshot, classes: &[String]) -> Result<(BootReport, Vec<(String, String)>), String> {
    let active_record = snapshot.active_record.clone().unwrap_or_default();
    let mut report = BootReport { env: snapshot.env.clone(), active_record: active_record.clone(), ..BootReport::default() };
    let namespaces: BTreeMap<&str, usize> = snapshot.bodies.keys().enumerate().map(|(i, ns)| (ns.as_str(), i)).collect();
    let mut generated = Vec::new();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for name in classes {
        let Some(class) = snapshot.classes.get(name) else {
            report.declined.push(BootDecline { class: name.clone(), reason: "the boot recorded nothing for it".into() });
            continue;
        };
        if let Some(reason) = &class.declined {
            report.declined.push(BootDecline { class: name.clone(), reason: reason.clone() });
            continue;
        }
        let missing = class.methods.iter().find(|(_, ns, canonical)| snapshot.bodies.get(ns).and_then(|b| b.get(canonical)).is_none());
        if let Some((method, _, _)) = missing {
            report.declined.push(BootDecline { class: name.clone(), reason: format!("no body was recorded for {method}") });
            continue;
        }
        let rel = format!("{}.rb", underscore(name));
        if let Some(other) = files.insert(rel.clone(), name.clone()) {
            return Err(format!("--boot: {other} and {name} would both be written to {DIR}classes/{rel}"));
        }
        generated.push((format!("{DIR}classes/{rel}"), class_file(name, class, &namespaces, &snapshot.env)));
        report.regions.push(BootRegion {
            class: name.clone(),
            file: class.file.clone().unwrap_or_default(),
            methods: class.methods.len(),
        });
    }
    if report.regions.is_empty() {
        return Ok((report, Vec::new()));
    }
    generated.push((BODIES.to_string(), bodies_file(snapshot, &report)));
    generated.push((INITIALIZER.to_string(), initializer(&report, &files)));
    generated.sort();
    Ok((report, generated))
}

fn initializer(report: &BootReport, files: &BTreeMap<String, String>) -> String {
    let mut out = RUNTIME.to_string();
    out.push('\n');
    out.push_str(&format!(
        "Fractal::Boot.configure(\n  env: {}, active_record: {},\n  regions: {{\n",
        ruby_str(&report.env),
        ruby_str(&report.active_record)
    ));
    let by_class: BTreeMap<&String, &String> = files.iter().map(|(rel, class)| (class, rel)).collect();
    for (class, rel) in by_class {
        out.push_str(&format!("    {} => {},\n", ruby_str(class), ruby_str(rel)));
    }
    out.push_str("  }.freeze\n)\nFractal::Boot.attach(Rails.application)\n");
    out
}

/// Each body once per namespace, as CodeGenerator keeps it: one module
/// per Active Support method cache, compiled with the frozen string
/// literals CodeGenerator compiles with.
fn bodies_file(snapshot: &Snapshot, report: &BootReport) -> String {
    let mut out = format!(
        "# frozen_string_literal: true\n\n# Fractal boot regions ({}): the attribute method bodies Active Support's\n# CodeGenerator compiled during a boot of this app, one module per method\n# cache. Generated by `roundhouse --target fractal --boot`; do not edit.\n\nmodule Fractal\n  module Boot\n    module Bodies\n",
        report.env
    );
    for (i, (namespace, bodies)) in snapshot.bodies.iter().enumerate() {
        out.push_str(&format!("      # {namespace}\n      N{i} = Module.new\n      N{i}.module_eval do\n"));
        for source in bodies.values() {
            for line in source.lines() {
                out.push_str("        ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str("      end\n\n");
    }
    out.push_str("    end\n  end\nend\n");
    out
}

fn class_file(name: &str, class: &SnapshotClass, namespaces: &BTreeMap<&str, usize>, env: &str) -> String {
    let mut out = format!(
        "# frozen_string_literal: true\n\n# Fractal boot region ({env}): the attribute methods of {name}{}. Generated by\n# `roundhouse --target fractal --boot`; do not edit.\n\nowner = {name}.send(:generated_attribute_methods)\n",
        class.file.as_ref().map(|f| format!(" ({f})")).unwrap_or_default()
    );
    let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (method, namespace, canonical) in &class.methods {
        let pairs = groups.entry(namespaces[namespace.as_str()]).or_default();
        pairs.push(ruby_sym(method));
        pairs.push(ruby_sym(canonical));
    }
    for (i, pairs) in groups {
        out.push_str(&format!("Fractal::Boot.bind(owner, Fractal::Boot::Bodies::N{i}, [{}])\n", pairs.join(", ")));
    }
    if class.schema_loaded {
        out.push_str(&format!("{name}.load_schema\n"));
    }
    // Registered the way Rails registers them at this point: before the
    // aliases are marked generated, so Active Record defines no method
    // for them (their methods are bound above).
    for (new_name, old_name) in &class.aliases {
        out.push_str(&format!("{name}.alias_attribute({}, {})\n", ruby_str(new_name), ruby_str(old_name)));
    }
    for flag in &class.flags {
        out.push_str(&format!("{name}.instance_variable_set({}, true)\n", ruby_sym(flag)));
    }
    out
}

/// `Admin::ReportCard` => `admin/report_card`, as Active Support's
/// `underscore` spells it.
pub fn underscore(name: &str) -> String {
    let chars: Vec<char> = name.trim_start_matches("::").replace("::", "/").chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                Some(p) if p.is_ascii_lowercase() || p.is_ascii_digit() => true,
                Some(p) if p.is_ascii_uppercase() => next.is_some_and(|n| n.is_ascii_lowercase()),
                _ => false,
            };
            if boundary {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn ruby_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn ruby_sym(s: &str) -> String {
    let plain = s.strip_prefix('@').unwrap_or(s);
    let simple = !plain.is_empty()
        && plain.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && plain.char_indices().all(|(i, c)| {
            c.is_ascii_alphanumeric() || c == '_' || (i == plain.len() - 1 && matches!(c, '?' | '!' | '='))
        })
        && !(s.starts_with('@') && plain.ends_with(['?', '!', '=']));
    if simple { format!(":{s}") } else { format!(":\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underscore_spells_paths_as_active_support_does() {
        assert_eq!(underscore("Article"), "article");
        assert_eq!(underscore("Admin::ReportCard"), "admin/report_card");
        assert_eq!(underscore("HTMLPage"), "html_page");
        assert_eq!(underscore("Widget12"), "widget12");
    }

    #[test]
    fn symbols_are_quoted_only_when_they_must_be() {
        assert_eq!(ruby_sym("title"), ":title");
        assert_eq!(ruby_sym("title="), ":title=");
        assert_eq!(ruby_sym("saved_change_to_title?"), ":saved_change_to_title?");
        assert_eq!(ruby_sym("@attribute_methods_generated"), ":@attribute_methods_generated");
        assert_eq!(ruby_sym("first name"), ":\"first name\"");
        assert_eq!(ruby_sym("a=b"), ":\"a=b\"");
    }
}
