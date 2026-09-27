//! MCP server at `POST /mcp` (streamable HTTP, stateless, JSON responses).
//! Tools come from op-core's registry (op-engine's own are in
//! [`crate::tools`]), listed and called as the caller's [`Identity`] under
//! its [`ToolScope`]: `?scope=brain` (what the decision cycle hands a brain)
//! is the brain identity with the brain tools, or a brain token's allowlist.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use op_core::tools::{ToolScope, ToolSpec};
use op_core::{ApiError, Ctx, Identity};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;

pub use crate::tools::{READ_TOOLS, WRITE_TOOLS};

/// Who is calling over MCP, resolved by [`handler`] and read back from the
/// request parts rmcp hands each call.
#[derive(Debug, Clone)]
pub struct Caller {
    pub identity: Identity,
    pub scope: ToolScope,
}

/// A registry tool as MCP shows it.
pub fn to_mcp(t: &ToolSpec) -> Tool {
    let schema = Arc::new(t.input_schema.as_object().cloned().unwrap_or_default());
    let mut tool = Tool::new(t.name, t.description, schema);
    tool.annotations = Some(if t.read { ToolAnnotations::new().read_only(true) } else { ToolAnnotations::new().read_only(false).destructive(false) });
    tool
}

/// Every registered tool, as MCP shows it.
pub fn tools(ctx: &Ctx) -> Vec<Tool> {
    crate::tools::register_tools(ctx);
    ctx.tools().list().iter().map(to_mcp).collect()
}

#[derive(Clone)]
pub struct Server {
    ctx: Ctx,
}

impl Server {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }

    /// What `caller` may list.
    pub fn listed(&self, caller: &Caller) -> Vec<Tool> {
        self.ctx.tools().listed_for(&caller.identity, &caller.scope).iter().map(to_mcp).collect()
    }
}

/// The caller [`handler`] put on the request. Missing means the request
/// didn't come through it: no tools.
fn caller_of(c: &RequestContext<RoleServer>) -> Caller {
    c.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Caller>().cloned())
        .unwrap_or(Caller { identity: Identity::anonymous(), scope: ToolScope::Only(vec![]) })
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let mut imp = Implementation::new("openpasture", env!("CARGO_PKG_VERSION"));
        imp.title = Some("openpasture".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(imp)
            .with_instructions("Farm tools for openpasture: the farm, paddocks, herds, collar positions, boundaries, grazing signals, land reports, knowledge and decisions. Every boundary comes from a recorded decision and the herd's autonomy setting.")
    }

    // Claude Code 2.1 negotiates the stateless 2026-07-28 revision through
    // `server/discover`, lists our tools, and then never offers them to the
    // model. Capped at 2025-11-25 it falls back to `initialize` and the tools
    // work, as they do for Codex.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    async fn list_tools(&self, _r: Option<PaginatedRequestParams>, c: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.listed(&caller_of(&c))))
    }

    async fn call_tool(&self, req: CallToolRequestParams, c: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let name = req.name.to_string();
        let caller = caller_of(&c);
        if !self.listed(&caller).iter().any(|t| t.name == name) {
            return Err(ErrorData::invalid_params(format!("Unknown tool {name}."), None));
        }
        let args = Value::Object(req.arguments.clone().unwrap_or_default());
        // Stateless requests carry rmcp's own default name, which says nothing.
        let client = c.peer.peer_info().map(|i| i.client_info.name.clone()).filter(|n| n != "rmcp");
        let started = std::time::Instant::now();
        let outcome = self.ctx.tools().call(&self.ctx, &name, args, client, caller.identity.clone(), &caller.scope).await;
        tracing::info!(
            target: "op_engine::mcp",
            tool = %name,
            scope = if matches!(caller.scope, ToolScope::Full) { "full" } else { "brain" },
            ok = outcome.is_ok(),
            ms = started.elapsed().as_millis() as u64,
            "mcp tool call"
        );
        let result = match outcome {
            Ok(v) => {
                let text = serde_json::to_string_pretty(&v).unwrap_or_default();
                let mut r = CallToolResult::success(vec![ContentBlock::text(text)]);
                if v.is_object() {
                    r.structured_content = Some(v);
                }
                r
            }
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.message)]),
        };
        Ok(result.into())
    }
}

/// Run one tool as the farm itself (owner, via `system`). Shared by tests
/// and callers that already checked who is asking.
pub async fn call(ctx: &Ctx, name: &str, a: &Value, client: Option<String>) -> Result<Value, ApiError> {
    crate::tools::register_tools(ctx);
    ctx.tools().call(ctx, name, a.clone(), client, Identity::system(), &ToolScope::Full).await
}

type Services = Mutex<HashMap<PathBuf, StreamableHttpService<Server, LocalSessionManager>>>;

fn services() -> &'static Services {
    static S: OnceLock<Services> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn service(ctx: &Ctx) -> StreamableHttpService<Server, LocalSessionManager> {
    let key = ctx.data_dir().to_path_buf();
    let mut map = services().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key)
        .or_insert_with(|| {
            let mut config = StreamableHttpServerConfig::default().with_legacy_session_mode(false).with_json_response(true).with_sse_keep_alive(None);
            // Off localhost the app token guards /mcp (op-server), so any Host is fine.
            let local = ctx.local_url();
            if !["http://127.0.0.1", "http://localhost", "http://[::1]"].iter().any(|p| local.starts_with(p)) {
                config = config.disable_allowed_hosts();
            }
            let c = ctx.clone();
            StreamableHttpService::new(move || Ok(Server::new(c.clone())), Arc::new(LocalSessionManager::default()), config)
        })
        .clone()
}

/// `/mcp`: resolve the caller from the guard's [`Identity`] (500 without
/// one) and hand the request to rmcp's streamable HTTP service.
/// `?scope=brain` narrows any caller to the brain: [`Identity::brain`] with a
/// brain token's allowlist, else the registry's brain tools.
pub async fn handler(State(ctx): State<Ctx>, mut req: Request) -> Response {
    let Some(identity) = req.extensions().get::<Identity>().cloned() else {
        return ApiError::internal("Identity missing").into_response();
    };
    crate::tools::register_tools(&ctx);
    let brain_scope = req.uri().query().is_some_and(|q| q.split('&').any(|kv| kv == "scope=brain"));
    let token_scope = req.extensions().get::<ToolScope>().cloned();
    let caller = if brain_scope {
        Caller { identity: Identity::brain(), scope: token_scope.unwrap_or_else(|| ToolScope::Only(ctx.tools().brain_tools())) }
    } else {
        Caller { identity, scope: token_scope.unwrap_or(ToolScope::Full) }
    };
    req.extensions_mut().insert(caller);
    let svc = service(&ctx);
    svc.handle(req).await.map(Body::new).into_response()
}
