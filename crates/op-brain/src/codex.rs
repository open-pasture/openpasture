//! The Codex CLI brain: `codex exec` as the user signed it in (their ChatGPT
//! plan). Prompt on stdin, answer shaped by `--output-schema`, progress from
//! the `--json` event stream, read-only sandbox in an empty temp dir, our MCP
//! server attached. Tested with codex-cli 0.157.
//!
//! Containment: the read-only sandbox still lets commands read any file, so
//! the tools that run commands or read files (shell, unified exec, image
//! viewer, browser, computer use, apps, plugins, sub-agents, memories, web
//! search) are switched off, HOME is the empty run dir, and only CODEX_HOME
//! (for sign-in) points at the user's Codex folder. Code mode stays on:
//! Codex 0.157 calls MCP tools through it, and its JS runtime has no file or
//! network access. `apply_patch` can't be switched off; in the read-only
//! sandbox it can't write and only reports whether given lines match. Codex
//! adds tools between versions, so this is best effort, and `/v1/decide`
//! refuses to serve with Codex.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use op_core::BrainId;
use serde_json::Value;

use crate::parse::{self, snippet};
use crate::prompt::build_full_prompt;
use crate::{Brain, DecisionOutput, DecisionRequest, cli, decision_schema};

pub const TIMEOUT: Duration = Duration::from_secs(300);

pub struct CodexBrain {
    bin: PathBuf,
    model: Option<String>,
    pub timeout: Duration,
}

impl CodexBrain {
    pub fn new(bin: PathBuf, model: Option<String>) -> Self {
        Self { bin, model, timeout: TIMEOUT }
    }
}

/// `codex exec` arguments. The prompt comes on stdin (`-`).
/// Codex features that run commands, read files or reach outside the run.
/// Set with `-c features.<name>=false`, which unknown names ignore (older or
/// newer CLIs), where `--disable` would fail.
pub const DISABLED_FEATURES: [&str; 13] = [
    "shell_tool",
    "unified_exec",
    "shell_snapshot",
    "view_image",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "apps",
    "plugins",
    "multi_agent",
    "multi_agent_v2",
    "memories",
    "image_generation",
];

/// Env var Codex reads the MCP bearer token from (never argv).
pub const MCP_TOKEN_ENV: &str = "OPENPASTURE_MCP_TOKEN";

/// `mcp_url` must not carry a token; pass `mcp_token` to send one from
/// [`MCP_TOKEN_ENV`].
pub fn args(dir: &std::path::Path, schema: &std::path::Path, last: &std::path::Path, mcp_url: &str, mcp_token: bool, model: Option<&str>) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--ephemeral",
        // The user's own MCP servers and rules stay out; sign-in still comes from CODEX_HOME.
        "--ignore-user-config",
        "--ignore-rules",
        "--sandbox",
        "read-only",
        "--color",
        "never",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    a.push("--output-schema".into());
    a.push(schema.into());
    a.push("--output-last-message".into());
    a.push(last.into());
    a.push("--cd".into());
    a.push(dir.into());
    for f in DISABLED_FEATURES {
        a.push("-c".into());
        a.push(format!("features.{f}=false").into());
    }
    a.push("-c".into());
    a.push("web_search=\"disabled\"".into());
    if !mcp_url.is_empty() {
        a.push("-c".into());
        a.push(format!("mcp_servers.openpasture.url=\"{}\"", toml_escape(mcp_url)).into());
        if mcp_token {
            a.push("-c".into());
            a.push(format!("mcp_servers.openpasture.bearer_token_env_var=\"{MCP_TOKEN_ENV}\"").into());
        }
    }
    if let Some(m) = model {
        a.push("--model".into());
        a.push(m.into());
    }
    a.push("-".into());
    a
}

/// Where Codex keeps its sign-in: `CODEX_HOME`, else `~/.codex`.
fn codex_home() -> String {
    std::env::var("CODEX_HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| std::env::var("HOME").map(|h| format!("{h}/.codex")).unwrap_or_else(|_| ".codex".into()))
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What the event stream told us.
#[derive(Default)]
pub struct Events {
    pub last_message: Option<String>,
    pub error: Option<String>,
}

impl Events {
    /// Read one JSONL event; returns a progress line worth showing.
    pub fn feed(&mut self, line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        let item = v.get("item");
        let item_type = item.and_then(|i| i.get("type")).and_then(Value::as_str).unwrap_or_default();
        let text = |k: &str| item.and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "thread.started" => Some("Codex started".into()),
            "item.started" => match item_type {
                "mcp_tool_call" => Some(format!("Tool {}", text("tool"))),
                "command_execution" => Some(format!("Running {}", snippet(&text("command")))),
                "web_search" => Some(format!("Searching {}", snippet(&text("query")))),
                _ => None,
            },
            "item.completed" => match item_type {
                "agent_message" => {
                    self.last_message = Some(text("text"));
                    Some("Answer received".into())
                }
                "reasoning" => {
                    let t = text("text");
                    let first = t.lines().map(|l| l.trim().trim_matches('*')).find(|l| !l.is_empty())?.to_owned();
                    Some(snippet(&first))
                }
                "mcp_tool_call" if item.and_then(|i| i.get("status")).and_then(Value::as_str) == Some("failed") => {
                    Some(format!("Tool {} failed", text("tool")))
                }
                "error" => {
                    let m = text("message");
                    Some(format!("Warning: {}", snippet(&m)))
                }
                _ => None,
            },
            "turn.failed" => {
                let m = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("turn failed").to_owned();
                self.error = Some(m.clone());
                Some(format!("Error: {}", snippet(&m)))
            }
            "error" => {
                let m = v.get("message").and_then(Value::as_str).unwrap_or("error").to_owned();
                self.error = Some(m.clone());
                Some(format!("Error: {}", snippet(&m)))
            }
            "turn.completed" => {
                let u = v.get("usage");
                let n = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
                Some(format!("Done ({} tokens in, {} out)", n("input_tokens"), n("output_tokens")))
            }
            _ => None,
        }
    }
}

#[async_trait::async_trait]
impl Brain for CodexBrain {
    fn id(&self) -> BrainId {
        BrainId::Codex
    }

    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput> {
        let dir = tempfile::Builder::new().prefix("openpasture-codex-").tempdir()?;
        let schema = dir.path().join("decision.schema.json");
        std::fs::write(&schema, serde_json::to_vec_pretty(&decision_schema())?)?;
        let last = dir.path().join("last-message.txt");
        let (mcp_url, mcp_token) = crate::split_mcp_token(&req.mcp_url);
        let args = args(dir.path(), &schema, &last, &mcp_url, mcp_token.is_some(), self.model.as_deref());
        let prompt = build_full_prompt(&req.instructions, &req.context, &decision_schema(), !req.mcp_url.is_empty());
        // HOME is the empty run dir; sign-in still comes from the real CODEX_HOME.
        let mut env = vec![("HOME".to_owned(), dir.path().display().to_string()), ("CODEX_HOME".to_owned(), codex_home())];
        if let Some(t) = mcp_token {
            env.push((MCP_TOKEN_ENV.to_owned(), t));
        }

        let mut events = Events::default();
        let log = req.log.clone();
        let mut on_line = |line: &str| {
            if let Some(p) = events.feed(line) {
                let _ = log.send(p);
            }
        };
        let out = cli::run_streaming(&self.bin, &args, dir.path(), &env, prompt, self.timeout, &mut on_line).await.context("Codex")?;

        let text = events.last_message.clone().filter(|t| !t.is_empty()).or_else(|| std::fs::read_to_string(&last).ok().filter(|t| !t.trim().is_empty()));
        let Some(text) = text else {
            let why = events.error.clone().unwrap_or_else(|| out.stderr_tail());
            bail!("Codex gave no answer{}", if why.is_empty() { String::new() } else { format!(": {}", snippet(&why)) });
        };
        let mut decision = parse::parse_text(&text).context("Codex")?;
        parse::check_paddock(&mut decision, &req.context)?;
        decision.model = decision.model.or_else(|| self.model.clone());
        Ok(decision)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::detect::script;
    use crate::fixture;

    const STREAM: &str = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"**Checking paddock rest**\n\nCreek has rested longest."}}
{"type":"item.started","item":{"id":"item_1","type":"mcp_tool_call","server":"openpasture","tool":"get_signals","status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"{\"action\":\"MOVE\",\"to_paddock_id\":\"pad_creek\",\"geometry\":null,\"reasoning\":\"Home is short; Creek rested 34 days.\",\"confidence\":0.72,\"need\":null}"}}
{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":80}}"#;

    #[test]
    fn events_to_progress() {
        let mut e = Events::default();
        let lines: Vec<String> = STREAM.lines().filter_map(|l| e.feed(l)).collect();
        assert_eq!(lines, vec!["Codex started", "Checking paddock rest", "Tool get_signals", "Answer received", "Done (1200 tokens in, 80 out)"]);
        assert!(e.last_message.unwrap().contains("pad_creek"));
        let mut e = Events::default();
        e.feed(r#"{"type":"turn.failed","error":{"message":"usage limit reached"}}"#);
        assert_eq!(e.error.as_deref(), Some("usage limit reached"));
    }

    #[test]
    fn exec_args() {
        let d = std::path::Path::new("/tmp/x");
        let a = args(d, &d.join("s.json"), &d.join("l.txt"), "http://127.0.0.1:7878/mcp?scope=brain", false, Some("gpt-5.5"));
        let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        let joined = a.join(" ");
        assert!(joined.starts_with("exec --json --skip-git-repo-check"));
        assert!(joined.contains("--sandbox read-only"));
        assert!(joined.contains("--output-schema /tmp/x/s.json"));
        assert!(a.contains(&"mcp_servers.openpasture.url=\"http://127.0.0.1:7878/mcp?scope=brain\"".to_string()));
        assert!(joined.contains("--model gpt-5.5"));
        assert_eq!(a.last().unwrap(), "-");
        // Containment: no shell, exec, file viewer or web search.
        for f in ["shell_tool", "unified_exec", "view_image"] {
            assert!(a.contains(&format!("features.{f}=false")), "{f}");
        }
        assert!(a.contains(&"web_search=\"disabled\"".to_string()));
        assert!(!joined.contains("bearer_token_env_var"));
        let a = args(d, &d.join("s.json"), &d.join("l.txt"), "", false, None);
        assert!(!a.iter().any(|s| s.to_string_lossy().starts_with("mcp_servers") || s == "--model"));
        // A token goes through the environment, not argv.
        let a: Vec<String> = args(d, &d.join("s.json"), &d.join("l.txt"), "http://10.0.0.2:7878/mcp?scope=brain", true, None)
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(a.contains(&format!("mcp_servers.openpasture.bearer_token_env_var=\"{MCP_TOKEN_ENV}\"")));
    }

    fn fake_codex(dir: &std::path::Path, stream: &str) -> PathBuf {
        // Checks the prompt arrives on stdin and the schema file exists, then streams.
        let body = format!(
            r#"prompt=$(cat)
case "$prompt" in *pad_creek*) ;; *) echo "no context in prompt" >&2; exit 3;; esac
schema=""; while [ $# -gt 0 ]; do [ "$1" = "--output-schema" ] && schema="$2"; shift; done
[ -f "$schema" ] || {{ echo "no schema" >&2; exit 4; }}
cat <<'EOF'
{stream}
EOF"#
        );
        script(dir, "codex", &body)
    }

    #[tokio::test]
    async fn decides_with_fake_cli() {
        let bin = tempfile::tempdir().unwrap();
        let codex = fake_codex(bin.path(), STREAM);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest {
            herd_id: "herd_test".into(),
            context: fixture::context(),
            instructions: "Decide.".into(),
            mcp_url: "http://127.0.0.1:1/mcp".into(),
            log: tx,
        };
        let out = CodexBrain::new(codex, Some("gpt-5.5".into())).decide(req).await.unwrap();
        assert_eq!(out.action, crate::Action::Move);
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_creek"));
        assert_eq!(out.confidence, 0.72);
        assert_eq!(out.model.as_deref(), Some("gpt-5.5"));
        let mut lines = Vec::new();
        while let Ok(l) = rx.try_recv() {
            lines.push(l);
        }
        assert!(lines.contains(&"Tool get_signals".to_string()), "{lines:?}");
    }

    #[tokio::test]
    async fn failures_are_readable() {
        let bin = tempfile::tempdir().unwrap();
        let codex = fake_codex(bin.path(), r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit."}}"#);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest { herd_id: "h".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx };
        let err = CodexBrain::new(codex, None).decide(req).await.unwrap_err();
        assert!(format!("{err:#}").contains("usage limit"), "{err:#}");
    }

    #[tokio::test]
    async fn timeout_kills() {
        let bin = tempfile::tempdir().unwrap();
        let codex = script(bin.path(), "codex", "cat >/dev/null; sleep 30");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest { herd_id: "h".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx };
        let mut brain = CodexBrain::new(codex, None);
        brain.timeout = Duration::from_millis(500);
        let t = std::time::Instant::now();
        let err = brain.decide(req).await.unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(format!("{err:#}").contains("timed out"));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let bin = tempfile::tempdir().unwrap();
        let pidfile = bin.path().join("grandchild.pid");
        let codex = script(bin.path(), "codex", &format!("cat >/dev/null; sleep 30 & echo $! > {}; wait", pidfile.display()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest { herd_id: "h".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx };
        let mut brain = CodexBrain::new(codex, None);
        brain.timeout = Duration::from_secs(3);
        let err = brain.decide(req).await.unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the CLI's own children die with it");
    }

    #[tokio::test]
    async fn odd_bytes_on_stdout_are_tolerated() {
        let bin = tempfile::tempdir().unwrap();
        let body = format!("cat >/dev/null; printf 'garbage \\377\\376 line\\n'; cat <<'EOF'\n{STREAM}\nEOF");
        let codex = script(bin.path(), "codex", &body);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest { herd_id: "h".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx };
        let out = CodexBrain::new(codex, None).decide(req).await.unwrap();
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_creek"));
    }
}
