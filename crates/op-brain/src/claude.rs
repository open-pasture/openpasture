//! The Claude Code CLI brain: `claude -p` as the user signed it in (their
//! Claude plan). Prompt on stdin, answer shaped by `--json-schema`, progress
//! from `--output-format stream-json`, no built-in tools, only our MCP read
//! tools allowed, empty temp dir. Tested with Claude Code 2.1.283.
//!
//! Questions ([`Brain::ask`]) run the same way without the schema: the answer
//! is the result text, and the MCP is this server's brain scope with a token
//! that allows the question's tools only (never `run_sql`).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use op_core::{BrainId, BrainToken, Ctx};
use serde_json::{Value, json};

use crate::parse::{self, snippet};
use crate::prompt::{SYSTEM, build_prompt};
use crate::{AskError, AskRequest, Brain, DecisionOutput, DecisionRequest, cli, decision_schema};

pub const TIMEOUT: Duration = Duration::from_secs(300);

/// The MCP read tools the decision brain may call (op-engine's brain tools).
pub const MCP_TOOLS: [&str; 11] = [
    "get_farm",
    "list_paddocks",
    "get_herd",
    "get_herd_positions",
    "get_boundary_status",
    "get_signals",
    "get_land_report",
    "search_knowledge",
    "list_decisions",
    "get_decision",
    "run_sql",
];

pub struct ClaudeBrain {
    bin: PathBuf,
    model: Option<String>,
    pub timeout: Duration,
    /// This server, whose MCP read tools a question may use.
    ctx: Option<Ctx>,
}

impl ClaudeBrain {
    pub fn new(bin: PathBuf, model: Option<String>) -> Self {
        Self { bin, model, timeout: TIMEOUT, ctx: None }
    }

    /// Questions reach this server's MCP read tools through a brain token.
    /// Without it they are answered from the record alone.
    pub fn with_ctx(mut self, ctx: Ctx) -> Self {
        self.ctx = Some(ctx);
        self
    }
}

/// The `--mcp-config` file for our MCP server: 0600 in the run dir, so a
/// brain token in its headers never shows up in argv.
pub fn write_mcp_config(dir: &std::path::Path, mcp_url: &str) -> anyhow::Result<std::path::PathBuf> {
    let (url, token) = crate::split_mcp_token(mcp_url);
    let mut server = json!({ "type": "http", "url": url });
    if let Some(t) = token {
        server["headers"] = json!({ "Authorization": format!("Bearer {t}") });
    }
    let path = dir.join("mcp.json");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&path)?;
    std::io::Write::write_all(&mut f, json!({ "mcpServers": { "openpasture": server } }).to_string().as_bytes())?;
    Ok(path)
}

/// `claude -p` arguments. The prompt comes on stdin. `mcp_config` is the
/// file from [`write_mcp_config`], `tools` the MCP tools the run may call;
/// without a config (tests, hosted decisions) the run is also in safe mode:
/// no CLAUDE.md, skills, plugins or memory.
pub fn args(mcp_config: Option<&std::path::Path>, tools: &[String], model: Option<&str>) -> Vec<OsString> {
    cli_args(Some(decision_schema().to_string()), SYSTEM, mcp_config, tools, model)
}

/// `claude -p` arguments for a question: as [`args`], with the question's
/// system prompt and no output schema (the answer is the result text).
pub fn ask_args(mcp_config: Option<&std::path::Path>, tools: &[String], model: Option<&str>) -> Vec<OsString> {
    cli_args(None, crate::ask::SYSTEM, mcp_config, tools, model)
}

fn cli_args(schema: Option<String>, system: &str, mcp_config: Option<&std::path::Path>, tools: &[String], model: Option<&str>) -> Vec<OsString> {
    let mut a: Vec<String> = vec!["-p".into(), "--output-format".into(), "stream-json".into(), "--verbose".into(), "--no-session-persistence".into()];
    if let Some(schema) = schema {
        a.push("--json-schema".into());
        a.push(schema);
    }
    a.extend([
        "--system-prompt".into(),
        system.into(),
        // No built-in tools (no shell, no file edits); anything not allowed is refused.
        "--tools".into(),
        String::new(),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--strict-mcp-config".into(),
        "--settings".into(),
        r#"{"disableAllHooks":true}"#.into(),
        // No code-running tools, no user/project settings files, file tools
        // (none are enabled anyway) confined to the empty run dir.
        "--restricted".into(),
    ]);
    if let Some(cfg) = mcp_config {
        a.push("--mcp-config".into());
        a.push(cfg.display().to_string());
        a.push("--allowedTools".into());
        a.push(tools.iter().map(|t| format!("mcp__openpasture__{t}")).collect::<Vec<_>>().join(","));
    }
    if mcp_config.is_none() {
        a.push("--safe-mode".into());
    }
    if let Some(m) = model {
        a.push("--model".into());
        a.push(m.into());
    }
    a.into_iter().map(OsString::from).collect()
}

/// The MCP config for a question's run: this server's brain scope
/// (`/mcp?scope=brain`) with a token that lists and calls `tools` only. The
/// token lives until the returned guard drops, at most the question budget
/// and a minute.
pub fn ask_mcp(ctx: &Ctx, dir: &std::path::Path, tools: &[String]) -> anyhow::Result<(PathBuf, BrainToken)> {
    let tools: Vec<String> = tools.iter().filter(|t| !crate::ask::NEVER.contains(&t.as_str())).cloned().collect();
    let token = ctx.mint_brain_token(crate::ask::BUDGET + Duration::from_secs(60), tools);
    let path = write_mcp_config(dir, &format!("{}/mcp?scope=brain&token={}", ctx.local_url(), token.as_str()))?;
    Ok((path, token))
}

#[derive(Default)]
pub struct Events {
    pub model: Option<String>,
    pub structured: Option<Value>,
    pub result_text: Option<String>,
    pub last_text: Option<String>,
    pub error: Option<String>,
}

impl Events {
    pub fn feed(&mut self, line: &str) -> Vec<String> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return vec![] };
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                self.model = v.get("model").and_then(Value::as_str).map(str::to_owned);
                out.push(format!("Claude started ({})", self.model.as_deref().unwrap_or("default model")));
                if let Some(servers) = v.get("mcp_servers").and_then(Value::as_array) {
                    for s in servers.iter().filter(|s| s.get("name").and_then(Value::as_str) == Some("openpasture")) {
                        let status = s.get("status").and_then(Value::as_str).unwrap_or("unknown");
                        out.push(if status == "connected" { "MCP connected".into() } else { format!("MCP {status}; deciding from the context") });
                    }
                }
            }
            "assistant" => {
                for c in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    match c.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = c.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
                            // Skip the JSON itself; show prose only.
                            if let Some(first) = t.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with('{')) {
                                out.push(snippet(first));
                            }
                            if !t.is_empty() {
                                self.last_text = Some(t);
                            }
                        }
                        Some("tool_use") => {
                            let name = c.get("name").and_then(Value::as_str).unwrap_or_default();
                            if name == "StructuredOutput" {
                                self.structured = c.get("input").cloned();
                                out.push("Answer received".into());
                            } else {
                                out.push(format!("Tool {}", name.trim_start_matches("mcp__openpasture__")));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                if let Some(s) = v.get("structured_output").filter(|s| s.is_object()) {
                    self.structured = Some(s.clone());
                }
                self.result_text = v.get("result").and_then(Value::as_str).map(str::to_owned);
                let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || v.get("subtype").and_then(Value::as_str).is_some_and(|s| s.starts_with("error"));
                if is_error {
                    let why = self
                        .result_text
                        .clone()
                        .filter(|t| !t.is_empty())
                        .or_else(|| v.get("errors").map(|e| e.to_string()))
                        .or_else(|| v.get("subtype").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_else(|| "failed".into());
                    out.push(format!("Error: {}", snippet(&why)));
                    self.error = Some(why);
                } else {
                    let ms = v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
                    out.push(format!("Done in {:.1} s", ms as f64 / 1000.0));
                }
            }
            _ => {}
        }
        out
    }
}

#[async_trait::async_trait]
impl Brain for ClaudeBrain {
    fn id(&self) -> BrainId {
        BrainId::Claude
    }

    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput> {
        let dir = tempfile::Builder::new().prefix("openpasture-claude-").tempdir()?;
        let mcp_config = if req.mcp_url.is_empty() { None } else { Some(write_mcp_config(dir.path(), &req.mcp_url)?) };
        let args = args(mcp_config.as_deref(), req.prompt_tools(), self.model.as_deref());
        let prompt = build_prompt(&req.instructions, &req.context, &decision_schema(), req.prompt_tools());

        let mut events = Events::default();
        let log = req.log.clone();
        let mut on_line = |line: &str| {
            for p in events.feed(line) {
                let _ = log.send(p);
            }
        };
        let out = cli::run_streaming(&self.bin, &args, dir.path(), &[], prompt, self.timeout, &mut on_line).await.context("Claude")?;

        let model = events.model.clone().or_else(|| self.model.clone());
        if let Some(s) = &events.structured {
            return parse::finish(s, &req.context, model).context("Claude");
        }
        let text = events.result_text.clone().filter(|t| !t.trim().is_empty()).or_else(|| events.last_text.clone());
        match (text, &events.error) {
            (_, Some(e)) => bail!("Claude: {}", snippet(e)),
            (Some(t), None) => {
                let mut d = parse::parse_text(&t).context("Claude")?;
                parse::check_paddock(&mut d, &req.context)?;
                d.model = d.model.or(model);
                Ok(d)
            }
            (None, None) => {
                let tail = out.stderr_tail();
                bail!("Claude gave no answer{}", if tail.is_empty() { String::new() } else { format!(": {}", snippet(&tail)) })
            }
        }
    }

    async fn ask(&self, req: AskRequest) -> Result<String, AskError> {
        let dir = tempfile::Builder::new().prefix("openpasture-claude-").tempdir().map_err(anyhow::Error::from)?;
        let tools: Vec<String> = req.offered().into_iter().map(|t| t.name).collect();
        // The token (if any) lives until this run ends.
        let mcp = match &self.ctx {
            Some(ctx) if !tools.is_empty() => Some(ask_mcp(ctx, dir.path(), &tools)?),
            _ => None,
        };
        let named: &[String] = if mcp.is_some() { &tools } else { &[] };
        let args = ask_args(mcp.as_ref().map(|(p, _)| p.as_path()), named, self.model.as_deref());
        let prompt = crate::ask::prompt(&req.question, &req.context, named, req.max_chars);

        let mut events = Events::default();
        let log = req.log.clone();
        let mut on_line = |line: &str| {
            for p in events.feed(line) {
                if let Some(l) = &log {
                    let _ = l.send(p);
                }
            }
        };
        let timeout = self.timeout.min(crate::ask::BUDGET);
        let out = cli::run_streaming(&self.bin, &args, dir.path(), &[], prompt, timeout, &mut on_line).await.context("Claude")?;
        drop(mcp);
        if let Some(e) = &events.error {
            return Err(AskError::Failed(anyhow::anyhow!("Claude: {}", snippet(e))));
        }
        match events.result_text.clone().filter(|t| !t.trim().is_empty()).or_else(|| events.last_text.clone()) {
            Some(t) => crate::ask::answer(&t, req.max_chars),
            None => {
                let tail = out.stderr_tail();
                Err(AskError::Failed(anyhow::anyhow!("Claude gave no answer{}", if tail.is_empty() { String::new() } else { format!(": {}", snippet(&tail)) })))
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::detect::script;
    use crate::fixture;

    const STREAM: &str = r#"{"type":"system","subtype":"init","cwd":"/tmp","tools":["StructuredOutput","mcp__openpasture__get_herd"],"mcp_servers":[{"name":"openpasture","status":"connected"}],"model":"claude-sonnet-5"}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__openpasture__get_herd","input":{}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"{}"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"StructuredOutput","input":{"action":"STAY","to_paddock_id":null,"geometry":null,"reasoning":"Two more days in Home.","confidence":0.66,"need":null}}]}}
{"type":"result","subtype":"success","is_error":false,"duration_ms":4200,"result":"","structured_output":{"action":"STAY","to_paddock_id":null,"geometry":null,"reasoning":"Two more days in Home.","confidence":0.66,"need":null}}"#;

    #[test]
    fn events_to_progress() {
        let mut e = Events::default();
        let lines: Vec<String> = STREAM.lines().flat_map(|l| e.feed(l)).collect();
        assert_eq!(lines, vec!["Claude started (claude-sonnet-5)", "MCP connected", "Tool get_herd", "Answer received", "Done in 4.2 s"]);
        assert_eq!(e.structured.unwrap()["action"], "STAY");
    }

    fn names() -> Vec<String> {
        MCP_TOOLS.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn cli_args() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_mcp_config(dir.path(), "http://127.0.0.1:7878/mcp").unwrap();
        let a: Vec<String> = args(Some(&path), &names(), Some("haiku")).iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(&a[..4], ["-p", "--output-format", "stream-json", "--verbose"]);
        let i = a.iter().position(|s| s == "--tools").unwrap();
        assert_eq!(a[i + 1], "");
        assert!(a.contains(&"--restricted".to_string()));
        assert!(!a.contains(&"--safe-mode".to_string()));
        let i = a.iter().position(|s| s == "--mcp-config").unwrap();
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&a[i + 1]).unwrap()).unwrap();
        assert_eq!(cfg["mcpServers"]["openpasture"]["type"], "http");
        assert!(cfg["mcpServers"]["openpasture"].get("headers").is_none());
        let i = a.iter().position(|s| s == "--allowedTools").unwrap();
        assert!(a[i + 1].split(',').all(|t| t.starts_with("mcp__openpasture__")));
        assert_eq!(a[i + 1].split(',').count(), MCP_TOOLS.len());
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "haiku"));
        let a = args(None, &[], None);
        assert!(!a.iter().any(|s| s == "--mcp-config" || s == "--allowedTools"));
        assert!(a.iter().any(|s| s == "--safe-mode"));
    }

    #[test]
    fn brain_token_goes_in_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = write_mcp_config(dir.path(), "http://10.0.0.2:7878/mcp?scope=brain&token=opb_secret").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg["mcpServers"]["openpasture"]["url"], "http://10.0.0.2:7878/mcp?scope=brain");
        assert_eq!(cfg["mcpServers"]["openpasture"]["headers"]["Authorization"], "Bearer opb_secret");
        let a: Vec<String> = args(Some(&path), &names(), None).iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert!(!a.iter().any(|s| s.contains("opb_secret")));
    }

    #[tokio::test]
    async fn decides_with_fake_cli() {
        let bin = tempfile::tempdir().unwrap();
        let claude = script(bin.path(), "claude", &format!("prompt=$(cat)\ncase \"$prompt\" in *pad_creek*) ;; *) exit 3;; esac\ncat <<'EOF'\n{STREAM}\nEOF"));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest {
            herd_id: "h".into(),
            context: fixture::context(),
            instructions: String::new(),
            mcp_url: "http://x/mcp".into(),
            tools: names(),
            log: tx,
        };
        let out = ClaudeBrain::new(claude, None).decide(req).await.unwrap();
        assert_eq!(out.action, crate::Action::Stay);
        assert_eq!(out.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[tokio::test]
    async fn text_result_and_errors() {
        let bin = tempfile::tempdir().unwrap();
        let claude = script(
            bin.path(),
            "claude",
            r#"cat >/dev/null; printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Here:\n```json\n{\"action\":\"NEEDS_INFO\",\"reasoning\":\"No notes.\",\"confidence\":0.3,\"need\":\"Walk Home\"}\n```"}'"#,
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest {
            herd_id: "h".into(),
            context: fixture::context(),
            instructions: String::new(),
            mcp_url: String::new(),
            tools: vec![],
            log: tx.clone(),
        };
        let out = ClaudeBrain::new(claude, None).decide(req).await.unwrap();
        assert_eq!(out.action, crate::Action::NeedsInfo);
        assert_eq!(out.need.as_deref(), Some("Walk Home"));

        let claude = script(
            bin.path(),
            "claude",
            r#"cat >/dev/null; echo '{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}'; exit 1"#,
        );
        let req =
            DecisionRequest { herd_id: "h".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), tools: vec![], log: tx };
        let err = ClaudeBrain::new(claude, None).decide(req).await.unwrap_err();
        assert!(format!("{err:#}").contains("Not logged in"), "{err:#}");
    }
}
