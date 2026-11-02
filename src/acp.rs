//! Client of the Agent Client Protocol (https://agentclientprotocol.com):
//! coding agents such as Claude Code and Codex run as programs speaking
//! JSON-RPC on their standard streams, one message per line.
//!
//! The agents are reached through their ACP adapters, installed apart
//! (like the language servers): nothing is downloaded by the editor.

use gtk::prelude::*;
use gtk::{gio, glib};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Version of the protocol spoken by the editor.
const PROTOCOL_VERSION: u64 = 1;
/// Error code of a request that needs the user to log in first.
pub const AUTH_REQUIRED: i64 = -32000;

/// A coding agent and its ACP adapter.
pub struct AgentSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// Name on the button of the status bar.
    pub short: &'static str,
    /// Command of the adapter (looked for in `PATH`).
    pub command: &'static str,
    /// npm package of the adapter, for the install hint.
    pub package: &'static str,
    /// Command logging in to the agent, run in the terminal.
    pub login: &'static str,
}

pub const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        label: "Claude Code",
        short: "Claude",
        command: "claude-agent-acp",
        package: "@agentclientprotocol/claude-agent-acp",
        login: "claude /login",
    },
    AgentSpec {
        id: "codex",
        label: "Codex",
        short: "Codex",
        command: "codex-acp",
        package: "@zed-industries/codex-acp",
        login: "codex login",
    },
];

/// Error of a request: JSON-RPC code and message.
#[derive(Clone, Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    fn from_value(e: &Value) -> RpcError {
        RpcError {
            code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: e.get("message").and_then(Value::as_str).unwrap_or("errore").to_string(),
        }
    }
}

type Reply = Box<dyn FnOnce(Result<Value, RpcError>)>;
/// Handler of the requests of the agent (method, params, answer).
pub type RequestHandler = Box<dyn Fn(&str, &Value, Responder)>;

/// Answer to a request of the agent (permission, file access).
pub struct Responder {
    agent: Weak<Agent>,
    id: Value,
}

impl Responder {
    pub fn ok(self, result: Value) {
        if let Some(a) = self.agent.upgrade() {
            a.write(&json!({"jsonrpc": "2.0", "id": self.id, "result": result}));
        }
    }

    pub fn err(self, code: i64, message: &str) {
        if let Some(a) = self.agent.upgrade() {
            a.write(&json!({"jsonrpc": "2.0", "id": self.id, "error": {"code": code, "message": message}}));
        }
    }
}

/// What the agent tells the editor.
pub struct Handlers {
    /// A `session/update` notification: its `update` object.
    pub update: Box<dyn Fn(&Value)>,
    /// A request of the agent (method, params); it must be answered.
    pub request: RequestHandler,
    /// The agent stopped; the text explains why when it failed.
    pub exited: Box<dyn Fn(String)>,
}

pub struct Agent {
    pub spec: &'static AgentSpec,
    proc: gio::Subprocess,
    stdin: gio::OutputStream,
    next_id: Cell<i64>,
    pending: RefCell<HashMap<i64, Reply>>,
    outbox: RefCell<VecDeque<Vec<u8>>>,
    writing: Cell<bool>,
    stopping: Cell<bool>,
    /// Last lines of the error output (to explain a failure).
    stderr: RefCell<String>,
    /// Result of `initialize` (capabilities, authentication methods).
    pub info: RefCell<Value>,
    handlers: Handlers,
}

/// The adapter of `spec`, if installed.
pub fn find_command(spec: &AgentSpec) -> Option<PathBuf> {
    crate::proc::find_in_path(spec.command)
}

/// The agents whose adapter is installed.
pub fn installed() -> Vec<&'static AgentSpec> {
    AGENTS.iter().filter(|a| find_command(a).is_some()).collect()
}

/// Splits complete lines off `buf` and parses them.
fn take_messages(buf: &mut Vec<u8>) -> Vec<Value> {
    let Some(last) = buf.iter().rposition(|&b| b == b'\n') else { return Vec::new() };
    let out = buf[..last].split(|&b| b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)).filter_map(|l| serde_json::from_slice(l).ok()).collect();
    buf.drain(..=last);
    out
}

impl Agent {
    /// Starts the adapter of `spec` in `cwd` and initializes it; `ready`
    /// is called with the result (or the error).
    pub fn start(spec: &'static AgentSpec, cwd: &Path, handlers: Handlers, ready: impl FnOnce(Result<(), String>) + 'static) -> Result<Rc<Agent>, String> {
        let cmd = find_command(spec).ok_or_else(|| format!("{} non è installato: npm install -g {}", spec.command, spec.package))?;
        let launcher = gio::SubprocessLauncher::new(
            gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
        );
        launcher.set_cwd(cwd);
        let proc = launcher.spawn(&[cmd.as_os_str()]).map_err(|e| format!("impossibile avviare {}: {e}", spec.command))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (proc.stdin_pipe(), proc.stdout_pipe(), proc.stderr_pipe()) else {
            return Err("pipe non disponibili".into());
        };
        let a = Rc::new(Agent {
            spec,
            proc,
            stdin,
            next_id: Cell::new(0),
            pending: RefCell::new(HashMap::new()),
            outbox: RefCell::new(VecDeque::new()),
            writing: Cell::new(false),
            stopping: Cell::new(false),
            stderr: RefCell::new(String::new()),
            info: RefCell::new(Value::Null),
            handlers,
        });
        a.read_loop(stdout);
        a.stderr_loop(stderr);
        let w = Rc::downgrade(&a);
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": false},
            "clientInfo": {"name": "macaco", "title": "macaco", "version": env!("CARGO_PKG_VERSION")},
        });
        a.request("initialize", params, move |res| {
            let Some(a) = w.upgrade() else { return };
            match res {
                Ok(v) => {
                    let version = v.get("protocolVersion").and_then(Value::as_u64).unwrap_or(0);
                    *a.info.borrow_mut() = v;
                    if version != PROTOCOL_VERSION {
                        ready(Err(format!("{} usa la versione {version} del protocollo (macaco: {PROTOCOL_VERSION})", a.spec.label)));
                    } else {
                        ready(Ok(()));
                    }
                }
                Err(e) => ready(Err(e.message)),
            }
        });
        Ok(a)
    }

    fn read_loop(self: &Rc<Self>, stdout: gio::InputStream) {
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let bytes = stdout.read_bytes_future(65536, glib::Priority::DEFAULT).await;
                let Some(a) = w.upgrade() else { return };
                match bytes {
                    Ok(b) if !b.is_empty() => {
                        buf.extend_from_slice(&b);
                        for msg in take_messages(&mut buf) {
                            a.handle(msg);
                        }
                    }
                    _ => {
                        a.finished();
                        return;
                    }
                }
            }
        });
    }

    fn stderr_loop(self: &Rc<Self>, stderr: gio::InputStream) {
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                let bytes = stderr.read_bytes_future(8192, glib::Priority::LOW).await;
                let Some(a) = w.upgrade() else { return };
                match bytes {
                    Ok(b) if !b.is_empty() => {
                        let mut e = a.stderr.borrow_mut();
                        e.push_str(&String::from_utf8_lossy(&b));
                        if e.len() > 4000 {
                            let mut cut = e.len() - 2000;
                            while !e.is_char_boundary(cut) {
                                cut += 1;
                            }
                            e.drain(..cut);
                        }
                    }
                    _ => return,
                }
            }
        });
    }

    /// The agent closed its output (it exited).
    fn finished(self: &Rc<Self>) {
        let pending: Vec<Reply> = self.pending.borrow_mut().drain().map(|(_, r)| r).collect();
        for reply in pending {
            reply(Err(RpcError { code: 0, message: format!("{} è terminato", self.spec.label) }));
        }
        // Its last error lines arrive just after: wait for them.
        let a = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            let why = if a.stopping.get() {
                String::new()
            } else {
                let e = a.stderr.borrow();
                let last: Vec<&str> = e.lines().filter(|l| !l.trim().is_empty()).collect();
                let tail = last[last.len().saturating_sub(6)..].join("\n");
                if tail.is_empty() { format!("{} è terminato.", a.spec.label) } else { format!("{} è terminato:\n{tail}", a.spec.label) }
            };
            (a.handlers.exited)(why);
        });
    }

    fn handle(self: &Rc<Self>, mut msg: Value) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        let id = msg.get("id").cloned();
        match (method, id) {
            // Answer to one of our requests.
            (None, Some(id)) => {
                let Some(reply) = id.as_i64().and_then(|i| self.pending.borrow_mut().remove(&i)) else { return };
                match msg.get("error") {
                    Some(e) => reply(Err(RpcError::from_value(e))),
                    None => reply(Ok(msg.get_mut("result").map(Value::take).unwrap_or(Value::Null))),
                }
            }
            // Request of the agent.
            (Some(m), Some(id)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                (self.handlers.request)(&m, &params, Responder { agent: Rc::downgrade(self), id });
            }
            (Some(m), None) if m == "session/update" => {
                if let Some(update) = msg.pointer("/params/update") {
                    (self.handlers.update)(update);
                }
            }
            _ => {}
        }
    }

    fn write(self: &Rc<Self>, msg: &Value) {
        // One message per line: serde_json escapes the newlines in strings.
        let Ok(mut data) = serde_json::to_vec(msg) else { return };
        data.push(b'\n');
        self.outbox.borrow_mut().push_back(data);
        if self.writing.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                let Some(a) = w.upgrade() else { return };
                let Some(data) = a.outbox.borrow_mut().pop_front() else {
                    a.writing.set(false);
                    return;
                };
                let stdin = a.stdin.clone();
                drop(a);
                if stdin.write_all_future(data, glib::Priority::DEFAULT).await.is_err() {
                    if let Some(a) = w.upgrade() {
                        a.outbox.borrow_mut().clear();
                        a.writing.set(false);
                    }
                    return;
                }
            }
        });
    }

    pub fn request(self: &Rc<Self>, method: &str, params: Value, reply: impl FnOnce(Result<Value, RpcError>) + 'static) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.pending.borrow_mut().insert(id, Box::new(reply));
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    pub fn notify(self: &Rc<Self>, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Authentication methods the agent runs itself (`authenticate`):
    /// (id, name). Those needing a terminal or environment variables are
    /// left out.
    pub fn auth_methods(&self) -> Vec<(String, String)> {
        let info = self.info.borrow();
        let Some(list) = info.get("authMethods").and_then(Value::as_array) else { return Vec::new() };
        list.iter()
            .filter(|m| m.get("type").and_then(Value::as_str).is_none_or(|t| t == "agent"))
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                let name = m.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
                Some((id, name))
            })
            .collect()
    }

    pub fn stop(&self) {
        self.stopping.set(true);
        self.proc.force_exit();
    }

    /// Process id, for the memory measure.
    pub fn pid(&self) -> Option<String> {
        self.proc.identifier().map(|s| s.to_string())
    }
}

// ------------------------------------------------------------- helpers

/// Text of a content block (`{"type": "text", "text": …}`), if any.
pub fn block_text(block: &Value) -> Option<&str> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block.get("text").and_then(Value::as_str),
        Some("resource") => block.pointer("/resource/text").and_then(Value::as_str),
        Some("resource_link") => block.get("uri").and_then(Value::as_str),
        _ => None,
    }
}

/// `text` from line `line` (1-based), at most `limit` lines.
pub fn slice_lines(text: &str, line: Option<u64>, limit: Option<u64>) -> String {
    if line.is_none() && limit.is_none() {
        return text.to_string();
    }
    let skip = line.map_or(0, |l| l.saturating_sub(1) as usize);
    let take = limit.map_or(usize::MAX, |l| l as usize);
    let mut out = String::new();
    for l in text.split_inclusive('\n').skip(skip).take(take) {
        out.push_str(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        let mut buf = b"{\"id\":1}\n\n{\"a\":\"\xc3\xa8\"}\n{\"incompl".to_vec();
        let m = take_messages(&mut buf);
        assert_eq!(m.len(), 2);
        assert_eq!(m[1]["a"], "è");
        assert_eq!(buf, b"{\"incompl");
        buf.extend_from_slice(b"ete\":2}\n");
        assert_eq!(take_messages(&mut buf)[0]["incomplete"], 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn lines() {
        let t = "a\nb\nc\n";
        assert_eq!(slice_lines(t, None, None), t);
        assert_eq!(slice_lines(t, Some(2), None), "b\nc\n");
        assert_eq!(slice_lines(t, Some(1), Some(2)), "a\nb\n");
        assert_eq!(slice_lines(t, Some(9), Some(1)), "");
    }

    #[test]
    fn specs() {
        assert!(AGENTS.iter().any(|a| a.id == "claude") && AGENTS.iter().any(|a| a.id == "codex"));
        assert_eq!(block_text(&json!({"type": "text", "text": "x"})), Some("x"));
    }
}
