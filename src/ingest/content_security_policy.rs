//! Controller `content_security_policy` macro lowering and admission.
//!
//! Rails' `content_security_policy(enabled = true, **options, &block)`
//! registers a `before_action` that rebuilds the response policy per
//! request from a copy of the current one:
//!
//! ```ruby
//! before_action(options) do
//!   if block_given?
//!     policy = current_content_security_policy
//!     instance_exec(policy, &block)
//!     request.content_security_policy = policy
//!   end
//!   unless enabled
//!     request.content_security_policy = nil
//!   end
//! end
//! ```
//!
//! This pass rewrites each accepted declaration into that same
//! `before_action` block - an `Unknown` dispatcher Send that
//! [`crate::ingest::controller::lambda_filter_target`] recognizes - so
//! it joins the controller filter chain through `build_filter_preamble`
//! and inherits declaration order, inheritance, once-only execution, and
//! `return if performed?` halting without a second mechanism. It runs
//! during ingest, before the source drain and before the concern splice
//! and class-body macro expansion, so source consumers that analyze
//! without the post-analyze lowering still see the generated callback.
//!
//! The `only:` / `except:` / `if:` / `unless:` guard is normalized into
//! one explicit `if:` condition in Rails' own evaluation order (only,
//! if, except, unless), rather than handed to the shared
//! `cond_from_guards`: its `only`-wins `include_check` drops an
//! accompanying `except:` and treats `only: []` as every action, both
//! wrong for a scoped override. The condition reads `action_name` (the
//! String reader), which also flips the dispatcher's `reads_action_name`
//! so the name resolves to the reader, not the Symbol dispatch local.
//!
//! Every accepted callback inlines into one dispatcher method, so each
//! declaration's block and lambda-guard locals are renamed with a
//! per-scope suffix. Two controllers that both write `value = ...` keep
//! separate dispatcher locals, and the generated `policy` local is fresh
//! against every callback local too.
//!
//! Only `frame_src` is admitted. Every other directive, a non-literal
//! `enabled` argument, an unknown option or condition shape, a proc
//! source, a rebound or shadowed policy parameter, non-local control
//! flow, pattern matching, `begin`/`rescue`, loops, and any declaration
//! carried by a concern `included do` block or a custom macro method are
//! refused with a located error that survey mode keeps as a gap.

use super::{sources, survey, IngestError, IngestResult};
use crate::expr::{
    ArrayStyle, BlockStyle, BoolOpKind, BoolOpSurface, Expr, ExprNode, LValue, Literal,
};
use crate::ident::{Symbol, VarId};
use crate::span::Span;
use crate::{App, ControllerBodyItem};

/// The controller class-body macro this pass claims.
const MACRO: &str = "content_security_policy";

/// The one admitted policy directive.
const DIRECTIVE: &str = "frame_src";

/// Rewrite every accepted controller `content_security_policy`
/// declaration into a `before_action` block and record its source span,
/// then refuse every concern/custom-macro carrier of the macro. A
/// refusal records a located gap (survey mode) or aborts ingest (strict
/// mode); it never half-mutates a declaration.
pub(super) fn synthesize(app: &mut App, source_files: &[crate::span::SourceFile]) -> IngestResult<()> {
    let mut spans: Vec<Span> = Vec::new();
    let mut prefix = "__csp".to_string();
    while source_files.iter().any(|source| source.text.contains(&prefix)) {
        prefix.push('_');
    }
    let mut scopes = LocalScopes { prefix, next: 0 };
    let captures = captured_declarations(source_files);
    for controller in &mut app.controllers {
        for item in &mut controller.body {
            let ControllerBodyItem::Unknown { expr, .. } = item else {
                continue;
            };
            if macro_name(expr) == Some("content_security_policy_report_only") {
                refuse_unit(expr.span, "content_security_policy_report_only declarations are not supported")?;
                continue;
            }
            if macro_name(expr) != Some(MACRO) {
                continue;
            }
            if captures.iter().any(|capture| capture.file == expr.span.file
                && capture.start >= expr.span.start && capture.end <= expr.span.end)
            {
                refuse_unit(expr.span, "content_security_policy callbacks do not support captured class-body locals, block-local declarations, or numbered parameters")?;
                continue;
            }
            // Build the replacement fully before touching `expr`, so a
            // refusal leaves the original declaration untouched.
            if let Some(before_action) = lower(expr, &mut scopes, false)? {
                spans.push(expr.span);
                *expr = before_action;
            }
        }
    }
    let mut claimed = Vec::new();
    for filters in app.concern_filters.values_mut() {
        for filter in filters {
            let Some(expr) = &mut filter.block else { continue };
            if macro_name(expr) != Some(MACRO) { continue; }
            if captures.iter().any(|capture| capture.file == expr.span.file
                && capture.start >= expr.span.start && capture.end <= expr.span.end)
            {
                refuse_unit(expr.span, "content_security_policy mutation uses unsupported local bindings")?;
                continue;
            }
            if let Some(callback) = lower(expr, &mut scopes, true)? {
                claimed.push(expr.span);
                *expr = callback;
            }
        }
    }
    app.content_security_policy_spans = spans;
    reject_indirect(app, &claimed)?;
    Ok(())
}

/// Source spans of the accepted controller `content_security_policy`
/// declarations - block, `true`, and `false` forms alike. Read by
/// `project::reject_content_security_policy` to gate every target but
/// Ruby, which alone has verified the request-policy storage and
/// final-response header contract.
pub(crate) fn declaration_spans(app: &App) -> &[Span] {
    &app.content_security_policy_spans
}

/// A declaration in the controller class context.
fn macro_name(expr: &Expr) -> Option<&str> {
    match &*expr.node {
        ExprNode::Send { recv, method, .. }
            if recv.as_ref().is_none_or(|receiver| matches!(&*receiver.node, ExprNode::SelfRef)) =>
        {
            Some(method.as_str())
        }
        _ => None,
    }
}

/// Main's concern profile edits raw directives or worker-src rather than
/// declaring a new controller frame-src policy.
pub(super) fn is_core_mutation(expr: &Expr) -> bool {
    let ExprNode::Send { recv: None, method, block: Some(block), .. } = &*expr.node else { return false };
    if method.as_str() != MACRO { return false; }
    let ExprNode::Lambda { params, rest_param, extra_params, block_param, body, .. } = &*block.node else { return false };
    if params.len() != 1 || rest_param.is_some() || !extra_params.is_empty() || block_param.is_some() {
        return false;
    }
    fn uses_core(expr: &Expr, policy: &Symbol) -> bool {
        if matches!(&*expr.node, ExprNode::Send { recv: Some(recv), method, .. }
            if is_policy(recv, policy) && matches!(method.as_str(), "directives" | "worker_src"))
        { return true; }
        let mut found = false;
        expr.node.for_each_child(&mut |child| found |= uses_core(child, policy));
        found
    }
    uses_core(body, &params[0]) && block_violation(body, Some(&params[0]), true).is_none()
}

/// `Some(before_action)` for an accepted declaration; `None` when the
/// declaration was refused in survey mode (a gap was recorded). In
/// strict mode a refusal returns `Err`.
fn lower(expr: &Expr, scopes: &mut LocalScopes, mutation: bool) -> IngestResult<Option<Expr>> {
    let span = expr.span;
    let ExprNode::Send { args, block, .. } = &*expr.node else {
        return Ok(None);
    };

    // `**options` cannot be validated statically.
    if args.iter().any(|a| {
        matches!(
            &*a.node,
            ExprNode::KeywordSplat { .. }
                | ExprNode::ForwardKeywords
                | ExprNode::ForwardKeywordsWithPairs { .. }
        )
    }) {
        return refuse(
            span,
            "content_security_policy with dynamic `**` options is not supported",
        );
    }

    // Split the trailing options hash from the positional `enabled`.
    let (positional, options): (&[Expr], &[(Expr, Expr)]) = match args.split_last() {
        Some((last, rest)) => match &*last.node {
            ExprNode::Hash { entries, .. } => (rest, entries.as_slice()),
            _ => (args.as_slice(), &[]),
        },
        None => (&[], &[]),
    };

    let enabled = match positional {
        [] => true,
        [one] => match &*one.node {
            ExprNode::Lit { value: Literal::Bool { value } } => *value,
            _ => {
                return refuse(
                    span,
                    "content_security_policy enabled argument must be a literal `true` or `false`",
                )
            }
        },
        _ => {
            return refuse(
                span,
                "content_security_policy takes at most one positional (enabled) argument",
            )
        }
    };

    let mut only: Option<Vec<String>> = None;
    let mut except: Option<Vec<String>> = None;
    let mut if_cond: Option<Expr> = None;
    let mut unless_cond: Option<Expr> = None;
    for (key, value) in options {
        let ExprNode::Lit { value: Literal::Sym { value: key } } = &*key.node else {
            return refuse(span, "content_security_policy options must use symbol keys");
        };
        match key.as_str() {
            "only" => match action_list(value) {
                Some(list) => only = Some(list),
                None => {
                    return refuse(
                        span,
                        "content_security_policy `only:` must be a symbol, string, or array of them",
                    )
                }
            },
            "except" => match action_list(value) {
                Some(list) => except = Some(list),
                None => {
                    return refuse(
                        span,
                        "content_security_policy `except:` must be a symbol, string, or array of them",
                    )
                }
            },
            "if" => match condition(value, scopes) {
                Some(cond) => if_cond = Some(cond),
                None => {
                    return refuse(
                        span,
                        "content_security_policy `if:` must be a symbol or a parameterless lambda",
                    )
                }
            },
            "unless" => match condition(value, scopes) {
                Some(cond) => unless_cond = Some(cond),
                None => {
                    return refuse(
                        span,
                        "content_security_policy `unless:` must be a symbol or a parameterless lambda",
                    )
                }
            },
            other => {
                return refuse(
                    span,
                    format!("content_security_policy option `{other}:` is not supported"),
                )
            }
        }
    }

    let Some(body) = filter_body(block, enabled, span, scopes, mutation)? else {
        return Ok(None);
    };

    let mut before_action_args: Vec<Expr> = Vec::new();
    if let Some(guard) = guard(&only, &except, &if_cond, &unless_cond) {
        before_action_args.push(syn(ExprNode::Hash {
            entries: vec![(
                syn(ExprNode::Lit { value: Literal::Sym { value: Symbol::from("if") } }),
                guard_lambda(guard),
            )],
            kwargs: true,
        }));
    }

    let parenthesized = !before_action_args.is_empty();
    let mut callback = Expr::new(
        span,
        ExprNode::Send {
            recv: None,
            method: Symbol::from("before_action"),
            args: before_action_args,
            block: Some(block_lambda(body)),
            parenthesized,
        },
    );
    callback.inherit_span(span);
    Ok(Some(callback))
}

/// The `before_action` block body: `policy = current_content_security_policy`,
/// the renamed block statements, `request.content_security_policy = policy`,
/// and - when disabled - the trailing `request.content_security_policy = nil`
/// that Rails writes after the block runs. `None` when the block was refused
/// in survey mode.
fn filter_body(
    block: &Option<Expr>,
    enabled: bool,
    span: Span,
    scopes: &mut LocalScopes,
    mutation: bool,
) -> IngestResult<Option<Expr>> {
    let mut stmts: Vec<Expr> = Vec::new();
    if let Some(block) = block {
        let Some((policy, mut body)) = validate_block(block, span, mutation)? else {
            return Ok(None);
        };
        let suffix = scopes.suffix();
        rename_locals(&mut body, &suffix);
        let policy = Symbol::from(format!("{}{suffix}", policy.as_str()).as_str());
        stmts.push(assign_var(
            policy.clone(),
            self_send(Symbol::from("current_content_security_policy")),
        ));
        push_body(&mut stmts, body);
        stmts.push(request_policy_assign(var(policy)));
    }
    if !enabled {
        stmts.push(request_policy_assign(syn(ExprNode::Lit { value: Literal::Nil })));
    }
    Ok(Some(syn(ExprNode::Seq { exprs: stmts })))
}

/// `Some((policy_param, body))` for a well-formed single-parameter block;
/// a refusal otherwise. Validates the body before cloning it.
fn validate_block(block: &Expr, span: Span, mutation: bool) -> IngestResult<Option<(Symbol, Expr)>> {
    let ExprNode::Lambda { params, rest_param, extra_params, block_param, body, .. } = &*block.node
    else {
        return refuse(span, "content_security_policy block is malformed");
    };
    if params.len() > 1
        || rest_param.is_some()
        || !extra_params.is_empty()
        || block_param.is_some()
    {
        return refuse(
            span,
            "content_security_policy block takes a single policy parameter",
        );
    }
    let policy = params.first().cloned().unwrap_or_else(|| Symbol::from("policy"));
    if let Some(reason) = block_violation(body, Some(&policy), mutation) {
        return refuse(span, reason);
    }
    Ok(Some((policy, body.clone())))
}

/// The reason a block body is unsupported, or `None` when it is a flat
/// sequence of `policy.frame_src(...)` calls and isolable local work.
fn block_violation(body: &Expr, policy: Option<&Symbol>, mutation: bool) -> Option<String> {
    let mut reason: Option<String> = None;
    check(body, policy, 0, mutation, &mut reason);
    reason
}

fn check(e: &Expr, policy: Option<&Symbol>, depth: u32, mutation: bool, reason: &mut Option<String>) {
    if reason.is_some() {
        return;
    }
    match &*e.node {
        // The one supported policy use: `policy.frame_src(args)`.
        ExprNode::Send { recv: Some(recv), method, args, block, .. }
            if policy.is_some_and(|policy| is_policy(recv, policy)) =>
        {
            let allowed = if mutation {
                (method.as_str() == "directives" && args.is_empty())
                    || (method.as_str() == "worker_src" && args.len() == 1)
            } else {
                method.as_str() == DIRECTIVE
            };
            if !allowed {
                *reason = Some(format!(
                    "content_security_policy supports only the `{DIRECTIVE}` directive, not `{}`",
                    method.as_str()
                ));
                return;
            }
            if block.is_some() {
                *reason =
                    Some(format!("content_security_policy `{DIRECTIVE}` does not accept a block"));
                return;
            }
            for arg in args {
                if is_proc(arg) {
                    *reason = Some(format!(
                        "content_security_policy `{DIRECTIVE}` does not accept directive-level Proc sources"
                    ));
                    return;
                }
                check(arg, policy, depth, mutation, reason);
                if reason.is_some() {
                    return;
                }
            }
            return;
        }
        // Any other mention of the policy parameter is unsupported.
        ExprNode::Var { name, .. } if policy == Some(name) => {
            *reason = Some(format!(
                "content_security_policy block may only call `{DIRECTIVE}` on the `{}` parameter",
                name.as_str()
            ));
            return;
        }
        ExprNode::Assign { target: LValue::Var { name, .. }, .. }
        | ExprNode::OpAssign { target: LValue::Var { name, .. }, .. }
        | ExprNode::Let { name, .. }
            if policy == Some(name) =>
        {
            *reason =
                Some("content_security_policy block must not rebind the policy parameter".into());
            return;
        }
        ExprNode::MultiAssign { targets, .. }
            if policy.is_some_and(|policy| targets.iter().any(|target|
                matches!(target, LValue::Var { name, .. } if name == policy))) =>
        {
            *reason = Some("content_security_policy block must not rebind the policy parameter".into());
            return;
        }
        ExprNode::Lambda { params, rest_param, extra_params, block_param, .. }
            if policy.is_some_and(|policy| params.contains(policy)
                || rest_param.as_ref() == Some(policy)
                || block_param.as_ref() == Some(policy)
                || extra_params.iter().any(|param| &param.name == policy)) =>
        {
            *reason =
                Some("content_security_policy block must not shadow the policy parameter".into());
            return;
        }
        // Pattern matching, begin/rescue, and loops introduce bindings or
        // control flow the single-dispatcher inlining cannot isolate.
        ExprNode::Case { .. }
        | ExprNode::CaseMatch { .. }
        | ExprNode::MatchPredicate { .. }
        | ExprNode::MatchRequired { .. }
        | ExprNode::BeginRescue { .. }
        | ExprNode::While { .. }
        | ExprNode::Yield { .. }
        | ExprNode::Super { .. } => {
            *reason = Some(format!(
                "content_security_policy block does not support `{}`",
                e.node.kind_str()
            ));
            return;
        }
        // Non-local control flow cannot be inlined into the dispatcher.
        ExprNode::Return { .. } | ExprNode::Retry => {
            *reason =
                Some("content_security_policy block must not use non-local control flow".into());
            return;
        }
        ExprNode::Next { .. } | ExprNode::Break { .. } | ExprNode::Redo if depth == 0 => {
            *reason =
                Some("content_security_policy block must not use non-local control flow".into());
            return;
        }
        _ => {}
    }
    let depth = match &*e.node {
        ExprNode::Lambda { .. } => depth + 1,
        _ => depth,
    };
    e.node.for_each_child(&mut |child| check(child, policy, depth, mutation, reason));
}

fn is_policy(e: &Expr, policy: &Symbol) -> bool {
    matches!(&*e.node, ExprNode::Var { name, .. } if name == policy)
}

/// A proc/lambda source: `-> {}`, `proc {}`, `lambda {}`, or `Proc.new {}`.
fn is_proc(e: &Expr) -> bool {
    match &*e.node {
        ExprNode::Lambda { .. } => true,
        ExprNode::Splat { value } => is_proc(value),
        ExprNode::Array { elements, .. } => elements.iter().any(is_proc),
        ExprNode::Send { recv, method, block: Some(_), .. } => match method.as_str() {
            "proc" | "lambda" => recv.is_none(),
            "new" => matches!(
                recv.as_ref().map(|r| &*r.node),
                Some(ExprNode::Const { path }) if path.last().map(|s| s.as_str()) == Some("Proc")
            ),
            _ => false,
        },
        _ => false,
    }
}

/// A symbol predicate (`if: :ready?`) becomes a self-send; a parameterless
/// lambda (`if: -> { ... }`) contributes its renamed body expression.
fn condition(value: &Expr, scopes: &mut LocalScopes) -> Option<Expr> {
    match &*value.node {
        ExprNode::Lit { value: Literal::Sym { value } } => Some(self_send(value.clone())),
        ExprNode::Lambda { params, rest_param, extra_params, block_param, body, .. }
            if params.is_empty()
                && rest_param.is_none()
                && extra_params.is_empty()
                && block_param.is_none()
                && block_violation(body, None, false).is_none() =>
        {
            let mut body = body.clone();
            rename_locals(&mut body, &scopes.suffix());
            Some(body)
        }
        _ => None,
    }
}

/// A symbol, string, or array of them, as action-name strings; `None` for
/// any other shape.
fn action_list(value: &Expr) -> Option<Vec<String>> {
    match &*value.node {
        ExprNode::Array { elements, .. } => elements.iter().map(|e| action_atom(e)).collect(),
        _ => Some(vec![action_atom(value)?]),
    }
}

fn action_atom(value: &Expr) -> Option<String> {
    match &*value.node {
        ExprNode::Lit { value: Literal::Sym { value } } => Some(value.as_str().to_string()),
        ExprNode::Lit { value: Literal::Str { value } } => Some(value.clone()),
        _ => None,
    }
}

/// The folded `if:` condition, in Rails' evaluation order: `only`, then
/// `if`, then negated `except`, then negated `unless`, conjoined with `&&`
/// so the short-circuit matches `all?`/`none?`. `None` when unconditional.
fn guard(
    only: &Option<Vec<String>>,
    except: &Option<Vec<String>>,
    if_cond: &Option<Expr>,
    unless_cond: &Option<Expr>,
) -> Option<Expr> {
    let mut terms: Vec<Expr> = Vec::new();
    if let Some(list) = only {
        terms.push(action_in(list));
    }
    if let Some(cond) = if_cond {
        terms.push(cond.clone());
    }
    if let Some(list) = except {
        terms.push(negate(action_in(list)));
    }
    if let Some(cond) = unless_cond {
        terms.push(negate(cond.clone()));
    }
    terms.into_iter().reduce(|left, right| {
        syn(ExprNode::BoolOp {
            op: BoolOpKind::And,
            surface: BoolOpSurface::Symbol,
            left,
            right,
        })
    })
}

/// `["a", "b"].include?(action_name)` - empty list yields the
/// always-false `[].include?(action_name)`, so `only: []` runs nothing.
fn action_in(list: &[String]) -> Expr {
    let elements = list
        .iter()
        .map(|name| syn(ExprNode::Lit { value: Literal::Str { value: name.clone() } }))
        .collect();
    syn(ExprNode::Send {
        recv: Some(syn(ExprNode::Array { elements, style: ArrayStyle::Brackets })),
        method: Symbol::from("include?"),
        args: vec![self_send(Symbol::from("action_name"))],
        block: None,
        parenthesized: true,
    })
}

/// Refuse the macro inside a concern `included do` block or a custom macro
/// method, where the single-dispatcher lowering cannot reach it.
fn reject_indirect(app: &App, claimed: &[Span]) -> IngestResult<()> {
    for class in &app.library_classes {
        for call in &class.unknown_calls {
            reject_nested(call, &app.controllers, claimed)?;
        }
        for method in &class.methods {
            reject_nested(&method.body, &app.controllers, claimed)?;
        }
    }
    for controller in &app.controllers {
        for item in &controller.body {
            match item {
                ControllerBodyItem::ClassMethod { method, .. } => reject_nested(&method.body, &app.controllers, claimed)?,
                ControllerBodyItem::Unknown { expr, .. }
                    if !matches!(macro_name(expr), Some(MACRO | "content_security_policy_report_only")) =>
                {
                    reject_nested(expr, &app.controllers, claimed)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn reject_nested(body: &Expr, controllers: &[crate::Controller], claimed: &[Span]) -> IngestResult<()> {
    let mut spans: Vec<Span> = Vec::new();
    collect_macro_calls(body, controllers, &mut spans);
    for span in spans {
        if claimed.contains(&span) { continue; }
        refuse_unit(
            span,
            "content_security_policy requires a receiverless or self declaration; nested DSLs, concerns, custom macros, and explicit controller receivers are not supported",
        )?;
    }
    Ok(())
}

fn collect_macro_calls(e: &Expr, controllers: &[crate::Controller], out: &mut Vec<Span>) {
    let explicit_controller = match &*e.node {
        ExprNode::Send { recv: Some(receiver), method, args, block, .. }
            if matches!(method.as_str(), MACRO | "content_security_policy_report_only") =>
        {
            (matches!(&*receiver.node, ExprNode::Var { .. }) && (block.is_some() || !args.is_empty()))
                || matches!(&*receiver.node, ExprNode::Const { path } if !path.is_empty()
                && controllers.iter().any(|controller| {
                    let mut names = controller.name.0.as_str().rsplit("::");
                    path.iter().rev().all(|part| names.next() == Some(part.as_str()))
                }))
        }
        _ => false,
    };
    if explicit_controller || matches!(macro_name(e), Some(MACRO | "content_security_policy_report_only")) {
        out.push(e.span);
    }
    e.node.for_each_child(&mut |child| collect_macro_calls(child, controllers, out));
}

/// Append `suffix` to every local name in the subtree - `Var` reads,
/// `Let`, lambda parameters, and assignment `LValue::Var` targets - so the
/// callback's locals cannot collide with another inlined callback's.
fn rename_locals(e: &mut Expr, suffix: &str) {
    match &mut *e.node {
        ExprNode::Var { name, .. } | ExprNode::Let { name, .. } => append(name, suffix),
        ExprNode::Lambda { params, rest_param, extra_params, block_param, .. } => {
            for p in params.iter_mut() {
                append(p, suffix);
            }
            if let Some(r) = rest_param {
                append(r, suffix);
            }
            if let Some(b) = block_param {
                append(b, suffix);
            }
            for p in extra_params.iter_mut() {
                append(&mut p.name, suffix);
            }
        }
        ExprNode::Assign { target, .. } | ExprNode::OpAssign { target, .. } => {
            rename_lvalue(target, suffix);
        }
        ExprNode::MultiAssign { targets, .. } => {
            for t in targets.iter_mut() {
                rename_lvalue(t, suffix);
            }
        }
        _ => {}
    }
    e.node.for_each_child_mut(&mut |c| rename_locals(c, suffix));
}

fn rename_lvalue(lv: &mut LValue, suffix: &str) {
    if let LValue::Var { name, .. } = lv {
        append(name, suffix);
    }
}

fn append(name: &mut Symbol, suffix: &str) {
    *name = Symbol::from(format!("{}{suffix}", name.as_str()).as_str());
}

fn syn(node: ExprNode) -> Expr {
    Expr::new(Span::synthetic(), node)
}

fn self_send(method: Symbol) -> Expr {
    syn(ExprNode::Send {
        recv: None,
        method,
        args: vec![],
        block: None,
        parenthesized: false,
    })
}

fn var(name: Symbol) -> Expr {
    syn(ExprNode::Var { id: VarId(0), name })
}

fn assign_var(name: Symbol, value: Expr) -> Expr {
    syn(ExprNode::Assign {
        target: LValue::Var { id: VarId(0), name },
        value,
    })
}

fn request_policy_assign(value: Expr) -> Expr {
    syn(ExprNode::Assign {
        target: LValue::Attr {
            recv: self_send(Symbol::from("request")),
            name: Symbol::from(MACRO),
        },
        value,
    })
}

fn push_body(stmts: &mut Vec<Expr>, mut body: Expr) {
    if let ExprNode::Seq { exprs } = &mut *body.node {
        stmts.append(exprs);
    } else {
        stmts.push(body);
    }
}

fn negate(e: Expr) -> Expr {
    syn(ExprNode::Send {
        recv: Some(e),
        method: Symbol::from("!"),
        args: vec![],
        block: None,
        parenthesized: false,
    })
}

fn block_lambda(body: Expr) -> Expr {
    syn(ExprNode::Lambda {
        params: vec![],
        rest_param: None,
        extra_params: vec![],
        block_param: None,
        body,
        block_style: BlockStyle::Do,
    })
}

fn guard_lambda(body: Expr) -> Expr {
    syn(ExprNode::Lambda {
        params: vec![],
        rest_param: None,
        extra_params: vec![],
        block_param: None,
        body,
        block_style: BlockStyle::Brace,
    })
}

/// Prism retains closure depth and block-local declarations; IR does not.
/// Refuse either unsupported scope before the source distinctions disappear.
fn captured_declarations(source_files: &[crate::span::SourceFile]) -> Vec<Span> {
    #[derive(Default)]
    struct Capture {
        depth: u32,
        found: bool,
    }
    impl<'pr> ruby_prism::Visit<'pr> for Capture {
        fn visit_block_node(&mut self, node: &ruby_prism::BlockNode<'pr>) {
            self.found |= node.parameters().is_some_and(|params| params.as_numbered_parameters_node().is_some());
            self.found |= node.parameters().and_then(|params| params.as_block_parameters_node())
                .is_some_and(|params| params.locals().iter().next().is_some());
            self.depth += 1;
            ruby_prism::visit_block_node(self, node);
            self.depth -= 1;
        }
        fn visit_lambda_node(&mut self, node: &ruby_prism::LambdaNode<'pr>) {
            self.found |= node.parameters().is_some_and(|params| params.as_numbered_parameters_node().is_some());
            self.found |= node.parameters().and_then(|params| params.as_block_parameters_node())
                .is_some_and(|params| params.locals().iter().next().is_some());
            self.depth += 1;
            ruby_prism::visit_lambda_node(self, node);
            self.depth -= 1;
        }
        fn visit_local_variable_read_node(&mut self, node: &ruby_prism::LocalVariableReadNode<'pr>) {
            self.found |= self.depth > 0 && node.depth() >= self.depth;
        }
        fn visit_local_variable_target_node(&mut self, node: &ruby_prism::LocalVariableTargetNode<'pr>) {
            self.found |= self.depth > 0 && node.depth() >= self.depth;
        }
        fn visit_local_variable_write_node(&mut self, node: &ruby_prism::LocalVariableWriteNode<'pr>) {
            self.found |= self.depth > 0 && node.depth() >= self.depth;
            ruby_prism::visit_local_variable_write_node(self, node);
        }
        fn visit_local_variable_or_write_node(&mut self, node: &ruby_prism::LocalVariableOrWriteNode<'pr>) {
            self.found |= self.depth > 0 && node.depth() >= self.depth;
            ruby_prism::visit_local_variable_or_write_node(self, node);
        }
        fn visit_local_variable_and_write_node(&mut self, node: &ruby_prism::LocalVariableAndWriteNode<'pr>) {
            self.found |= self.depth > 0 && node.depth() >= self.depth;
            ruby_prism::visit_local_variable_and_write_node(self, node);
        }
        fn visit_local_variable_operator_write_node(&mut self, node: &ruby_prism::LocalVariableOperatorWriteNode<'pr>) {
            self.found |= self.depth > 0 && node.depth() >= self.depth;
            ruby_prism::visit_local_variable_operator_write_node(self, node);
        }
    }
    struct Declarations {
        file: crate::span::FileId,
        captures: Vec<Span>,
    }
    impl<'pr> ruby_prism::Visit<'pr> for Declarations {
        fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
            if node.receiver().is_none_or(|receiver| receiver.as_self_node().is_some())
                && super::util::constant_id_str(&node.name()) == MACRO
            {
                let mut capture = Capture::default();
                ruby_prism::visit_call_node(&mut capture, node);
                if capture.found {
                    let location = node.location();
                    self.captures.push(Span {
                        file: self.file,
                        start: location.start_offset() as u32,
                        end: location.end_offset() as u32,
                    });
                }
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let mut captures = Vec::new();
    for source in source_files {
        if !source.text.contains(MACRO) {
            continue;
        }
        let parsed = super::prism::parse_silent(source.text.as_bytes());
        let mut scan = Declarations { file: sources::file_id(&source.path), captures: Vec::new() };
        ruby_prism::Visit::visit(&mut scan, &parsed.node());
        captures.extend(scan.captures);
    }
    captures
}

struct LocalScopes {
    prefix: String,
    next: u32,
}

impl LocalScopes {
    fn suffix(&mut self) -> String {
        let suffix = format!("{}{}", self.prefix, self.next);
        self.next += 1;
        suffix
    }
}

fn refuse<T>(span: Span, message: impl Into<String>) -> IngestResult<Option<T>> {
    // Strict mode aborts ingest; survey mode records the located gap and
    // returns `None` so the declaration is left untouched, not replaced.
    survey::continue_or_fail(located(span, message.into()))?;
    Ok(None)
}

fn refuse_unit(span: Span, message: impl Into<String>) -> IngestResult<()> {
    survey::continue_or_fail(located(span, message.into()))
}

fn located(span: Span, message: String) -> IngestError {
    let file = match sources::path_of(span.file) {
        Some(path) => match sources::line_at(&path, span.start as usize) {
            Some(line) => format!("{path}:{line}"),
            None => path,
        },
        None => String::new(),
    };
    IngestError::Unsupported { file, message }
}
