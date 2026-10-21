//! `inline-enum-predicates`: a call to an Active Record enum predicate
//! inside the enum's own class becomes the comparison Rails runs.
//!
//! Rails defines each predicate with
//! `define_method("#{value_method_name}?") { public_send(:"#{name}_for_database") == value }`
//! (unchanged from 7.1 to 8.1 and main), so inside a method of the model
//!
//! ```ruby
//! enum :status, { draft: 0, published: 1 }
//! def listed? = published? && !hidden?
//! ```
//!
//! `published?` becomes `(status_for_database == 1)`: the same comparison,
//! with no block-backed method (which ZJIT cannot call directly) and no
//! `public_send` of a dynamic symbol in between. `status_for_database`
//! is an ordinary `def` Active Record generates.
//!
//! The compiled tree has no runtime fallback, so everything the rewrite
//! assumes is checked at build time, and anything it cannot check is
//! declined:
//!
//! - Active Record is 7.1 or later in `Gemfile.lock`;
//! - the enum is declared with literals in a class body of the compiled
//!   file (`enum :status, {...}`, `enum :status, [...]` or
//!   `enum :status, a: 0, b: 1`), with literal `prefix:`/`suffix:`;
//!   symbol values are declined, because Rails 8.1 compares their name and
//!   earlier versions the symbol;
//! - no source in the app defines the predicate or `<name>_for_database`
//!   by any other means, no other enum the app declares produces the same
//!   predicate, and no subclass (or module one includes) declares an enum
//!   this pass cannot read, which could produce it;
//! - the call has no receiver, arguments or block, sits directly in an
//!   instance method of that class in the same file (not in a block, where `self` may
//!   differ, nor under `defined?`), and the method has no local variable
//!   named `<name>_for_database`.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use ruby_prism::{Node, Visit};

use super::definitions::{DYNAMIC, literal_name};
use super::pass::{Cx, Edit, Pass, Proposal, Unit};
use super::scope::Scope;
use crate::span::Span;

#[derive(Default)]
pub struct EnumPredicates {
    /// Predicate name -> how many enum declarations in the app produce
    /// it, and the files holding declarations this pass cannot read.
    app: OnceCell<AppEnums>,
}

#[derive(Default)]
struct AppEnums {
    produced: HashMap<String, usize>,
    unreadable_in: Vec<String>,
}

/// One predicate an enum declaration generates.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Predicate {
    attribute: String,
    /// The stored value, as Ruby source.
    value: String,
}

/// What a declaration yields: its predicates, or why it cannot be read.
type Declared = Result<BTreeMap<String, Predicate>, String>;

impl Pass for EnumPredicates {
    fn name(&self) -> &'static str {
        "inline-enum-predicates"
    }

    fn summary(&self) -> &'static str {
        "Inside the enum's own class, a call to an enum predicate (`published?`) becomes the comparison Rails' generated method runs (`(status_for_database == 1)`)."
    }

    fn scope(&self) -> Scope {
        // Everything it needs it reads from source through `Cx`.
        Scope::Files(Vec::new())
    }

    fn run(&self, cx: &Cx<'_>, unit: &Unit<'_>) -> Proposal {
        let mut proposal = Proposal::default();
        let parsed = ruby_prism::parse(unit.text.as_bytes());
        let frozen = unit.frozen_string_literals();

        // The enums each class declares in this file.
        let mut collect = Declarations { unit, frozen, by_owner: BTreeMap::new(), twice: BTreeSet::new(), declined: Vec::new() };
        collect.visit(&parsed.node());
        proposal.declined.append(&mut collect.declined);
        if collect.by_owner.is_empty() {
            return proposal;
        }
        if let Some(reason) = rails_refusal(cx) {
            if let Some((span, _)) = collect.by_owner.values().flat_map(|predicates| predicates.values()).next() {
                proposal.declined.push((*span, reason));
            }
            return proposal;
        }

        // Build-time checks that the predicates are Rails' own.
        let app = self.app.get_or_init(|| app_enums(cx));
        let defined = cx.defined_methods();
        let mut usable: BTreeMap<&str, BTreeMap<String, Predicate>> = BTreeMap::new();
        for (owner, predicates) in &collect.by_owner {
            let files = hierarchy_files(cx, owner);
            let hierarchy_unknown = app.unreadable_in.iter().find(|file| files.contains(file));
            let computed = defined.get(DYNAMIC).and_then(|sites| sites.iter().find(|site| files.iter().any(|f| site.starts_with(&format!("{f}:")))));
            for (name, (span, predicate)) in predicates {
                let reason = if let Some(file) = &hierarchy_unknown {
                    Some(format!("`{owner}` or a class below it declares an enum this pass cannot read ({file}), which could also define `{name}`"))
                } else if let Some(sites) = defined.get(name) {
                    Some(format!("the app defines `{name}` itself ({})", sites.join(", ")))
                } else if let Some(sites) = defined.get(&format!("{}_for_database", predicate.attribute)) {
                    Some(format!("the app defines `{}_for_database` itself ({})", predicate.attribute, sites.join(", ")))
                } else if app.produced.get(name).copied().unwrap_or(0) > 1 {
                    Some(format!("another enum in the app also defines `{name}`"))
                } else {
                    // A computed `define_method` could produce any name.
                    computed.map(|site| format!("`{owner}` or a class below it defines methods with computed names ({site}), which could include `{name}`"))
                };
                match reason {
                    Some(reason) => proposal.declined.push((*span, reason)),
                    None => {
                        usable.entry(owner.as_str()).or_default().insert(name.clone(), predicate.clone());
                    }
                }
            }
        }
        if usable.is_empty() {
            return proposal;
        }

        let mut sites = Sites { unit, usable: &usable, stack: Vec::new(), proposal: &mut proposal };
        sites.visit(&parsed.node());
        proposal
    }
}

/// Active Record must be one whose predicate is the one this pass
/// reproduces.
fn rails_refusal(cx: &Cx<'_>) -> Option<String> {
    let Some(version) = cx.gem_version("activerecord") else {
        return Some("Gemfile.lock pins no activerecord, so which predicate Rails generates is unknown".to_string());
    };
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) < (7, 1) {
        return Some(format!("activerecord {version} is older than 7.1, the first version this pass reproduces"));
    }
    None
}

/// Every enum declaration in the app's source, read leniently: what
/// predicates each produces, and where one cannot be read.
fn app_enums(cx: &Cx<'_>) -> AppEnums {
    let mut out = AppEnums::default();
    for source in cx.app_sources() {
        if !source.text.contains("enum") {
            continue;
        }
        let parsed = ruby_prism::parse(source.text.as_bytes());
        let mut finder = AnyEnum { found: Vec::new() };
        finder.visit(&parsed.node());
        for declared in finder.found {
            match declared {
                Ok(predicates) => {
                    for name in predicates.into_keys() {
                        *out.produced.entry(name).or_default() += 1;
                    }
                }
                Err(_) => out.unreadable_in.push(cx.relative(&source.path)),
            }
        }
    }
    out.unreadable_in.sort();
    out.unreadable_in.dedup();
    out
}

/// The files defining `owner`.
fn unit_files(cx: &Cx<'_>, owner: &str) -> Vec<String> {
    cx.index().files.get(owner).map(|paths| paths.iter().map(|p| cx.relative(p)).collect()).unwrap_or_default()
}

/// The files that can define methods on `owner`'s instances besides
/// Rails: `owner`'s own, its subclasses', and those of the modules and
/// classes between each subclass and `owner`.
fn hierarchy_files(cx: &Cx<'_>, owner: &str) -> Vec<String> {
    let index = cx.index();
    let mut names: Vec<&str> = vec![owner];
    for descendant in cx.descendants(owner) {
        names.push(descendant);
        for ancestor in index.ancestors.get(descendant).into_iter().flatten() {
            if ancestor.as_ref() == owner {
                break;
            }
            names.push(ancestor.as_ref());
        }
    }
    let mut files: Vec<String> = names.iter().flat_map(|name| unit_files(cx, name)).collect();
    files.sort();
    files.dedup();
    files
}

/// Collects the enum declarations written directly in each class
/// body of a unit.
struct Declarations<'u, 'a> {
    unit: &'u Unit<'a>,
    frozen: bool,
    by_owner: BTreeMap<String, BTreeMap<String, (Span, Predicate)>>,
    /// (owner, predicate) pairs two declarations define.
    twice: BTreeSet<(String, String)>,
    declined: Vec<(Span, String)>,
}

impl<'pr> Visit<'pr> for Declarations<'_, '_> {
    fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
        let loc = node.location();
        let span = self.unit.span(loc.start_offset(), loc.end_offset());
        if let Some(owner) = self.unit.body_at(span).map(str::to_string) {
            let statements = node.body().and_then(|b| b.as_statements_node());
            for statement in statements.iter().flat_map(|s| s.body().iter()) {
                let Some(call) = statement.as_call_node() else { continue };
                if call.receiver().is_some() || call.name().as_slice() != b"enum" {
                    continue;
                }
                let at = self.unit.span(call.location().start_offset(), call.location().end_offset());
                match declaration(&call, self.frozen) {
                    Ok(predicates) => {
                        let entry = self.by_owner.entry(owner.clone()).or_default();
                        for (name, predicate) in predicates {
                            let key = (owner.clone(), name.clone());
                            if self.twice.contains(&key) || entry.remove(&name).is_some() {
                                // Rails refuses a second enum defining it at load.
                                self.declined.push((at, format!("`{name}` is defined by two enums in this class")));
                                self.twice.insert(key);
                            } else {
                                entry.insert(name, (at, predicate));
                            }
                        }
                    }
                    Err(reason) => self.declined.push((at, reason)),
                }
            }
        }
        ruby_prism::visit_class_node(self, node);
    }
}

/// Any receiverless `enum` call anywhere, for the app-wide count.
struct AnyEnum {
    found: Vec<Declared>,
}

impl<'pr> Visit<'pr> for AnyEnum {
    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
        if node.receiver().is_none() && node.name().as_slice() == b"enum" && node.arguments().is_some() {
            self.found.push(declaration(node, true));
        }
        ruby_prism::visit_call_node(self, node);
    }
}

/// The predicates `enum ...` defines, keyed by method name, or why this
/// pass cannot say. Rails 7.1+: `enum(name, values = nil, **options)`;
/// with no positional mapping, every keyword is a label.
fn declaration(call: &ruby_prism::CallNode<'_>, frozen: bool) -> Declared {
    if call.block().is_some() {
        return Err("an `enum` call with a block".to_string());
    }
    let args: Vec<Node<'_>> = call.arguments().map(|a| a.arguments().iter().collect()).unwrap_or_default();
    let Some(first) = args.first() else { return Err("an `enum` call with no arguments".to_string()) };
    if first.as_keyword_hash_node().is_some() {
        return Err("the keyword form `enum name: {...}` (removed in Rails 8.0); write `enum :name, {...}`".to_string());
    }
    let Some(attribute) = literal_name(first) else { return Err("the enum's name is not a literal".to_string()) };
    if !identifier(&attribute) {
        return Err(format!("the enum name `{attribute}` is not a plain method name"));
    }
    let (labels, options): (Vec<(String, String)>, Option<&Node<'_>>) = match (args.get(1), args.get(2), args.len()) {
        (Some(mapping), options, 2 | 3) if mapping.as_keyword_hash_node().is_some() && options.is_none() => {
            let kh = mapping.as_keyword_hash_node().expect("checked");
            (pairs(&kh.elements(), frozen)?, None)
        }
        (Some(mapping), options, 2 | 3) => {
            if options.is_some_and(|o| o.as_keyword_hash_node().is_none()) {
                return Err("the enum's options are not literal keywords".to_string());
            }
            let labels = if let Some(hash) = mapping.as_hash_node() {
                pairs(&hash.elements(), frozen)?
            } else if let Some(array) = mapping.as_array_node() {
                let mut out = Vec::new();
                for (i, element) in array.elements().iter().enumerate() {
                    let Some(label) = literal_name(&element) else { return Err("an enum label is not a literal".to_string()) };
                    out.push((label, i.to_string()));
                }
                out
            } else {
                return Err("the enum's mapping is not a literal hash or array".to_string());
            };
            (labels, options)
        }
        _ => return Err("an `enum` call of a shape this pass does not read".to_string()),
    };

    let (mut prefix, mut suffix, mut instance_methods) = (String::new(), String::new(), true);
    if let Some(options) = options.and_then(|o| o.as_keyword_hash_node()) {
        for element in options.elements().iter() {
            let Some(pair) = element.as_assoc_node() else { return Err("the enum's options splat a hash".to_string()) };
            let Some(key) = literal_name(&pair.key()) else { return Err("an enum option key is not a literal".to_string()) };
            let value = pair.value();
            match key.as_str() {
                "prefix" | "suffix" => {
                    let affix = if value.as_true_node().is_some() {
                        Some(attribute.clone())
                    } else if value.as_false_node().is_some() || value.as_nil_node().is_some() {
                        None
                    } else if let Some(name) = literal_name(&value) {
                        Some(name)
                    } else {
                        return Err(format!("the enum's `{key}:` is not a literal"));
                    };
                    let affix = affix.map(|a| if key == "prefix" { format!("{a}_") } else { format!("_{a}") }).unwrap_or_default();
                    if key == "prefix" { prefix = affix } else { suffix = affix }
                }
                "instance_methods" => {
                    instance_methods = if value.as_true_node().is_some() {
                        true
                    } else if value.as_false_node().is_some() {
                        false
                    } else {
                        return Err("the enum's `instance_methods:` is not a literal".to_string());
                    };
                }
                "scopes" | "default" | "validate" => {}
                other => return Err(format!("the enum option `{other}:` is one this pass does not know")),
            }
        }
    }
    let mut out = BTreeMap::new();
    if !instance_methods {
        return Ok(out);
    }
    for (label, value) in labels {
        let method = format!("{prefix}{label}{suffix}");
        // A label Ruby cannot call bare (`"in progress"`) gets an alias
        // from Rails; neither is rewritten here.
        if identifier(&method) {
            out.insert(format!("{method}?"), Predicate { attribute: attribute.clone(), value });
        }
    }
    Ok(out)
}

/// `label => value` pairs of a literal hash, with each value as the Ruby
/// source the rewrite compares against.
fn pairs(elements: &ruby_prism::NodeList<'_>, frozen: bool) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for element in elements.iter() {
        let Some(pair) = element.as_assoc_node() else { return Err("the enum's mapping splats a hash".to_string()) };
        let Some(label) = literal_name(&pair.key()) else { return Err("an enum label is not a literal".to_string()) };
        let value = pair.value();
        let source = String::from_utf8_lossy(value.location().as_slice()).into_owned();
        let rendered = if value.as_integer_node().is_some()
            || value.as_float_node().is_some()
            || value.as_true_node().is_some()
            || value.as_false_node().is_some()
            || value.as_nil_node().is_some()
        {
            source
        } else if value.as_string_node().is_some() {
            // Rails' predicate compares against one string object; an
            // unfrozen literal would allocate on every call.
            if frozen { source } else { format!("{source}.freeze") }
        } else if value.as_symbol_node().is_some() {
            return Err(format!("`{label}` has a symbol value, which Rails 8.1 compares by name and earlier versions as a symbol"));
        } else {
            return Err(format!("`{label}`'s value is not a literal"));
        };
        out.push((label, rendered));
    }
    Ok(out)
}

fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Visibility calls a `def` may be the argument of; any other enclosing
/// call or block may change what `def` defines on, or what `self` is.
const VISIBILITY: [&str; 3] = ["private", "protected", "public"];

/// Finds the rewritable calls.
struct Sites<'u, 'a, 'p> {
    unit: &'u Unit<'a>,
    usable: &'u BTreeMap<&'u str, BTreeMap<String, Predicate>>,
    stack: Vec<Node<'p>>,
    proposal: &'u mut Proposal,
}

impl<'pr> Visit<'pr> for Sites<'_, '_, 'pr> {
    fn visit_branch_node_enter(&mut self, node: Node<'pr>) {
        self.stack.push(node);
    }

    fn visit_branch_node_leave(&mut self) {
        self.stack.pop();
    }

    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        let name = node.name_loc();
        let Some(owner) = self.unit.owner_of(self.unit.span(name.start_offset(), name.end_offset())) else { return };
        let Some(predicates) = self.usable.get(owner) else { return };
        if node.receiver().is_some() || !plainly_in_class_body(&self.stack[..self.stack.len().saturating_sub(1)]) {
            return;
        }
        let locals: Vec<String> = node.locals().iter().map(|id| String::from_utf8_lossy(id.as_slice()).into_owned()).collect();
        let Some(body) = node.body() else { return };
        let mut calls = Calls { predicates, found: Vec::new() };
        calls.visit(&body);
        for (start, end, predicate) in calls.found {
            let span = self.unit.span(start, end);
            let reader = format!("{}_for_database", predicate.attribute);
            if locals.contains(&reader) {
                self.proposal.declined.push((span, format!("the method has a local variable `{reader}`")));
                continue;
            }
            self.proposal.edits.push(Edit { span, text: format!("({reader} == {})", predicate.value) });
        }
    }
}

/// The ancestors of a `def` (outermost first) allow only nesting that
/// defines it on the innermost class: statements, class and module
/// bodies, and visibility calls taking it as their argument.
fn plainly_in_class_body(ancestors: &[Node<'_>]) -> bool {
    ancestors.iter().enumerate().all(|(i, ancestor)| {
        ancestor.as_program_node().is_some()
            || ancestor.as_statements_node().is_some()
            || ancestor.as_class_node().is_some()
            || ancestor.as_module_node().is_some()
            || ancestor.as_call_node().is_some_and(|call| visibility_call(&call))
            || (ancestor.as_arguments_node().is_some()
                && i > 0
                && ancestors[i - 1].as_call_node().is_some_and(|call| visibility_call(&call)))
    })
}

fn visibility_call(call: &ruby_prism::CallNode<'_>) -> bool {
    call.receiver().is_none() && call.block().is_none() && VISIBILITY.contains(&String::from_utf8_lossy(call.name().as_slice()).as_ref())
}

/// Receiverless, argument-less predicate calls in a method body, outside
/// blocks, lambdas, nested definitions and `defined?`.
struct Calls<'u> {
    predicates: &'u BTreeMap<String, Predicate>,
    found: Vec<(usize, usize, &'u Predicate)>,
}

impl<'pr> Visit<'pr> for Calls<'_> {
    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
        if node.receiver().is_none() && node.arguments().is_none() && node.block().is_none() {
            let name = String::from_utf8_lossy(node.name().as_slice());
            if let Some(predicate) = self.predicates.get(name.as_ref()) {
                let loc = node.location();
                self.found.push((loc.start_offset(), loc.end_offset(), predicate));
            }
        }
        ruby_prism::visit_call_node(self, node);
    }

    fn visit_block_node(&mut self, _node: &ruby_prism::BlockNode<'pr>) {}
    fn visit_lambda_node(&mut self, _node: &ruby_prism::LambdaNode<'pr>) {}
    fn visit_def_node(&mut self, _node: &ruby_prism::DefNode<'pr>) {}
    fn visit_defined_node(&mut self, _node: &ruby_prism::DefinedNode<'pr>) {}
    fn visit_class_node(&mut self, _node: &ruby_prism::ClassNode<'pr>) {}
    fn visit_module_node(&mut self, _node: &ruby_prism::ModuleNode<'pr>) {}
    fn visit_singleton_class_node(&mut self, _node: &ruby_prism::SingletonClassNode<'pr>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(src: &str) -> Declared {
        let parsed = ruby_prism::parse(src.as_bytes());
        let mut finder = AnyEnum { found: Vec::new() };
        finder.visit(&parsed.node());
        finder.found.into_iter().next().expect("an enum call")
    }

    fn names(src: &str) -> Vec<(String, String, String)> {
        declared(src).unwrap().into_iter().map(|(n, p)| (n, p.attribute, p.value)).collect()
    }

    #[test]
    fn reads_hash_array_and_keyword_mappings() {
        assert_eq!(
            names("enum :status, { draft: 0, published: 1 }"),
            [("draft?".into(), "status".into(), "0".into()), ("published?".into(), "status".into(), "1".into())]
        );
        assert_eq!(names("enum :size, %i[small large]"), [("large?".into(), "size".into(), "1".into()), ("small?".into(), "size".into(), "0".into())]);
        // With no positional mapping every keyword is a label, `prefix` too.
        assert_eq!(
            names("enum :kind, a: 0, prefix: 1").into_iter().map(|(n, _, _)| n).collect::<Vec<_>>(),
            ["a?", "prefix?"]
        );
    }

    #[test]
    fn applies_prefix_and_suffix() {
        let got: Vec<String> = names("enum :status, { published: 1 }, prefix: true, suffix: :now").into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(got, ["status_published_now?"]);
        let got: Vec<String> = names("enum :status, [:published], prefix: \"is\"").into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(got, ["is_published?"]);
    }

    #[test]
    fn freezes_string_values_unless_the_file_does() {
        let parsed = ruby_prism::parse(b"enum :state, { open: \"o\" }");
        let call = parsed.node().as_program_node().unwrap().statements().body().iter().next().unwrap();
        let call = call.as_call_node().unwrap();
        assert_eq!(declaration(&call, false).unwrap()["open?"].value, "\"o\".freeze");
        assert_eq!(declaration(&call, true).unwrap()["open?"].value, "\"o\"");
    }

    #[test]
    fn declines_what_it_cannot_reproduce() {
        assert!(declared("enum :status, { published: :p }").unwrap_err().contains("symbol value"));
        assert!(declared("enum status: { published: 1 }").unwrap_err().contains("keyword form"));
        assert!(declared("enum :status, STATUSES").unwrap_err().contains("not a literal"));
        assert!(declared("enum :status, { published: 1 }, prefix: PREFIX").unwrap_err().contains("prefix"));
        assert!(declared("enum :status, { published: 1 }, _prefix: true").unwrap_err().contains("does not know"));
        assert_eq!(declared("enum :status, { published: 1 }, instance_methods: false").unwrap().len(), 0);
        // A label Ruby cannot call bare is skipped, not declined.
        assert_eq!(names("enum :status, { \"in progress\" => 0, done: 1 }").into_iter().map(|(n, _, _)| n).collect::<Vec<_>>(), ["done?"]);
    }
}
