//! Refuse unsupported Rails CSP configuration before generic config readers
//! consume it.
//!
//! The direct forms are `Rails.application.config`, `Rails.configuration`,
//! and a bare `config` inside a `Rails::Application` subclass body or a
//! `Rails.application.configure do … end` block. A valid initializer can
//! also reach the same configuration object indirectly, and the earlier
//! recognizer dropped those settings on the floor — the initializer ledger
//! only warns — so an unsupported global/nonce/report-only setting became a
//! silent success:
//!
//! ```ruby
//! Rails.application.config.tap do |config|
//!   config.content_security_policy do |policy|
//!     policy.script_src :self
//!   end
//! end
//!
//! config = Rails.application.config
//! config.content_security_policy_report_only = true
//! ```
//!
//! This pass now follows proven configuration and application aliases. An
//! alias is created only by a direct binding from a framework expression:
//! `config = Rails.application.config` / `Rails.configuration`,
//! `app = Rails.application`, the `tap` / `then` / `yield_self` block forms
//! on a proven object, and the application that `Rails.application.configure`
//! yields (Rails runs the block with `instance_eval`, so its parameter is the
//! application and `param.config` is the configuration). Reassignment and
//! block-parameter shadowing drop the alias again, and an unknown receiver is
//! never assumed to be configuration, so comments, strings, and unrelated
//! `config` objects are left alone. Detection still runs before generic
//! configuration consumption, reports each unsupported setting at its source
//! location, and keeps the survey gap.

use std::collections::HashMap;
use std::path::Path;

use ruby_prism::{Node, Visit};

use super::util::constant_id_str;
use super::{IngestError, IngestResult, survey};
use crate::vfs::Vfs;

fn rails(node: &Node<'_>) -> bool {
    if let Some(constant) = node.as_constant_read_node() {
        return constant_id_str(&constant.name()) == "Rails";
    }
    node.as_constant_path_node().is_some_and(|path| {
        path.parent().is_none() && path.name().is_some_and(|name| constant_id_str(&name) == "Rails")
    })
}

fn application(node: &Node<'_>) -> bool {
    node.as_call_node().is_some_and(|call| {
        constant_id_str(&call.name()) == "application"
            && call.arguments().is_none()
            && call.receiver().is_some_and(|receiver| rails(&receiver))
    })
}

/// Framework provenance and truthiness, including uncertain short-circuit results.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Alias {
    config: bool,
    app: bool,
    truthy: Option<bool>,
}

impl Alias {
    const CONFIG: Self = Self { config: true, app: false, truthy: Some(true) };
    const APP: Self = Self { config: false, app: true, truthy: Some(true) };
    const FALSE: Self = Self { config: false, app: false, truthy: Some(false) };
    const TRUE: Self = Self { config: false, app: false, truthy: Some(true) };

    fn union(self, other: Self) -> Self {
        Self {
            config: self.config || other.config,
            app: self.app || other.app,
            truthy: if self.truthy == other.truthy { self.truthy } else { None },
        }
    }

    fn or(self, other: Self) -> Self {
        match self.truthy {
            Some(true) => self,
            Some(false) => other,
            None => Self { truthy: Some(true), ..self }.union(other),
        }
    }

    fn and(self, other: Self) -> Self {
        match self.truthy {
            Some(false) => self,
            Some(true) => other,
            None => Self::FALSE.union(other),
        }
    }
}

#[derive(Default)]
struct Scan {
    in_application: bool,
    /// Proven aliases in the current lexical scope.
    locals: HashMap<String, Alias>,
    sites: Vec<(usize, &'static str)>,
}

impl Scan {
    /// True when `node` names a Rails configuration object: a direct
    /// `Rails.application.config` / `Rails.configuration` / bare `config`
    /// form, `<app>.config` through a proven application alias, or a read of
    /// a proven configuration alias.
    fn is_config(&self, node: &Node<'_>) -> bool {
        if let Some(read) = node.as_local_variable_read_node() {
            return self.locals.get(constant_id_str(&read.name())).is_some_and(|alias| alias.config);
        }
        if node.as_it_local_variable_read_node().is_some() {
            return self.locals.get("it").is_some_and(|alias| alias.config);
        }
        let Some(call) = node.as_call_node() else { return false };
        if call.arguments().is_some() {
            return false;
        }
        match constant_id_str(&call.name()) {
            "config" => match call.receiver() {
                None => self.in_application,
                Some(receiver) => {
                    self.is_app(&receiver)
                        || (self.in_application && receiver.as_self_node().is_some())
                }
            },
            "configuration" => call.receiver().is_some_and(|receiver| rails(&receiver)),
            "tap" => call.receiver().is_some_and(|receiver| self.is_config(&receiver)),
            _ => false,
        }
    }

    /// True when `node` names the Rails application: `Rails.application` or a
    /// read of a proven application alias.
    fn is_app(&self, node: &Node<'_>) -> bool {
        if let Some(read) = node.as_local_variable_read_node() {
            return self.locals.get(constant_id_str(&read.name())).is_some_and(|alias| alias.app);
        }
        if node.as_it_local_variable_read_node().is_some() {
            return self.locals.get("it").is_some_and(|alias| alias.app);
        }
        if let Some(call) = node.as_call_node()
            && constant_id_str(&call.name()) == "tap" && call.arguments().is_none()
        {
            return call.receiver().is_some_and(|receiver| self.is_app(&receiver));
        }
        application(node)
    }

    fn alias_of(&self, value: &Node<'_>) -> Alias {
        if let Some(read) = value.as_local_variable_read_node() {
            return self.locals.get(constant_id_str(&read.name())).copied().unwrap_or_default();
        }
        if value.as_it_local_variable_read_node().is_some() {
            return self.locals.get("it").copied().unwrap_or_default();
        }
        if let Some(or) = value.as_or_node() {
            let left = self.alias_of(&or.left());
            return if left.truthy == Some(true) { left } else { left.or(self.alias_of(&or.right())) };
        }
        if let Some(and) = value.as_and_node() {
            let left = self.alias_of(&and.left());
            return if left.truthy == Some(false) { left } else { left.and(self.alias_of(&and.right())) };
        }
        if value.as_nil_node().is_some() || value.as_false_node().is_some() {
            return Alias::FALSE;
        }
        if value.as_true_node().is_some() || value.as_string_node().is_some()
            || value.as_integer_node().is_some() || value.as_array_node().is_some()
            || value.as_hash_node().is_some() || value.as_symbol_node().is_some()
        {
            return Alias::TRUE;
        }
        if let Some(call) = value.as_call_node()
            && constant_id_str(&call.name()) == "tap" && call.arguments().is_none()
        {
            return call.receiver().map(|receiver| self.alias_of(&receiver)).unwrap_or_default();
        }
        if self.is_config(value) {
            let mut alias = Alias::CONFIG;
            if let Some(call) = value.as_call_node()
                && constant_id_str(&call.name()) == "config"
                && call.receiver().is_some_and(|receiver|
                    !application(&receiver) && receiver.as_self_node().is_none())
            {
                alias.truthy = None;
            }
            alias
        } else if self.is_app(value) {
            Alias::APP
        } else {
            Alias::default()
        }
    }

    /// The alias bound to a block's first parameter when the call yields a
    /// proven object: `tap` / `then` / `yield_self` forward their receiver,
    /// and `configure` yields the application (Rails runs the block with
    /// `instance_eval`).
    fn yielded_alias(&self, name: &str, receiver: Option<Node<'_>>) -> Option<Alias> {
        let alias = self.alias_of(&receiver?);
        match name {
            "tap" | "then" | "yield_self" if alias.config || alias.app => Some(alias),
            "configure" if alias.app => Some(alias),
            _ => None,
        }
    }

    fn record(&mut self, name: &str, receiver: Option<Node<'_>>, offset: usize) {
        let key = name.strip_suffix('=').unwrap_or(name);
        let construct = match key {
            "content_security_policy" => "global content_security_policy",
            "content_security_policy_nonce_generator" => "content_security_policy_nonce_generator",
            "content_security_policy_nonce_directives" => "content_security_policy_nonce_directives",
            "content_security_policy_report_only" => "content_security_policy_report_only",
            _ => return,
        };
        if receiver.is_some_and(|receiver| self.is_config(&receiver)) {
            self.sites.push((offset, construct));
        }
    }

    /// Shadow — or, for a yielded alias, bind — every name a scope
    /// introduces, returning the previous bindings to restore when the scope
    /// ends.
    fn enter_scope(
        &mut self,
        names: &[String],
        bound: Option<&str>,
        alias: Option<Alias>,
    ) -> Vec<(String, Option<Alias>)> {
        let mut saved = Vec::with_capacity(names.len());
        for name in names {
            saved.push((name.clone(), self.locals.get(name).copied()));
            match alias {
                Some(kind) if Some(name.as_str()) == bound => {
                    self.locals.insert(name.clone(), kind);
                }
                _ => {
                    self.locals.remove(name);
                }
            }
        }
        saved
    }

    fn exit_scope(&mut self, saved: Vec<(String, Option<Alias>)>) {
        for (name, previous) in saved.into_iter().rev() {
            match previous {
                Some(kind) => {
                    self.locals.insert(name, kind);
                }
                None => {
                    self.locals.remove(&name);
                }
            }
        }
    }
}

/// Every local name a block/lambda scope introduces: its scope locals plus
/// each declared parameter, so a parameter that shadows an outer alias is
/// dropped for the scope regardless of how prism lists it.
fn scope_names(params: Option<Node<'_>>, locals: ruby_prism::ConstantList<'_>) -> Vec<String> {
    let mut names: Vec<String> =
        locals.iter().map(|id| constant_id_str(&id).to_string()).collect();
    let Some(params) = params else { return names };
    if params.as_it_parameters_node().is_some() {
        names.push("it".to_string());
        return names;
    }
    if params.as_numbered_parameters_node().is_some() {
        names.push("_1".to_string());
        return names;
    }
    let Some(params) = params.as_block_parameters_node() else {
        return names;
    };
    if let Some(parameters) = params.parameters() {
        for required in parameters.requireds().iter() {
            if let Some(required) = required.as_required_parameter_node() {
                names.push(constant_id_str(&required.name()).to_string());
            }
        }
        for post in parameters.posts().iter() {
            if let Some(post) = post.as_required_parameter_node() {
                names.push(constant_id_str(&post.name()).to_string());
            }
        }
        for optional in parameters.optionals().iter() {
            if let Some(optional) = optional.as_optional_parameter_node() {
                names.push(constant_id_str(&optional.name()).to_string());
            }
        }
        if let Some(rest) = parameters.rest().and_then(|node| node.as_rest_parameter_node()) {
            if let Some(name) = rest.name() {
                names.push(constant_id_str(&name).to_string());
            }
        }
        for keyword in parameters.keywords().iter() {
            if let Some(keyword) = keyword.as_required_keyword_parameter_node() {
                names.push(constant_id_str(&keyword.name()).to_string());
            } else if let Some(keyword) = keyword.as_optional_keyword_parameter_node() {
                names.push(constant_id_str(&keyword.name()).to_string());
            }
        }
        if let Some(rest) =
            parameters.keyword_rest().and_then(|node| node.as_keyword_rest_parameter_node())
        {
            if let Some(name) = rest.name() {
                names.push(constant_id_str(&name).to_string());
            }
        }
    }
    for local in params.locals().iter() {
        if let Some(local) = local.as_block_local_variable_node() {
            names.push(constant_id_str(&local.name()).to_string());
        }
    }
    names
}

/// The first yielded value binds a positional or implicit block parameter.
fn first_yielded_param(params: Option<Node<'_>>) -> Option<String> {
    let params = params?;
    if params.as_it_parameters_node().is_some() {
        return Some("it".to_string());
    }
    if params.as_numbered_parameters_node().is_some() {
        return Some("_1".to_string());
    }
    let parameters = params.as_block_parameters_node()?.parameters()?;
    if let Some(required) = parameters.requireds().iter().next() {
        return required.as_required_parameter_node()
            .map(|parameter| constant_id_str(&parameter.name()).to_string());
    }
    let optional = parameters.optionals().iter().next()?.as_optional_parameter_node()?;
    Some(constant_id_str(&optional.name()).to_string())
}

impl<'pr> Visit<'pr> for Scan {
    fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
        let previous_in_application = self.in_application;
        let previous_locals = std::mem::take(&mut self.locals);
        self.in_application = node.superclass().is_some_and(|parent| {
            parent.as_constant_path_node().is_some_and(|path| {
                path.name().is_some_and(|name| constant_id_str(&name) == "Application")
                    && path.parent().is_some_and(|parent| rails(&parent))
            })
        });
        ruby_prism::visit_class_node(self, node);
        self.in_application = previous_in_application;
        self.locals = previous_locals;
    }

    fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
        let previous_in_application = self.in_application;
        let previous_locals = std::mem::take(&mut self.locals);
        self.in_application = false;
        ruby_prism::visit_module_node(self, node);
        self.in_application = previous_in_application;
        self.locals = previous_locals;
    }

    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        let previous_locals = std::mem::take(&mut self.locals);
        ruby_prism::visit_def_node(self, node);
        self.locals = previous_locals;
    }

    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
        let name = constant_id_str(&node.name());
        self.record(name, node.receiver(), node.location().start_offset());

        // Recurse the receiver and arguments in the enclosing scope: a block
        // parameter is not in scope there, so set it up only around the block
        // body — and after any nested receiver block has already been walked.
        if let Some(receiver) = node.receiver() {
            self.visit(&receiver);
        }
        if let Some(arguments) = node.arguments() {
            self.visit_arguments_node(&arguments);
        }

        if let Some(block) = node.block() {
            let alias = self.yielded_alias(name, node.receiver());
            let previous_in_application = self.in_application;
            if name == "configure"
                && node.receiver().is_some_and(|receiver| self.is_app(&receiver))
            {
                self.in_application = true;
            }
            match block.as_block_node() {
                Some(block) => {
                    let names = scope_names(block.parameters(), block.locals());
                    let bound = first_yielded_param(block.parameters());
                    let saved = self.enter_scope(&names, bound.as_deref(), alias);
                    ruby_prism::visit_block_node(self, &block);
                    self.exit_scope(saved);
                }
                None => self.visit(&block),
            }
            self.in_application = previous_in_application;
        }
    }

    fn visit_lambda_node(&mut self, node: &ruby_prism::LambdaNode<'pr>) {
        let names = scope_names(node.parameters(), node.locals());
        let saved = self.enter_scope(&names, None, None);
        ruby_prism::visit_lambda_node(self, node);
        self.exit_scope(saved);
    }

    fn visit_or_node(&mut self, node: &ruby_prism::OrNode<'pr>) {
        let left = self.alias_of(&node.left());
        self.visit(&node.left());
        if left.truthy != Some(true) {
            self.visit(&node.right());
        }
    }

    fn visit_and_node(&mut self, node: &ruby_prism::AndNode<'pr>) {
        let left = self.alias_of(&node.left());
        self.visit(&node.left());
        if left.truthy != Some(false) {
            self.visit(&node.right());
        }
    }

    fn visit_local_variable_write_node(&mut self, node: &ruby_prism::LocalVariableWriteNode<'pr>) {
        let alias = self.alias_of(&node.value());
        ruby_prism::visit_local_variable_write_node(self, node);
        self.locals.insert(constant_id_str(&node.name()).to_string(), alias);
    }

    fn visit_local_variable_operator_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOperatorWriteNode<'pr>,
    ) {
        ruby_prism::visit_local_variable_operator_write_node(self, node);
        self.locals.remove(constant_id_str(&node.name()));
    }

    fn visit_local_variable_or_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableOrWriteNode<'pr>,
    ) {
        let name = constant_id_str(&node.name());
        let previous = self.locals.get(name).copied().unwrap_or_default();
        if previous.truthy == Some(true) {
            return;
        }
        let assigned = self.alias_of(&node.value());
        ruby_prism::visit_local_variable_or_write_node(self, node);
        let result = previous.or(assigned);
        self.locals.insert(name.to_string(), result);
    }

    fn visit_local_variable_and_write_node(
        &mut self,
        node: &ruby_prism::LocalVariableAndWriteNode<'pr>,
    ) {
        let name = constant_id_str(&node.name());
        let previous = self.locals.get(name).copied().unwrap_or_default();
        if previous.truthy == Some(false) {
            return;
        }
        let assigned = self.alias_of(&node.value());
        ruby_prism::visit_local_variable_and_write_node(self, node);
        let result = previous.and(assigned);
        self.locals.insert(name.to_string(), result);
    }

    fn visit_call_or_write_node(&mut self, node: &ruby_prism::CallOrWriteNode<'pr>) {
        self.record(
            constant_id_str(&node.read_name()),
            node.receiver(),
            node.location().start_offset(),
        );
        ruby_prism::visit_call_or_write_node(self, node);
    }

    fn visit_call_and_write_node(&mut self, node: &ruby_prism::CallAndWriteNode<'pr>) {
        self.record(
            constant_id_str(&node.read_name()),
            node.receiver(),
            node.location().start_offset(),
        );
        ruby_prism::visit_call_and_write_node(self, node);
    }

    fn visit_call_operator_write_node(&mut self, node: &ruby_prism::CallOperatorWriteNode<'pr>) {
        self.record(
            constant_id_str(&node.read_name()),
            node.receiver(),
            node.location().start_offset(),
        );
        ruby_prism::visit_call_operator_write_node(self, node);
    }
}

pub(super) fn validate<V: Vfs + ?Sized>(vfs: &V, dir: &Path) -> IngestResult<()> {
    let mut files = vec![dir.join("config/application.rb")];
    for subdirectory in ["config/environments", "config/initializers"] {
        let path = dir.join(subdirectory);
        if vfs.is_dir(&path) {
            files.extend(super::app::read_rb_files(vfs, &path)?);
        }
    }
    for path in files {
        let Ok(source) = vfs.read(&path) else { continue };
        // This only avoids parsing irrelevant files; admission uses AST nodes.
        if !source
            .windows(b"content_security_policy".len())
            .any(|word| word == b"content_security_policy")
        {
            continue;
        }
        let result = super::prism::parse_silent(&source);
        let mut scan = Scan::default();
        scan.visit(&result.node());
        for (offset, construct) in scan.sites {
            let line = source[..offset].iter().filter(|&&byte| byte == b'\n').count() + 1;
            let column = offset
                - source[..offset].iter().rposition(|&byte| byte == b'\n').map_or(0, |index| index + 1)
                + 1;
            let file = path.display().to_string();
            survey::continue_or_fail(IngestError::Unsupported {
                message: format!(
                    "{construct} is not supported at {file}:{line}:{column}; only direct controller frame_src declarations have a verified runtime"
                ),
                file,
            })?;
        }
    }
    Ok(())
}
