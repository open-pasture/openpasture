//! The tool registry: every MCP tool (and every tool a brain or a text
//! question may use) is registered here by the crate that owns it, so crates
//! below op-engine reach tools without depending on it.
//!
//! Each crate with tools exposes an idempotent `pub fn register_tools(ctx)`
//! (built on [`ToolRegistry::register_once`]) and calls it from its `start()`;
//! tests call it directly. MCP lists and calls through the registry, filtered
//! by the caller's [`Identity`] and [`ToolScope`].

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use serde_json::Value;

use crate::Ctx;
use crate::error::ApiError;
use crate::identity::{Identity, Role};

pub type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send>>;
pub type ToolFn = Arc<dyn Fn(ToolCall) -> ToolFuture + Send + Sync>;

/// One call: the tool gets the context, its arguments, the MCP client's name
/// (when known) and who is calling.
pub struct ToolCall {
    pub ctx: Ctx,
    pub args: Value,
    pub client: Option<String>,
    pub identity: Identity,
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema object, `additionalProperties: false`.
    pub input_schema: Value,
    /// No side effects: offered to text questions and viewers.
    pub read: bool,
    /// Offered to the daily decision brain (the 11 existing read tools only;
    /// new tools false unless a stream's notes justify it).
    pub brain: bool,
    /// `Viewer` for read tools; `Hand` or `Manager` for writes.
    pub min_role: Role,
    pub run: ToolFn,
}

impl ToolSpec {
    /// Wrap an async fn as a tool body: `ToolSpec::run_fn(|call| async move { … })`.
    pub fn run_fn<F, Fut>(f: F) -> ToolFn
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ApiError>> + Send + 'static,
    {
        Arc::new(move |call| Box::pin(f(call)))
    }

    pub fn info(&self) -> ToolInfo {
        ToolInfo { name: self.name.to_owned(), description: self.description.to_owned(), input_schema: self.input_schema.clone() }
    }
}

impl std::fmt::Debug for ToolSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSpec")
            .field("name", &self.name)
            .field("read", &self.read)
            .field("brain", &self.brain)
            .field("min_role", &self.min_role)
            .finish_non_exhaustive()
    }
}

/// What a tool loop shows a model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Which tools a caller may list and call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolScope {
    /// Everything the caller's role allows.
    Full,
    /// Only these names, and only read tools among them (a brain token's allowlist).
    Only(Vec<String>),
}

#[derive(Default)]
pub struct ToolRegistry {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    tools: Vec<ToolSpec>,
    groups: HashSet<&'static str>,
}

impl ToolRegistry {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Panics on a duplicate name.
    pub fn register(&self, spec: ToolSpec) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        add(&mut inner, spec);
    }

    /// Register a crate's tools once: the first call for `group` registers
    /// what `make` returns, later calls do nothing. Atomic, so two starts at
    /// once can't both register. A name another group already took panics.
    pub fn register_once(&self, group: &'static str, make: impl FnOnce() -> Vec<ToolSpec>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if !inner.groups.insert(group) {
            return;
        }
        for spec in make() {
            add(&mut inner, spec);
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.read().tools.iter().any(|t| t.name == name)
    }

    pub fn get(&self, name: &str) -> Option<ToolSpec> {
        self.read().tools.iter().find(|t| t.name == name).cloned()
    }

    /// Every tool, in registration order.
    pub fn list(&self) -> Vec<ToolSpec> {
        self.read().tools.clone()
    }

    /// Names with `brain: true`, in registration order.
    pub fn brain_tools(&self) -> Vec<String> {
        self.read().tools.iter().filter(|t| t.brain).map(|t| t.name.to_owned()).collect()
    }

    /// What `identity` may list under `scope`, in registration order.
    /// `Only`: those names, read tools only. `Full`: every tool whose
    /// `min_role` the identity has (read tools carry `Viewer`, so that is the
    /// read tools plus the writes the role allows). Anonymous: nothing.
    pub fn listed_for(&self, identity: &Identity, scope: &ToolScope) -> Vec<ToolSpec> {
        self.read().tools.iter().filter(|t| allowed(t, identity, scope)).cloned().collect()
    }

    /// Call a tool as `identity` under `scope`. A name that doesn't exist or
    /// isn't in the scope is 400 "Unknown tool"; a role too low is 403 (401
    /// when anonymous).
    pub async fn call(&self, ctx: &Ctx, name: &str, args: Value, client: Option<String>, identity: Identity, scope: &ToolScope) -> Result<Value, ApiError> {
        let spec = self.get(name).filter(|t| in_scope(t, scope)).ok_or_else(|| ApiError::bad_request(format!("Unknown tool {name}.")))?;
        identity.require(spec.min_role)?;
        (spec.run)(ToolCall { ctx: ctx.clone(), args, client, identity }).await
    }
}

fn add(inner: &mut Inner, spec: ToolSpec) {
    assert!(!inner.tools.iter().any(|t| t.name == spec.name), "tool {} is registered twice", spec.name);
    inner.tools.push(spec);
}

fn in_scope(t: &ToolSpec, scope: &ToolScope) -> bool {
    match scope {
        ToolScope::Full => true,
        ToolScope::Only(names) => t.read && names.iter().any(|n| n == t.name),
    }
}

fn allowed(t: &ToolSpec, identity: &Identity, scope: &ToolScope) -> bool {
    in_scope(t, scope) && identity.can(t.min_role)
}

/// A set of tools a model can use in a loop (a text question, `Brain::ask`).
#[async_trait::async_trait]
pub trait ToolRunner: Send + Sync {
    fn tools(&self) -> Vec<ToolInfo>;
    async fn call(&self, name: &str, args: Value) -> Result<Value, String>;
}

/// [`Ctx::tool_runner`]: read tools the identity may use, minus some.
pub(crate) struct RegistryRunner {
    pub ctx: Ctx,
    pub identity: Identity,
    pub names: Vec<String>,
}

impl RegistryRunner {
    pub fn new(ctx: &Ctx, identity: Identity, exclude: &[&str]) -> Self {
        let names = ctx
            .tools()
            .listed_for(&identity, &ToolScope::Full)
            .into_iter()
            .filter(|t| t.read && !exclude.contains(&t.name))
            .map(|t| t.name.to_owned())
            .collect();
        Self { ctx: ctx.clone(), identity, names }
    }
}

#[async_trait::async_trait]
impl ToolRunner for RegistryRunner {
    fn tools(&self) -> Vec<ToolInfo> {
        self.ctx.tools().listed_for(&self.identity, &ToolScope::Only(self.names.clone())).iter().map(ToolSpec::info).collect()
    }

    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let scope = ToolScope::Only(self.names.clone());
        self.ctx.tools().call(&self.ctx, name, args, None, self.identity.clone(), &scope).await.map_err(|e| e.message)
    }
}
