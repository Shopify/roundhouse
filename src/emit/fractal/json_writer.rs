//! `json-writer`: write a fixed-shape hash's JSON directly, where a
//! method builds the hash only to encode it with Active Support's
//! `to_json`.
//!
//! ```ruby
//! def to_wire
//!   { label: label, count: count }.to_json
//! end
//! ```
//!
//! Rails' `Hash#to_json` hands the hash to Active Support's encoder,
//! which walks it into a second hash (`jsonify`), has the json gem
//! generate that, then scans the result again to escape HTML entities
//! and the JavaScript line separators. When the keys are fixed and every
//! value turns out to be a JSON scalar whose bytes are known, all of that
//! produces one predictable string. The variant evaluates the values,
//! exactly as the hash literal did, and writes that string; no hash is
//! built and nothing is traversed twice.
//!
//! What it leaves alone, and why:
//!
//! - **`JSON.generate`.** The json gem writes a small hash natively, and
//!   a Ruby-level writer measured slower than it under YJIT and ZJIT.
//!   Fusing it would trade CPU for one avoided hash, so such sites are
//!   reported and kept.
//! - **Any hash a caller could see.** The hash must be the literal
//!   receiver of `to_json`, or a local assigned that literal exactly once
//!   at the top level of the method and read exactly once, by a
//!   top-level `to_json` call after it. A method that can observe its
//!   locals (`binding`, `eval`, `local_variables`, ...) is left alone,
//!   because the variant introduces temporaries. Public `as_json` and
//!   `to_json` methods are not touched: only this encoding consumer is.
//! - **Keys that are not plain literals**, duplicate keys (an overwritten
//!   value can still have effects), splats, `to_json` with options, and
//!   values the analyzer cannot place among String, Integer, true, false
//!   and nil (or one of those or nil).
//!
//! The analyzer's types only choose which shape to try. They are not
//! trusted: each call checks every value with Ruby's own core methods,
//! never with a method of the value itself, and any value that is not
//! exactly what the writer reproduces byte for byte (a String subclass,
//! a non-UTF-8 string, one that needs escaping, a Float, ...) sends the
//! call to the original expression, with the values already computed.
//! So values are evaluated once, in source order, either way, and every
//! case the writer does not cover is the app's own encoding.
//!
//! The variant relies on Active Support's encoding being the one in
//! effect, unmodified, and on the core methods its checks call being
//! Ruby's own. The [`Guard::ActiveSupportJson`] guard checks both when
//! the owner loads; otherwise the app's method stays. It also declines
//! when Active Support encodes with the json gem's `Coder` (json 2.15.2
//! and later): the original is then native from end to end, and the
//! writer measured slower than it for a six-value hash, so which shapes
//! win there needs a cost model this pass does not have yet.

use std::collections::{BTreeSet, HashMap};

use ruby_prism::{Node, Visit};

use crate::expr::Expr;
use crate::span::Span;
use crate::ty::Ty;

use super::pass::{Cx, Edit, Guard, Pass, Proposal};
use super::region::Region;

pub struct JsonWriter;

/// Calls that can read or write the method's locals, which a variant's
/// temporaries would change.
const LOCAL_OBSERVERS: [&str; 9] = [
    "binding",
    "local_variables",
    "local_variable_get",
    "local_variable_set",
    "local_variable_defined?",
    "eval",
    "instance_eval",
    "class_eval",
    "module_eval",
];

/// Prefix of the variant's temporaries. A method that already mentions
/// it is left alone.
const TEMP: &str = "__fractal_json";

/// The runtime module holding what the checks compare against.
const RT: &str = "::Fractal::JsonWriter";

impl Pass for JsonWriter {
    fn name(&self) -> &'static str {
        "json-writer"
    }

    fn run(&self, cx: &Cx<'_>, region: &Region<'_>) -> Proposal {
        let Some(source) = cx.app.sources.get(region.file.0 as usize - 1) else { return Proposal::default() };
        let text = source.text.as_str();
        let (def_start, def_end) = (region.def_span.start as usize, region.def_span.end as usize);
        let def_text = &text[def_start..def_end];
        if !def_text.contains("to_json") && !def_text.contains("JSON") {
            return Proposal::default();
        }
        let parsed = ruby_prism::parse(text.as_bytes());
        let mut finder = DefFinder { start: def_start, end: def_end, scan: None };
        finder.visit(&parsed.node());
        let Some(scan) = finder.scan else { return Proposal::default() };
        plan(region, text, scan)
    }
}

/// What one walk over the method's definition found.
#[derive(Default)]
struct Scan {
    /// Ranges of the statements directly in the method body.
    top: Vec<(usize, usize)>,
    /// Text of the parameter list, for the local-shadowing check.
    params: String,
    sites: Vec<RawSite>,
    /// Local name -> (writes, reads), counting every scope in the method.
    locals: HashMap<String, (usize, usize)>,
    /// `name = { ... }` assignments, by name.
    hash_writes: HashMap<String, (usize, usize, Result<Vec<RawPair>, String>)>,
    observer: Option<String>,
    heredoc: bool,
    /// Sites the pass recognised as JSON encoding and will not rewrite.
    declined: Vec<(usize, usize, String)>,
}

enum RawSite {
    /// `{ ... }.to_json`
    Literal { call: (usize, usize), pairs: Result<Vec<RawPair>, String> },
    /// `name.to_json`
    Local { call: (usize, usize), name: String },
}

struct RawPair {
    name: String,
    key_src: String,
    arrow: bool,
    value: RawValue,
    range: (usize, usize),
    value_src: String,
    /// A local, instance variable or `self`: reading it twice is unobservable.
    plain_read: bool,
}

enum RawValue {
    /// A literal whose JSON is fixed: its bytes are written directly.
    Baked(String),
    Expr,
}

struct DefFinder {
    start: usize,
    end: usize,
    scan: Option<Scan>,
}

impl<'pr> Visit<'pr> for DefFinder {
    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        let loc = node.location();
        if loc.start_offset() == self.start && loc.end_offset() == self.end {
            let mut scan = Scan::default();
            if let Some(params) = node.parameters() {
                scan.params = text(params.location().as_slice());
            }
            if let Some(body) = node.body() {
                let statements = body.as_statements_node().or_else(|| body.as_begin_node().and_then(|b| b.statements()));
                if let Some(statements) = statements {
                    scan.top = statements.body().iter().map(|n| (n.location().start_offset(), n.location().end_offset())).collect();
                }
                body_walk(&mut scan, &body);
            }
            self.scan = Some(scan);
            return;
        }
        ruby_prism::visit_def_node(self, node);
    }
}

fn body_walk(scan: &mut Scan, body: &Node<'_>) {
    let mut walker = Walker { scan };
    walker.visit(body);
}

struct Walker<'s> {
    scan: &'s mut Scan,
}

impl Walker<'_> {
    fn local(&mut self, name: &[u8], write: bool) {
        let entry = self.scan.locals.entry(text(name)).or_default();
        if write {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }
}

impl<'pr> Visit<'pr> for Walker<'_> {
    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
        let name = text(node.name().as_slice());
        if LOCAL_OBSERVERS.contains(&name.as_str()) {
            self.scan.observer.get_or_insert(name.clone());
        }
        let range = (node.location().start_offset(), node.location().end_offset());
        if name == "to_json" {
            if let Some(receiver) = node.receiver() {
                let refusal = if node.arguments().is_some() || node.block().is_some() {
                    Some("`to_json` is called with options or a block")
                } else if node.is_safe_navigation() {
                    Some("`to_json` is called with `&.`")
                } else {
                    None
                };
                if let Some(hash) = receiver.as_hash_node() {
                    match refusal {
                        Some(reason) => self.scan.declined.push((range.0, range.1, reason.to_string())),
                        None => self.scan.sites.push(RawSite::Literal { call: range, pairs: pairs(&hash) }),
                    }
                } else if let Some(read) = receiver.as_local_variable_read_node().filter(|r| r.depth() == 0) {
                    match refusal {
                        Some(reason) => self.scan.declined.push((range.0, range.1, reason.to_string())),
                        None => self.scan.sites.push(RawSite::Local { call: range, name: text(read.name().as_slice()) }),
                    }
                } else {
                    self.scan.declined.push((range.0, range.1, "the receiver of `to_json` is not a hash built in this method".to_string()));
                }
            }
        } else if name == "generate" && node.receiver().is_some_and(|r| is_json_constant(&r)) {
            self.scan.declined.push((
                range.0,
                range.1,
                "`JSON.generate` is the json gem's native generator, and a Ruby writer measured slower than it".to_string(),
            ));
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_local_variable_read_node(&mut self, node: &ruby_prism::LocalVariableReadNode<'pr>) {
        self.local(node.name().as_slice(), false);
    }

    fn visit_local_variable_write_node(&mut self, node: &ruby_prism::LocalVariableWriteNode<'pr>) {
        self.local(node.name().as_slice(), true);
        if node.depth() == 0 {
            if let Some(hash) = node.value().as_hash_node() {
                let loc = node.location();
                self.scan.hash_writes.insert(text(node.name().as_slice()), (loc.start_offset(), loc.end_offset(), pairs(&hash)));
            }
        }
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_local_variable_operator_write_node(&mut self, node: &ruby_prism::LocalVariableOperatorWriteNode<'pr>) {
        self.local(node.name().as_slice(), true);
        ruby_prism::visit_local_variable_operator_write_node(self, node);
    }

    fn visit_local_variable_or_write_node(&mut self, node: &ruby_prism::LocalVariableOrWriteNode<'pr>) {
        self.local(node.name().as_slice(), true);
        ruby_prism::visit_local_variable_or_write_node(self, node);
    }

    fn visit_local_variable_and_write_node(&mut self, node: &ruby_prism::LocalVariableAndWriteNode<'pr>) {
        self.local(node.name().as_slice(), true);
        ruby_prism::visit_local_variable_and_write_node(self, node);
    }

    fn visit_local_variable_target_node(&mut self, node: &ruby_prism::LocalVariableTargetNode<'pr>) {
        self.local(node.name().as_slice(), true);
    }

    fn visit_string_node(&mut self, node: &ruby_prism::StringNode<'pr>) {
        self.scan.heredoc |= node.opening_loc().is_some_and(|o| o.as_slice().starts_with(b"<<"));
    }

    fn visit_interpolated_string_node(&mut self, node: &ruby_prism::InterpolatedStringNode<'pr>) {
        self.scan.heredoc |= node.opening_loc().is_some_and(|o| o.as_slice().starts_with(b"<<"));
        ruby_prism::visit_interpolated_string_node(self, node);
    }

    fn visit_x_string_node(&mut self, node: &ruby_prism::XStringNode<'pr>) {
        self.scan.heredoc |= node.opening_loc().as_slice().starts_with(b"<<");
    }

    fn visit_interpolated_x_string_node(&mut self, node: &ruby_prism::InterpolatedXStringNode<'pr>) {
        self.scan.heredoc |= node.opening_loc().as_slice().starts_with(b"<<");
        ruby_prism::visit_interpolated_x_string_node(self, node);
    }
}

/// `JSON` or `::JSON`.
fn is_json_constant(node: &Node<'_>) -> bool {
    if let Some(read) = node.as_constant_read_node() {
        return read.name().as_slice() == b"JSON";
    }
    node.as_constant_path_node().is_some_and(|path| path.parent().is_none() && path.name().is_some_and(|n| n.as_slice() == b"JSON"))
}

/// The hash literal's pairs, or why it is not a fixed shape.
fn pairs(hash: &ruby_prism::HashNode<'_>) -> Result<Vec<RawPair>, String> {
    let mut out: Vec<RawPair> = Vec::new();
    for element in hash.elements().iter() {
        let Some(assoc) = element.as_assoc_node() else {
            return Err("the hash has a splat".to_string());
        };
        let key = assoc.key();
        let name = if let Some(sym) = key.as_symbol_node() {
            text(sym.unescaped())
        } else if let Some(string) = key.as_string_node().filter(|s| quoted(s.opening_loc())) {
            text(string.unescaped())
        } else {
            return Err(format!("the key `{}` is not a plain symbol or string literal", text(key.location().as_slice())));
        };
        if !is_identifier(&name) {
            return Err(format!("the key `{}` is not a plain identifier", text(key.location().as_slice())));
        }
        if out.iter().any(|p| p.name == name) {
            return Err(format!("the key `{name}` appears twice, and an overwritten value can still have effects"));
        }
        let value = assoc.value();
        if value.as_implicit_node().is_some() {
            return Err(format!("the value of `{name}` is implicit (`{name}:`)"));
        }
        let loc = value.location();
        out.push(RawPair {
            name,
            key_src: text(key.location().as_slice()),
            arrow: assoc.operator_loc().is_some(),
            value: baked(&value).map_or(RawValue::Expr, RawValue::Baked),
            range: (loc.start_offset(), loc.end_offset()),
            value_src: text(loc.as_slice()),
            plain_read: value.as_local_variable_read_node().is_some()
                || value.as_instance_variable_read_node().is_some()
                || value.as_self_node().is_some(),
        });
    }
    Ok(out)
}

/// The JSON of a literal whose encoding cannot vary, if it is one.
fn baked(value: &Node<'_>) -> Option<String> {
    if value.as_nil_node().is_some() {
        return Some("null".into());
    }
    if value.as_true_node().is_some() {
        return Some("true".into());
    }
    if value.as_false_node().is_some() {
        return Some("false".into());
    }
    if let Some(int) = value.as_integer_node().filter(|i| i.is_decimal()) {
        let digits = text(int.location().as_slice());
        let body = digits.strip_prefix('-').unwrap_or(&digits);
        if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) {
            return Some(if body.bytes().all(|b| b == b'0') { "0".into() } else { digits });
        }
        return None;
    }
    if let Some(string) = value.as_string_node().filter(|s| quoted(s.opening_loc())) {
        let unescaped = string.unescaped();
        let verbatim = unescaped == string.content_loc().as_slice();
        if verbatim && unescaped.iter().all(|&b| (0x20..0x7f).contains(&b) && !b"\"\\<>&".contains(&b)) {
            return Some(format!("\"{}\"", text(unescaped)));
        }
    }
    None
}

fn quoted(opening: Option<ruby_prism::Location<'_>>) -> bool {
    opening.is_some_and(|o| o.as_slice() == b"\"" || o.as_slice() == b"'")
}

fn is_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The JSON scalar shapes the writer reproduces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Nil,
    Bool,
    Int,
    Str,
}

fn kinds_of(ty: &Ty, out: &mut BTreeSet<Kind>) -> bool {
    match ty {
        Ty::Nil => out.insert(Kind::Nil),
        Ty::Bool => out.insert(Kind::Bool),
        Ty::Int => out.insert(Kind::Int),
        Ty::Str => out.insert(Kind::Str),
        Ty::Union { variants } => return variants.iter().all(|v| kinds_of(v, out)),
        _ => return false,
    };
    true
}

fn describe(ty: &Ty) -> String {
    match ty {
        Ty::Float => "Float".into(),
        Ty::Sym => "Symbol".into(),
        Ty::Date => "Date".into(),
        Ty::Time => "Time".into(),
        Ty::Untyped | Ty::Var { .. } => "no resolved type".into(),
        Ty::Array { .. } | Ty::Tuple { .. } => "an Array type".into(),
        Ty::Hash { .. } | Ty::Record { .. } => "a Hash type".into(),
        Ty::Union { .. } => "a union beyond one scalar or nil".into(),
        _ => "a type the writer does not cover".into(),
    }
}

/// Every analyzed copy's type for the expression at `range`.
fn kinds_at(region: &Region<'_>, range: (usize, usize)) -> Result<BTreeSet<Kind>, String> {
    if region.analyzed.is_empty() {
        return Err("the analyzer did not model the method".into());
    }
    let mut kinds = BTreeSet::new();
    for copy in &region.analyzed {
        let mut found: Option<&Expr> = None;
        walk(copy.body, &mut |e| {
            if found.is_none() && e.span.start as usize == range.0 && e.span.end as usize == range.1 {
                found = Some(e);
            }
        });
        let Some(ty) = found.and_then(|e| e.ty.as_ref()) else {
            return Err("the analyzer has no type for it".into());
        };
        if !kinds_of(ty, &mut kinds) {
            return Err(format!("it has {}", describe(ty)));
        }
    }
    let nil = kinds.contains(&Kind::Nil);
    if kinds.len() == 1 || (kinds.len() == 2 && nil) {
        Ok(kinds)
    } else {
        Err("it may be more than one of String, Integer and true/false".into())
    }
}

fn walk<'a>(e: &'a Expr, f: &mut dyn FnMut(&'a Expr)) {
    f(e);
    e.node.for_each_child(&mut |child| walk(child, f));
}

/// One value of a site, ready to generate.
enum Value {
    Baked(String),
    Checked { temp: String, kinds: BTreeSet<Kind> },
}

struct Site {
    pairs: Vec<(RawPair, Value)>,
}

impl Site {
    fn checked(&self) -> impl Iterator<Item = (&RawPair, &str, &BTreeSet<Kind>)> {
        self.pairs.iter().filter_map(|(p, v)| match v {
            Value::Checked { temp, kinds } => Some((p, temp.as_str(), kinds)),
            Value::Baked(_) => None,
        })
    }

    /// The condition, the writer and the original expression, as one
    /// Ruby expression evaluated after the temporaries.
    fn tail(&self) -> String {
        let mut body = String::from("{");
        for (i, (pair, value)) in self.pairs.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!("\\\"{}\\\":", pair.name));
            match value {
                Value::Baked(json) => body.push_str(&ruby_dq(json)),
                Value::Checked { temp, kinds } => body.push_str(&fragment(kinds, temp)),
            }
        }
        body.push('}');
        let writer = if body.contains("#{") { format!("\"{body}\"") } else { format!("\"{body}\".dup") };
        let checks: Vec<String> = self.checked().map(|(_, temp, kinds)| check(kinds, temp)).collect();
        if checks.is_empty() {
            return writer;
        }
        let original = self
            .pairs
            .iter()
            .map(|(pair, value)| {
                let value = match value {
                    Value::Checked { temp, .. } => temp.clone(),
                    Value::Baked(_) => pair.value_src.clone(),
                };
                if pair.arrow { format!("{} => {value}", pair.key_src) } else { format!("{} {value}", pair.key_src) }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} ? {writer} : {{ {original} }}.to_json", checks.join(" && "))
    }
}

/// The text inside the writer's double-quoted literal for one value.
fn fragment(kinds: &BTreeSet<Kind>, t: &str) -> String {
    let nilable = kinds.contains(&Kind::Nil) && kinds.len() == 2;
    let kind = *kinds.iter().find(|k| **k != Kind::Nil).unwrap_or(&Kind::Nil);
    match (kind, nilable) {
        (Kind::Nil, _) => "null".into(),
        (Kind::Str, false) => format!("\\\"#{{{t}}}\\\""),
        (Kind::Str, true) => format!("#{{nil.equal?({t}) ? \"null\" : \"\\\"#{{{t}}}\\\"\"}}"),
        (_, false) => format!("#{{{t}}}"),
        (_, true) => format!("#{{nil.equal?({t}) ? \"null\" : {t}}}"),
    }
}

/// The test that `t` is a value whose JSON is exactly what
/// [`fragment`] writes. It calls only core methods: on the classes, on
/// the singletons, on the runtime's constants, and on `t` once it is
/// known to be a String.
fn check(kinds: &BTreeSet<Kind>, t: &str) -> String {
    let one = |kind: Kind| match kind {
        Kind::Nil => format!("nil.equal?({t})"),
        Kind::Bool => format!("(true.equal?({t}) || false.equal?({t}))"),
        Kind::Int => format!("::Integer === {t}"),
        Kind::Str => format!(
            "(::String === {t} && {t}.instance_of?(::String) && ({t}.ascii_only? || ({RT}::UTF_8.equal?({t}.encoding) && {t}.valid_encoding? && !{t}.include?({RT}::LINE_SEPARATOR) && !{t}.include?({RT}::PARAGRAPH_SEPARATOR))) && !{RT}::ESCAPE.match?({t}))"
        ),
    };
    let parts: Vec<String> = kinds.iter().map(|k| one(*k)).collect();
    if parts.len() == 1 { parts[0].clone() } else { format!("({})", parts.join(" || ")) }
}

/// `json` inside a Ruby double-quoted literal. Baked JSON is printable
/// ASCII without backslashes; quotes and `#` are escaped.
fn ruby_dq(json: &str) -> String {
    json.replace('"', "\\\"").replace('#', "\\#")
}

/// Rewrite `range` keeping only its line breaks and the `kept` values,
/// each assigned to its temporary, so every value stays on its line.
fn layout(text: &str, range: (usize, usize), kept: &[(usize, usize, String)]) -> String {
    let mut out = String::new();
    let mut at = range.0;
    let gap = |out: &mut String, from: usize, to: usize| {
        let newlines = text[from..to].matches('\n').count();
        if newlines == 0 {
            out.push(' ');
        }
        for _ in 0..newlines {
            out.push('\n');
        }
    };
    for (start, end, temp) in kept {
        gap(&mut out, at, *start);
        out.push_str(&format!("{temp} = ({}); ", &text[*start..*end]));
        at = *end;
    }
    gap(&mut out, at, range.1);
    out
}

fn plan(region: &Region<'_>, text: &str, mut scan: Scan) -> Proposal {
    let at = |offset: usize| Span { file: region.file, start: offset as u32, end: offset as u32 };
    let mut proposal = Proposal::default();
    for (start, _, reason) in std::mem::take(&mut scan.declined) {
        proposal.declined.push((at(start), reason));
    }
    if scan.sites.is_empty() {
        return proposal;
    }
    let def_text = &text[region.def_span.start as usize..region.def_span.end as usize];
    let method_refusal = if let Some(call) = &scan.observer {
        Some(format!("the method calls `{call}`, which can observe the locals a variant adds"))
    } else if scan.heredoc {
        Some("the method has a heredoc, whose body a rewrite could move".to_string())
    } else if def_text.contains(TEMP) {
        Some(format!("the method already mentions `{TEMP}`"))
    } else {
        None
    };
    if let Some(reason) = method_refusal {
        for site in &scan.sites {
            let (RawSite::Literal { call, .. } | RawSite::Local { call, .. }) = site;
            proposal.declined.push((at(call.0), reason.clone()));
        }
        return proposal;
    }

    // Every site that can be rewritten, with the spans it would replace.
    let mut ready: Vec<(Vec<(usize, usize)>, Vec<Edit>, bool)> = Vec::new();
    let span = |(start, end): (usize, usize)| Span { file: region.file, start: start as u32, end: end as u32 };
    let sites = std::mem::take(&mut scan.sites);
    for (index, site) in sites.into_iter().enumerate() {
        let (call, write, raw) = match site {
            RawSite::Literal { call, pairs } => (call, None, pairs),
            RawSite::Local { call, name } => match local_site(&mut scan, call, &name) {
                Ok((write, pairs)) => (call, Some(write), Ok(pairs)),
                Err(reason) => {
                    proposal.declined.push((at(call.0), reason));
                    continue;
                }
            },
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(reason) => {
                proposal.declined.push((at(call.0), reason));
                continue;
            }
        };
        let mut pairs = Vec::new();
        let mut refusal = None;
        for (i, pair) in raw.into_iter().enumerate() {
            let value = match &pair.value {
                RawValue::Baked(json) => Value::Baked(json.clone()),
                RawValue::Expr => match kinds_at(region, pair.range) {
                    Ok(kinds) => Value::Checked { temp: format!("{TEMP}{index}_{i}"), kinds },
                    Err(reason) => {
                        refusal = Some(format!("the value of `{}` (`{}`): {reason}", pair.name, pair.value_src));
                        break;
                    }
                },
            };
            pairs.push((pair, value));
        }
        if let Some(reason) = refusal {
            proposal.declined.push((at(call.0), reason));
            continue;
        }
        let site = Site { pairs };
        let plain = site.checked().all(|(p, _, _)| p.plain_read);
        let kept: Vec<(usize, usize, String)> = site.checked().map(|(p, temp, _)| (p.range.0, p.range.1, temp.to_string())).collect();
        let (spans, edits) = match write {
            None => {
                let replacement = format!("({}{})", layout(text, call, &kept), site.tail());
                (vec![call], vec![Edit { span: span(call), text: replacement }])
            }
            Some(write) => (
                vec![write, call],
                vec![
                    Edit { span: span(write), text: format!("({})", layout(text, write, &kept)) },
                    Edit { span: span(call), text: format!("({}{})", layout(text, call, &[]), site.tail()) },
                ],
            ),
        };
        ready.push((spans, edits, plain));
    }
    // Outermost first: a site written inside another's value stays part
    // of that value's text.
    ready.sort_by_key(|(spans, _, _)| (spans[0].0, std::cmp::Reverse(spans[spans.len() - 1].1)));
    let mut taken: Vec<(usize, usize)> = Vec::new();
    let mut replayable = true;
    for (spans, edits, plain) in ready {
        if spans.iter().any(|s| taken.iter().any(|t| s.0 < t.1 && t.0 < s.1)) {
            continue;
        }
        taken.extend(spans);
        proposal.edits.extend(edits);
        replayable &= plain;
    }
    if !proposal.edits.is_empty() {
        proposal.guards.push(Guard::ActiveSupportJson);
        proposal.replayable = replayable;
    }
    proposal
}

/// For `name.to_json`: the `name = { ... }` it encodes, when that hash
/// cannot be seen anywhere else.
fn local_site(scan: &mut Scan, call: (usize, usize), name: &str) -> Result<((usize, usize), Vec<RawPair>), String> {
    let Some((ws, we, _)) = scan.hash_writes.get(name) else {
        return Err(format!("`{name}` is not assigned a hash literal in this method"));
    };
    let write = (*ws, *we);
    let (writes, reads) = scan.locals.get(name).copied().unwrap_or_default();
    if writes != 1 || reads != 1 {
        return Err(format!("`{name}` is written {writes} time(s) and read {reads} time(s); the hash must be built once and read only by `to_json`"));
    }
    if scan.params.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|word| word == name) {
        return Err(format!("`{name}` is also a parameter"));
    }
    let top_index = |range: (usize, usize)| scan.top.iter().position(|t| *t == range);
    match (top_index(write), top_index(call)) {
        (Some(w), Some(c)) if w < c => {}
        _ => return Err(format!("`{name} = {{...}}` and `{name}.to_json` must both be statements of the method body, in that order")),
    }
    let (_, _, pairs) = scan.hash_writes.remove(name).expect("checked above");
    pairs.map(|p| (write, p))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
