//! Fractal's query rewriter: static Active Record chains inside the
//! selected classes.
//!
//! Started as a copy of `--target futamura`'s specialization 1
//! (`emit::futamura::query`) and kept apart from it on purpose, so the two
//! targets can change independently. A chain such as
//! `Account.where(name: @name).first` is decided here to be static except
//! for its request-time values, and rewritten as a Relation over bind
//! placeholders:
//!
//! ```ruby
//! Fractal.define(:key, Account, one: true, via: :first) { |p|
//!   Account.all.where(name: p.bind).send(:ordered_relation).limit(1) }
//! ```
//!
//! Rails builds that block once, at the first call, into an
//! `ActiveRecord::StatementCache`, and the runtime (`fractal.rb`) checks
//! on every call that the statement still gives Rails' answer. So the static
//! part is evaluated by Rails, not re-implemented here: this module only
//! decides what is static (binding-time analysis), which values are
//! request-time binds, and what the terminal means.
//!
//! - Root: a model constant, or a plain `has_many` reader on a typed owner
//!   (no `through:`, `as:`, scope lambda, or `primary_key:` — the bind is
//!   the owner's `id`, which a keyed association does not match).
//! - Links: `where(column: value)`, `order`/`preload`/`includes` with
//!   literal arguments, `limit` with a literal, and scopes. A scope whose
//!   body only builds a relation from literals (or calls other such scopes,
//!   or the `with_attached_*`/`with_rich_text_*` scopes Rails generates) is
//!   called by name in the block. One with parameters, or with a terminal
//!   in it, is inlined.
//! - Terminal: `first`/`last`/`take`/`find_by`, `first(n)`/`last(n)` with
//!   a literal count, or `to_a`.
//!
//! A site is rewritten only if it also clears these:
//!
//! - Every request-time value is a plain read (a local, an instance
//!   variable, `self`, a literal, or `owner.id` on one of those). The call
//!   site evaluates the values once for the statement, and the original
//!   chain, kept as the fallback, evaluates them again; a value that can
//!   differ between the two (`Time.current`, a helper with effects) would
//!   make the two paths disagree, and the order they run in would become
//!   observable.
//! - The app defines no method the rewrite would bypass: a class method
//!   named like the terminal (`def self.find_by`) on the model, or an
//!   instance method named like the association reader on its owner. The
//!   runtime checks the same at the first call, against the classes
//!   actually loaded, since ingest does not see every module a gem or
//!   concern mixes in.
//! - No request-time LIMIT: Active Record 8.1 casts a LIMIT with
//!   `Integer()` as it is set, so a placeholder raises while building.
//!
//! Anything else declines with a reason, and the site stays the app's. A
//! model that may have a `default_scope` is never rewritten: ingest does
//! not model it, so the generated root `where` could not be trusted to be
//! complete.

use std::collections::{HashMap, HashSet};

use crate::App;
use crate::dialect::{Association, MethodReceiver, Model, ModelBodyItem, Scope};
use crate::emit::ruby::emit_expr;
use crate::expr::{Expr, ExprNode, LValue, Literal};
use crate::ident::{ClassId, Symbol, VarId};
use crate::span::Span;
use crate::ty::Ty;

/// One specialized call site.
pub struct Site {
    pub span: Span,
    /// Ruby that replaces the call site's source text. `{orig}` stands for
    /// the original source of the site, kept as the fallback block.
    pub call: String,
    /// The `Fractal.define` line for the generated initializer.
    pub define: String,
}

/// A query-shaped site that was left to Rails, and why.
pub struct Residue {
    pub span: Span,
    pub reason: String,
}

#[derive(Clone)]
enum Val {
    /// Ruby source of a value fixed at build time.
    Static(String),
    /// Ruby source of a request-time value, evaluated at the call site.
    Bind(String),
}

#[derive(Clone)]
struct Chain {
    model: ClassId,
    /// `(owner source, association name)` for an association-rooted chain.
    owner: Option<(String, Symbol)>,
    /// Ruby link sources appended to the model constant, in call order.
    links: Vec<String>,
    /// Request-time values, in the order Rails binds them: WHERE values in
    /// call order.
    where_binds: Vec<String>,
    ordered: bool,
    limited: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Terminal {
    /// Not a terminal: the chain is still a relation.
    Link,
    /// Loads an Array of records (`to_a`, `first(n)`, `last(n)`); `reverse`
    /// when Rails reverses the loaded rows (`last(n)`).
    Records { reverse: bool },
    /// Loads one record or nil (`first`, `last`, `take`, `find_by`).
    One,
}

const TERMINALS: [&str; 5] = ["first", "last", "to_a", "take", "find_by"];
/// Relation builders a static scope body may use, with literal arguments.
const STATIC_BUILDERS: [&str; 9] = ["where", "order", "limit", "preload", "includes", "all", "joins", "left_joins", "select"];

pub struct Specializer<'a> {
    app: &'a App,
    models: HashMap<&'a str, &'a Model>,
    /// Integer class constants by bare name; `None` when two classes give
    /// the same name different values.
    int_consts: HashMap<String, Option<i64>>,
    /// Models that may carry a `default_scope`.
    default_scoped: Vec<String>,
}

impl<'a> Specializer<'a> {
    pub fn new(app: &'a App) -> Self {
        let models = app.models.iter().map(|m| (m.name.0.as_str(), m)).collect();
        let mut int_consts: HashMap<String, Option<i64>> = HashMap::new();
        for (name, value) in app.library_classes.iter().flat_map(|lc| lc.constants.iter()) {
            if let ExprNode::Lit { value: Literal::Int { value } } = &*value.node {
                let entry = int_consts.entry(name.as_str().to_string()).or_insert(Some(*value));
                if *entry != Some(*value) {
                    *entry = None;
                }
            }
        }
        let concern_mentions = app
            .sources
            .iter()
            .any(|s| s.path.contains("app/models/concerns/") && s.text.contains("default_scope"));
        let default_scoped = app
            .models
            .iter()
            .filter(|m| {
                concern_mentions
                    || app
                        .sources
                        .get((m.span.file.0 as usize).wrapping_sub(1))
                        .is_some_and(|s| s.text.contains("default_scope"))
            })
            .map(|m| m.name.0.as_str().to_string())
            .collect();
        Specializer { app, models, int_consts, default_scoped }
    }

    /// Every query site in a method body: specialized, or left with a reason.
    pub fn method_body(&self, body: &Expr, sites: &mut Vec<Site>, residue: &mut Vec<Residue>) {
        let mut locals = HashMap::new();
        collect_locals(body, &mut locals);
        let mut bound = HashSet::new();
        collect_bound(body, &mut bound);
        let cx = Cx {
            locals: &locals,
            bound: &bound,
            through_local: false,
            depth: Default::default(),
            args: HashMap::new(),
            self_chain: None,
        };
        self.visit(body, &cx, sites, residue);
    }

    fn visit(&self, e: &Expr, cx: &Cx, sites: &mut Vec<Site>, residue: &mut Vec<Residue>) {
        if let Some(outcome) = self.site(e, cx) {
            match outcome {
                Ok(site) => sites.push(site),
                Err(reason) => residue.push(Residue { span: e.span, reason }),
            }
            return;
        }
        e.node.for_each_child(&mut |c| self.visit(c, cx, sites, residue));
    }

    /// `None` when `e` is not a query terminal at all; otherwise the site
    /// or the reason it stays Rails'.
    fn site(&self, e: &Expr, cx: &Cx) -> Option<Result<Site, String>> {
        let ExprNode::Send { recv: Some(recv), method, args, block: None, .. } = &*e.node else {
            return None;
        };
        // The receiver's type names the model when analysis has one; a
        // chain it left untyped (a local built from nested scopes) is
        // walked instead, and is not a query site if that fails.
        let model = match relation_model(recv) {
            Some(id) => id.clone(),
            None if TERMINALS.contains(&method.as_str()) || self.any_scope_named(method) => {
                self.chain(recv, cx).ok()?.model
            }
            None => return None,
        };
        let m = self.models.get(model.0.as_str())?;
        let is_terminal = TERMINALS.contains(&method.as_str())
            || scope_named(m, method).is_some_and(|s| self.ends_in_terminal(m, &s.body, 0));
        if !is_terminal {
            return None;
        }
        if e.span.is_synthetic() {
            return Some(Err("synthesized call site".into()));
        }
        Some(self.specialize(e, recv, method, args, cx))
    }

    fn specialize(&self, e: &Expr, recv: &Expr, method: &Symbol, args: &[Expr], cx: &Cx) -> Result<Site, String> {
        let chain = self.chain(recv, cx)?;
        let (chain, terminal) = self.apply(chain, method, args, cx)?;
        if self.default_scoped.iter().any(|m| m == chain.model.0.as_str()) {
            return Err(format!("{} may have a default_scope", chain.model.0.as_str()));
        }
        let mut option = match terminal {
            Terminal::Link => return Err("chain does not end in a terminal".into()),
            Terminal::Records { reverse: false } => String::new(),
            Terminal::Records { reverse: true } => ", reverse: true".to_string(),
            Terminal::One => ", one: true".to_string(),
        };
        // A Rails finder as the terminal: the runtime confirms, before it
        // builds the statement, that the app has not redefined it.
        if TERMINALS.contains(&method.as_str()) {
            option.push_str(&format!(", via: :{}", method.as_str()));
        }
        self.refuse_bypassed_methods(&chain, method)?;
        let key = self.key(e.span);
        let binds = chain.where_binds.clone();
        // `Model.all` roots the block in a Relation: a link such as
        // `ordered_relation` is a Relation method, not a class method.
        let define = format!(
            "Fractal.define(:{key}, {model}{option}) {{ |p| {model}.all{links} }}",
            model = chain.model.0.as_str(),
            links = chain.links.concat(),
        );
        let owner = match &chain.owner {
            Some((owner, name)) => format!(", owner: {owner}, association: :{}", name.as_str()),
            None => String::new(),
        };
        let call = format!("Fractal.records(:{key}, [{}]{owner}) {{ {{orig}} }}", binds.join(", "));
        Ok(Site { span: e.span, call, define })
    }

    fn key(&self, span: Span) -> String {
        let source = &self.app.sources[span.file.0 as usize - 1];
        let (line, col) = source.line_col(span.start);
        let stem: String = source
            .path
            .trim_end_matches(".rb")
            .trim_end_matches(".html.erb")
            .rsplit("app/")
            .next()
            .unwrap_or(&source.path)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        // The file id keeps keys apart where stems coincide
        // (`app/models/user.rb` and `components/x/app/models/user.rb`,
        // `user_x.rb` and `user/x.rb`): a shared key would make one site
        // run the other's statement.
        format!("{stem}_{line}_{col}_f{}", span.file.0)
    }

    /// The chain a relation-valued expression denotes.
    fn chain(&self, e: &Expr, cx: &Cx) -> Result<Chain, String> {
        if cx.depth.get() > 64 {
            return Err("chain nests too deeply (a self-referencing local or recursive scopes)".into());
        }
        cx.depth.set(cx.depth.get() + 1);
        let result = self.chain_at(e, cx);
        cx.depth.set(cx.depth.get() - 1);
        result
    }

    fn chain_at(&self, e: &Expr, cx: &Cx) -> Result<Chain, String> {
        match &*e.node {
            ExprNode::Var { id, name } => match cx.locals.get(&(*id, name.clone())) {
                // The relation was built at the assignment, but the call
                // site evaluates its values at the terminal: `through_local`
                // makes `value` refuse any value that could have changed
                // in between.
                Some(Some(value)) => {
                    let through = Cx {
                        locals: cx.locals,
                        bound: cx.bound,
                        through_local: true,
                        depth: std::cell::Cell::new(cx.depth.get()),
                        args: cx.args.clone(),
                        self_chain: cx.self_chain.clone(),
                    };
                    self.chain(value, &through)
                }
                _ => Err(format!("`{}` is not assigned exactly once", name.as_str())),
            },
            ExprNode::Const { path } => {
                let name = path.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::");
                match self.models.get(name.as_str()) {
                    Some(m) => Ok(Chain::new(m.name.clone(), None)),
                    None => Err(format!("`{name}` is not a model")),
                }
            }
            ExprNode::Send { recv: None, method, args, block: None, .. } => match &cx.self_chain {
                // Inside an inlined scope body, a bare call is a link on
                // the relation the scope was called on.
                Some(chain) => Ok(self.apply(chain.clone(), method, args, cx)?.0),
                None => Err(format!("implicit-self `{}`", method.as_str())),
            },
            ExprNode::Send { recv: Some(recv), method, args, block: None, .. } => {
                if let Some(root) = self.assoc_root(recv, method, args, cx)? {
                    return Ok(root);
                }
                let chain = self.chain(recv, cx)?;
                match self.apply(chain, method, args, cx)? {
                    (chain, Terminal::Link) => Ok(chain),
                    _ => Err(format!("`{}` mid-chain loads records", method.as_str())),
                }
            }
            ExprNode::Send { block: Some(_), method, .. } => Err(format!("`{}` with a block", method.as_str())),
            other => Err(format!("chain rooted at {}", other.kind_str())),
        }
    }

    /// `owner.messages` for a plain `has_many` on a typed owner.
    fn assoc_root(&self, owner: &Expr, method: &Symbol, args: &[Expr], cx: &Cx) -> Result<Option<Chain>, String> {
        let Some(id) = owner.ty.as_ref().and_then(non_nil_class) else { return Ok(None) };
        let Some(m) = self.models.get(id.0.as_str()) else { return Ok(None) };
        let Some(assoc) = m.body.iter().find_map(|item| match item {
            ModelBodyItem::Association { assoc, .. } if assoc.name() == method => Some(assoc),
            _ => None,
        }) else {
            return Ok(None);
        };
        let Association::HasMany { target, foreign_key, through, as_interface, scope, .. } = assoc else {
            return Err(format!("`{}` is not a has_many", method.as_str()));
        };
        if crate::lower::assoc_primary_key::custom_primary_key(assoc).is_some() {
            return Err(format!("has_many `{}` declares `primary_key:`", method.as_str()));
        }
        if through.is_some() || as_interface.is_some() || scope.is_some() || !args.is_empty() {
            return Err(format!("has_many `{}` has options the specializer does not model", method.as_str()));
        }
        if !self.models.contains_key(target.0.as_str()) {
            return Err(format!("`{}` targets a non-model", method.as_str()));
        }
        let Val::Bind(owner) = self.value(owner, cx)? else {
            return Err("static association owner".into());
        };
        let mut chain = Chain::new(target.clone(), Some((owner.clone(), method.clone())));
        chain.links.push(format!(".where({}: p.bind)", foreign_key.as_str()));
        chain.where_binds.push(format!("{owner}.id"));
        Ok(Some(chain))
    }

    /// One link applied to a chain, and whether it was a terminal.
    fn apply(&self, mut chain: Chain, method: &Symbol, args: &[Expr], cx: &Cx) -> Result<(Chain, Terminal), String> {
        let model = self.models[chain.model.0.as_str()];
        if let Some(scope) = scope_named(model, method) {
            if scope.params.is_empty() && args.is_empty() && self.static_scope(model, &scope.body, 0) {
                chain.links.push(format!(".{}", method.as_str()));
                chain.ordered |= mentions(&scope.body, "order");
                chain.limited |= mentions(&scope.body, "limit");
                return Ok((chain, Terminal::Link));
            }
            return self.inline(chain, scope, args, cx);
        }
        if rails_generated_scope(model, method) && args.is_empty() {
            chain.links.push(format!(".{}", method.as_str()));
            return Ok((chain, Terminal::Link));
        }
        match method.as_str() {
            "where" => {
                let [arg] = args else { return Err("where with other than one argument".into()) };
                let ExprNode::Hash { entries, .. } = &*arg.node else {
                    return Err("where with a non-hash condition".into());
                };
                let mut pairs = Vec::new();
                for (k, v) in entries {
                    let ExprNode::Lit { value: Literal::Sym { value: column } } = &*k.node else {
                        return Err("where key is not a symbol".into());
                    };
                    if !self.has_column(&chain.model, column) {
                        return Err(format!("where on `{}`, not a column", column.as_str()));
                    }
                    if matches!(&*v.node, ExprNode::Hash { .. } | ExprNode::Array { .. } | ExprNode::Range { .. }) {
                        return Err("where value is a collection".into());
                    }
                    let value = match self.value(v, cx)? {
                        Val::Static(src) => src,
                        Val::Bind(src) => {
                            chain.where_binds.push(src);
                            "p.bind".into()
                        }
                    };
                    pairs.push(format!("{}: {value}", column.as_str()));
                }
                chain.links.push(format!(".where({})", pairs.join(", ")));
            }
            "order" | "preload" | "includes" => {
                let mut literal = Vec::new();
                for a in args {
                    literal.push(static_literal(a).ok_or_else(|| format!("{} with a non-literal argument", method.as_str()))?);
                }
                chain.links.push(format!(".{}({})", method.as_str(), literal.join(", ")));
                chain.ordered |= method.as_str() == "order";
            }
            "limit" => {
                let [n] = args else { return Err("limit arity".into()) };
                let n = self.limit_value(n, cx)?;
                chain.links.push(format!(".limit({n})"));
                chain.limited = true;
            }
            "all" if args.is_empty() => {}
            // Rails' FinderMethods, reproduced link for link. An unordered
            // chain takes Rails' implicit order (implicit_order_column, then
            // the primary key) from its own private `ordered_relation`, which
            // returns the relation unchanged when it already has an order.
            "first" | "last" => {
                if chain.limited {
                    return Err(format!("{} after limit", method.as_str()));
                }
                chain.links.push(".send(:ordered_relation)".into());
                let last = method.as_str() == "last";
                return match args {
                    // `first` = find_nth(0): ordered_relation.limit(1), first row.
                    // `last` = ordered_relation.limit(nil).reverse_order!.first.
                    [] => {
                        if last {
                            chain.links.push(".reverse_order".into());
                        }
                        chain.links.push(".limit(1)".into());
                        Ok((chain, Terminal::One))
                    }
                    // `first(n)` = ordered_relation.limit(n).to_a;
                    // `last(n)` = ordered_relation.limit(n).reverse_order!, reversed.
                    [n] => {
                        let n = self.limit_value(n, cx)?;
                        chain.links.push(format!(".limit({n})"));
                        if last {
                            chain.links.push(".reverse_order".into());
                        }
                        Ok((chain, Terminal::Records { reverse: last }))
                    }
                    _ => Err(format!("{} arity", method.as_str())),
                };
            }
            // `take` = limit(1).to_a.first, in whatever order the database
            // returns; `find_by(hash)` = where(hash).take.
            "take" if args.is_empty() => {
                if chain.limited {
                    return Err("take after limit".into());
                }
                chain.links.push(".limit(1)".into());
                return Ok((chain, Terminal::One));
            }
            "find_by" => {
                if chain.limited {
                    return Err("find_by after limit".into());
                }
                let (chain, _) = self.apply(chain, &Symbol::from("where"), args, cx)?;
                return self.apply(chain, &Symbol::from("take"), &[], cx);
            }
            "to_a" if args.is_empty() => return Ok((chain, Terminal::Records { reverse: false })),
            other => return Err(format!("`{other}` is not modeled")),
        }
        Ok((chain, Terminal::Link))
    }

    /// A scope's body, evaluated with the chain as implicit self and its
    /// parameters bound to the call's arguments. Statements before the
    /// last only build and discard relations, so they are dropped.
    fn inline(&self, chain: Chain, scope: &Scope, args: &[Expr], cx: &Cx) -> Result<(Chain, Terminal), String> {
        if scope.params.len() != args.len() {
            return Err(format!("scope `{}` arity", scope.name.as_str()));
        }
        let model = self.models[chain.model.0.as_str()];
        let body = match &*scope.body.node {
            ExprNode::Seq { exprs } => {
                let (last, earlier) = exprs.split_last().ok_or("empty scope")?;
                if !earlier.iter().all(|e| self.static_scope(model, e, 0)) {
                    return Err(format!("scope `{}` has statements with effects", scope.name.as_str()));
                }
                last
            }
            _ => &scope.body,
        };
        let mut bound = HashMap::new();
        for (p, a) in scope.params.iter().zip(args) {
            bound.insert(p.name.clone(), self.value(a, cx)?);
        }
        let inner = Cx {
            locals: cx.locals,
            bound: cx.bound,
            through_local: cx.through_local,
            depth: std::cell::Cell::new(cx.depth.get()),
            args: bound,
            self_chain: Some(chain.clone()),
        };
        let ExprNode::Send { recv, method, args, block: None, .. } = &*body.node else {
            return Err(format!("scope `{}` body is not a call chain", scope.name.as_str()));
        };
        let base = match recv {
            Some(r) => self.chain(r, &inner)?,
            None => chain,
        };
        self.apply(base, method, args, &inner)
    }

    /// Does this scope body only build a relation from literals, so that
    /// Rails can evaluate it once at boot?
    fn static_scope(&self, model: &Model, e: &Expr, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        match &*e.node {
            ExprNode::Seq { exprs } => exprs.iter().all(|x| self.static_scope(model, x, depth + 1)),
            ExprNode::Send { recv, method, args, block: None, .. } => {
                let recv_ok = recv.as_ref().is_none_or(|r| self.static_scope(model, r, depth + 1));
                let link_ok = if let Some(s) = scope_named(model, method) {
                    args.is_empty() && s.params.is_empty() && self.static_scope(model, &s.body, depth + 1)
                } else if rails_generated_scope(model, method) {
                    args.is_empty()
                } else {
                    STATIC_BUILDERS.contains(&method.as_str()) && args.iter().all(|a| static_literal(a).is_some())
                };
                recv_ok && link_ok
            }
            _ => false,
        }
    }

    /// `Owner#association` for every association whose scope lambda only
    /// builds a relation from literals. The runtime preloader builds such a
    /// scope once; any other scoped association it leaves to Rails, since a
    /// scope like `-> { where(user: Current.user) }` must be evaluated per
    /// request.
    pub fn static_association_scopes(&self) -> Vec<String> {
        let mut out = Vec::new();
        for m in self.app.models.iter() {
            for item in &m.body {
                let ModelBodyItem::Association { assoc, .. } = item else { continue };
                let (target, scope) = match assoc {
                    Association::HasMany { target, scope: Some(scope), .. }
                    | Association::HasOne { target, scope: Some(scope), .. } => (target, scope),
                    _ => continue,
                };
                if let Some(t) = self.models.get(target.0.as_str())
                    && self.static_scope(t, scope, 0)
                {
                    out.push(format!("{}#{}", m.name.0.as_str(), assoc.name().as_str()));
                }
            }
        }
        out.sort();
        out
    }

    fn any_scope_named(&self, name: &Symbol) -> bool {
        self.models.values().any(|m| scope_named(m, name).is_some())
    }

    fn ends_in_terminal(&self, model: &Model, body: &Expr, depth: usize) -> bool {
        let last = match &*body.node {
            ExprNode::Seq { exprs } => match exprs.last() {
                Some(l) => l,
                None => return false,
            },
            _ => body,
        };
        match &*last.node {
            ExprNode::Send { method, .. } if TERMINALS.contains(&method.as_str()) => true,
            ExprNode::Send { method, .. } if depth < 8 => {
                scope_named(model, method).is_some_and(|s| self.ends_in_terminal(model, &s.body, depth + 1))
            }
            _ => false,
        }
    }

    /// The value of a `where` operand.
    fn value(&self, e: &Expr, cx: &Cx) -> Result<Val, String> {
        if let Some(src) = static_literal(e) {
            return Ok(Val::Static(src));
        }
        match &*e.node {
            ExprNode::Var { name, .. } if cx.args.contains_key(name) => Ok(cx.args[name].clone()),
            // Inside a scope body, anything else is evaluated against the
            // model class, which the call site cannot reproduce.
            _ if cx.self_chain.is_some() => Err("scope body value other than a literal or parameter".into()),
            _ if is_call_site_value(e) && !is_plain_read(e) => Err(format!(
                "`{}` may not give the same value twice; the fallback evaluates it again",
                emit_expr(e)
            )),
            _ if cx.through_local && reads_rebound(e, cx.bound) => Err(format!(
                "`{}` is read at the terminal but the relation was built earlier, and the method assigns or rebinds it",
                emit_expr(e)
            )),
            _ if is_call_site_value(e) => Ok(Val::Bind(emit_expr(e))),
            other => Err(format!("value is {}", other.kind_str())),
        }
    }

    /// A `limit`/`first`/`last` count: a literal or known integer
    /// constant. A request-time count declines.
    fn limit_value(&self, e: &Expr, cx: &Cx) -> Result<String, String> {
        let v = match &*e.node {
            ExprNode::Const { path } => self.int_const(path)?,
            _ => self.value(e, cx)?,
        };
        Ok(match v {
            Val::Static(src) => src,
            // Active Record 8.1 casts a LIMIT with `Integer()` as it is
            // set, so a placeholder raises while the statement is built.
            Val::Bind(_) => {
                return Err("a request-time LIMIT cannot be a placeholder on Active Record 8.1".into());
            }
        })
    }

    /// An integer constant: its value when the declaring class records
    /// one, else a qualified path (ingest resolves references, so a
    /// multi-segment path names the constant from anywhere).
    fn int_const(&self, path: &[Symbol]) -> Result<Val, String> {
        let (name, owner) = path.split_last().ok_or("empty constant path")?;
        let owner = owner.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("::");
        let declared = self
            .app
            .library_classes
            .iter()
            .filter(|lc| owner.is_empty() || lc.name.0.as_str() == owner)
            .flat_map(|lc| lc.constants.iter())
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| match &*v.node {
                ExprNode::Lit { value: Literal::Int { value } } => Some(*value),
                _ => None,
            });
        match (declared, owner.is_empty()) {
            (Some(n), _) => Ok(Val::Static(n.to_string())),
            (None, true) => match self.int_consts.get(name.as_str()) {
                Some(Some(n)) => Ok(Val::Static(n.to_string())),
                _ => Err(format!("constant `{}` is not a known integer", name.as_str())),
            },
            (None, false) => Ok(Val::Static(format!("{owner}::{}", name.as_str()))),
        }
    }

    /// The app defines nothing the rewrite would step around.
    fn refuse_bypassed_methods(&self, chain: &Chain, terminal: &Symbol) -> Result<(), String> {
        let model = self.models[chain.model.0.as_str()];
        let defines = |m: &Model, name: &Symbol, receiver: MethodReceiver| {
            m.body.iter().any(|item| {
                matches!(item, ModelBodyItem::Method { method, .. } if &method.name == name && method.receiver == receiver)
            })
        };
        if TERMINALS.contains(&terminal.as_str()) && defines(model, terminal, MethodReceiver::Class) {
            return Err(format!(
                "{}.{} is defined by the app; the statement would bypass it",
                chain.model.0.as_str(),
                terminal.as_str()
            ));
        }
        if let Some((_, association)) = &chain.owner {
            let owners = self.models.values().filter(|m| {
                m.body.iter().any(|item| matches!(item, ModelBodyItem::Association { assoc, .. } if assoc.name() == association))
            });
            for owner in owners {
                if defines(owner, association, MethodReceiver::Instance) {
                    return Err(format!(
                        "{}#{} is redefined by the app; the statement would bypass it",
                        owner.name.0.as_str(),
                        association.as_str()
                    ));
                }
            }
        }
        Ok(())
    }

    fn has_column(&self, model: &ClassId, column: &Symbol) -> bool {
        let m = self.models[model.0.as_str()];
        self.app
            .schema
            .tables
            .get(&m.table.0)
            .is_some_and(|t| t.columns.iter().any(|c| &c.name == column))
    }
}

struct Cx<'b> {
    locals: &'b HashMap<(VarId, Symbol), Option<&'b Expr>>,
    /// Every local (`x`) and instance variable (`@x`) the method assigns
    /// or binds anywhere: assignments, block and lambda parameters, rescue
    /// and pattern bindings.
    bound: &'b HashSet<String>,
    /// Set while walking a relation reached through a local
    /// (`rel = Model.where(a: @a)` ... `rel.first`): its values are written
    /// at the terminal, so a value the method could change, or a block could
    /// shadow, in between is refused.
    through_local: bool,
    /// Chain-walk nesting (locals, links, inlined scopes), bounded so a
    /// self-referencing local (`stories = stories.where(...)`) or mutually
    /// recursive scopes end in a residue rather than a stack overflow.
    depth: std::cell::Cell<usize>,
    /// Scope parameters bound to the call's argument values.
    args: HashMap<Symbol, Val>,
    /// The relation an inlined scope body runs against.
    self_chain: Option<Chain>,
}

impl Chain {
    fn new(model: ClassId, owner: Option<(String, Symbol)>) -> Self {
        Chain {
            model,
            owner,
            links: Vec::new(),
            where_binds: Vec::new(),
            ordered: false,
            limited: false,
        }
    }
}

/// The model a chain receiver yields. Analysis types association readers
/// and scope chains as `Array[Model]` as well as `Relation[Model]`; the
/// chain walker, not the type, decides whether it is really a query.
fn relation_model(e: &Expr) -> Option<&ClassId> {
    match &e.ty {
        Some(Ty::Relation { of }) => Some(of),
        Some(Ty::Array { elem }) => match &**elem {
            Ty::Class { id, .. } => Some(id),
            _ => None,
        },
        Some(Ty::Class { id, .. }) if matches!(&*e.node, ExprNode::Const { .. }) => Some(id),
        _ => None,
    }
}

/// `Room` from `Room` or `Room | nil` (a nil owner raises at the call site
/// exactly as the original `owner.messages` would).
fn non_nil_class(ty: &Ty) -> Option<&ClassId> {
    match ty {
        Ty::Class { id, .. } => Some(id),
        Ty::Union { variants } => {
            let mut classes = variants.iter().filter(|v| !matches!(v, Ty::Nil));
            match (classes.next(), classes.next()) {
                (Some(Ty::Class { id, .. }), None) => Some(id),
                _ => None,
            }
        }
        _ => None,
    }
}

fn scope_named<'m>(m: &'m Model, name: &Symbol) -> Option<&'m Scope> {
    m.body.iter().find_map(|item| match item {
        ModelBodyItem::Scope { scope, .. } if &scope.name == name => Some(scope),
        _ => None,
    })
}

/// The eager-loading scopes Active Storage and Action Text generate
/// (`with_attached_avatar`, `with_rich_text_body_and_embeds`). Their trees
/// depend on boot-time configuration, which is why the block calls them
/// rather than this module expanding them. Refused when the model defines
/// a method of the same name itself.
fn rails_generated_scope(m: &Model, name: &Symbol) -> bool {
    let n = name.as_str();
    (n.starts_with("with_attached_") || n.starts_with("with_rich_text_"))
        && !m.body.iter().any(|item| matches!(item, ModelBodyItem::Method { method, .. } if &method.name == name))
}

fn mentions(e: &Expr, method: &str) -> bool {
    fn walk(e: &Expr, method: &str, found: &mut bool) {
        if let ExprNode::Send { method: m, .. } = &*e.node
            && m.as_str() == method
        {
            *found = true;
        }
        e.node.for_each_child(&mut |c| walk(c, method, found));
    }
    let mut found = false;
    walk(e, method, &mut found);
    found
}

/// Literal Ruby fixed at build time: symbols, strings, numbers, and
/// hashes/arrays of them (order and preload arguments, where values).
fn static_literal(e: &Expr) -> Option<String> {
    fn lit(e: &Expr) -> bool {
        match &*e.node {
            ExprNode::Lit { value } => !matches!(value, Literal::Regex { .. } | Literal::Float { .. }),
            ExprNode::Hash { entries, .. } => entries.iter().all(|(k, v)| lit(k) && lit(v)),
            ExprNode::Array { elements, .. } => elements.iter().all(lit),
            _ => false,
        }
    }
    lit(e).then(|| {
        let src = emit_expr(e);
        // A bare kwargs hash (`order(created_at: :desc)`) needs braces
        // once it stands alone as an argument.
        match &*e.node {
            ExprNode::Hash { .. } if !src.starts_with('{') => format!("{{ {src} }}"),
            _ => src,
        }
    })
}

/// A request-time value the call site can evaluate again. The call site
/// is the expression the chain was written in, so the same names mean the
/// same things there: instance and local variables, `self`, argument-less
/// helpers (`params`, `session`, `current_user`), and reads on those with
/// literal or likewise arguments (`params[:id].to_s`, `@user.id`, `m[1]`),
/// combined with `||`/`&&`. Blocks and anything else decline.
fn is_call_site_value(e: &Expr) -> bool {
    match &*e.node {
        ExprNode::Ivar { .. } | ExprNode::Var { .. } | ExprNode::SelfRef => true,
        ExprNode::Lit { value } => !matches!(value, Literal::Regex { .. }),
        ExprNode::Send { recv: None, args, block: None, .. } => args.is_empty(),
        ExprNode::Send { recv: Some(r), args, block: None, .. } => {
            is_call_site_value(r) && args.iter().all(is_call_site_value)
        }
        ExprNode::BoolOp { left, right, .. } => is_call_site_value(left) && is_call_site_value(right),
        _ => false,
    }
}

/// A value that reads the same however often it is evaluated: a local,
/// an instance variable, `self`, a literal, or `.id` on one of those (the
/// owner of an association-rooted chain). The only request-time values
/// admitted.
fn is_plain_read(e: &Expr) -> bool {
    match &*e.node {
        ExprNode::Ivar { .. } | ExprNode::Var { .. } | ExprNode::SelfRef => true,
        ExprNode::Lit { value } => !matches!(value, Literal::Regex { .. }),
        ExprNode::Send { recv: Some(r), method, args, block: None, .. } => {
            method.as_str() == "id" && args.is_empty() && is_plain_read(r)
        }
        _ => false,
    }
}

/// Does `e` read a local or instance variable that the method assigns
/// or rebinds somewhere?
fn reads_rebound(e: &Expr, bound: &HashSet<String>) -> bool {
    let mut hit = false;
    fn walk(e: &Expr, bound: &HashSet<String>, hit: &mut bool) {
        match &*e.node {
            ExprNode::Var { name, .. } if bound.contains(name.as_str()) => *hit = true,
            ExprNode::Ivar { name } if bound.contains(&format!("@{}", name.as_str().trim_start_matches('@'))) => *hit = true,
            _ => {}
        }
        e.node.for_each_child(&mut |c| walk(c, bound, hit));
    }
    walk(e, bound, &mut hit);
    hit
}

/// Every name `collect_bound` records for one assignment target.
fn lvalue_name(target: &LValue, out: &mut HashSet<String>) {
    match target {
        LValue::Var { name, .. } => {
            out.insert(name.as_str().to_string());
        }
        LValue::Ivar { name } => {
            out.insert(format!("@{}", name.as_str().trim_start_matches('@')));
        }
        _ => {}
    }
}

fn pattern_names(p: &crate::expr::Pattern, out: &mut HashSet<String>) {
    use crate::expr::Pattern;
    match p {
        Pattern::Bind { name } => {
            out.insert(name.as_str().to_string());
        }
        Pattern::Array { elems, rest } => {
            elems.iter().for_each(|e| pattern_names(e, out));
            if let Some(rest) = rest {
                out.insert(rest.as_str().to_string());
            }
        }
        Pattern::Record { fields, .. } => fields.iter().for_each(|(_, p)| pattern_names(p, out)),
        _ => {}
    }
}

/// Locals and instance variables the body assigns or binds anywhere
/// (see `Cx::bound`).
fn collect_bound(e: &Expr, out: &mut HashSet<String>) {
    match &*e.node {
        ExprNode::Assign { target, .. } | ExprNode::OpAssign { target, .. } => lvalue_name(target, out),
        ExprNode::MultiAssign { targets, .. } => targets.iter().for_each(|t| lvalue_name(t, out)),
        ExprNode::Let { name, .. } => {
            out.insert(name.as_str().to_string());
        }
        ExprNode::Lambda { params, rest_param, extra_params, block_param, .. } => {
            let names = params.iter().chain(rest_param).chain(block_param).chain(extra_params.iter().map(|p| &p.name));
            out.extend(names.map(|n| n.as_str().to_string()));
        }
        ExprNode::BeginRescue { rescues, .. } => {
            out.extend(rescues.iter().filter_map(|r| r.binding.as_ref()).map(|n| n.as_str().to_string()));
        }
        ExprNode::Case { arms, .. } => arms.iter().for_each(|a| pattern_names(&a.pattern, out)),
        ExprNode::CaseMatch { arms, .. } => {
            for arm in arms {
                let mut names = Vec::new();
                arm.pattern.bound_names(&mut names);
                out.extend(names.iter().map(|n| n.as_str().to_string()));
            }
        }
        ExprNode::MatchPredicate { pattern, .. } | ExprNode::MatchRequired { pattern, .. } => {
            let mut names = Vec::new();
            pattern.bound_names(&mut names);
            out.extend(names.iter().map(|n| n.as_str().to_string()));
        }
        _ => {}
    }
    e.node.for_each_child(&mut |c| collect_bound(c, out));
}

/// Locals assigned exactly once in a body (`None` when assigned more).
/// Keyed with the name as well: analysis may reuse an id across locals.
fn collect_locals<'e>(e: &'e Expr, out: &mut HashMap<(VarId, Symbol), Option<&'e Expr>>) {
    if let ExprNode::Assign { target: LValue::Var { id, name }, value } = &*e.node {
        out.entry((*id, name.clone())).and_modify(|v| *v = None).or_insert(Some(value));
    }
    e.node.for_each_child(&mut |c| collect_locals(c, out));
}
