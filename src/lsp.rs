//! Language Server Protocol client: starts a language server (rust-analyzer,
//! clangd, pyright, …) as a child process and talks JSON-RPC with it over
//! its standard input and output.
//!
//! Everything runs on the GLib main loop with asynchronous reads and
//! writes: no threads, and a busy server never blocks the editor.

use crate::lang::Lang;
use gtk::{gio, glib};
use gtk::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A language server and the languages it serves.
pub struct ServerSpec {
    /// Shown to the user.
    pub name: &'static str,
    pub langs: &'static [Lang],
    /// Commands tried in order: (program, arguments).
    pub commands: &'static [(&'static str, &'static [&'static str])],
    /// Arch package to suggest when nothing is installed.
    pub package: &'static str,
}

pub const SERVERS: &[ServerSpec] = &[
    ServerSpec { name: "rust-analyzer", langs: &[Lang::Rust], commands: &[("rust-analyzer", &[])], package: "rust-analyzer" },
    ServerSpec { name: "clangd", langs: &[Lang::C, Lang::Cpp], commands: &[("clangd", &[])], package: "clang" },
    ServerSpec {
        name: "Python",
        langs: &[Lang::Python],
        commands: &[
            ("basedpyright-langserver", &["--stdio"]),
            ("pyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
            ("jedi-language-server", &[]),
        ],
        package: "pyright",
    },
    ServerSpec {
        name: "typescript-language-server",
        langs: &[Lang::JavaScript, Lang::TypeScript],
        commands: &[("typescript-language-server", &["--stdio"])],
        package: "typescript-language-server",
    },
    ServerSpec { name: "gopls", langs: &[Lang::Go], commands: &[("gopls", &[])], package: "gopls" },
    ServerSpec {
        name: "PHP",
        langs: &[Lang::Php],
        commands: &[("intelephense", &["--stdio"]), ("phpactor", &["language-server"])],
        package: "intelephense (npm install -g intelephense)",
    },
    ServerSpec { name: "jdtls", langs: &[Lang::Java], commands: &[("jdtls", &[])], package: "jdtls (AUR)" },
    ServerSpec {
        name: "bash-language-server",
        langs: &[Lang::Bash],
        commands: &[("bash-language-server", &["start"])],
        package: "bash-language-server",
    },
    ServerSpec { name: "marksman", langs: &[Lang::Markdown], commands: &[("marksman", &["server"])], package: "marksman" },
    ServerSpec { name: "sqls", langs: &[Lang::Sql], commands: &[("sqls", &[])], package: "sqls (AUR)" },
    ServerSpec {
        name: VUE_SERVER,
        langs: &[Lang::Vue],
        commands: &[("vue-language-server", &["--stdio"])],
        package: "vue-language-server typescript-language-server typescript",
    },
];

/// The Vue server works together with the TypeScript one, which loads the
/// Vue plugin and answers the `tsserver/request` of the Vue server.
pub const VUE_SERVER: &str = "Vue";
pub const TS_SERVER: &str = "typescript-language-server";

pub fn spec_named(name: &str) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|s| s.name == name)
}

pub fn spec_for(lang: Lang) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|s| s.langs.contains(&lang))
}

/// The LSP identifier of the language of `path`.
pub fn language_id(lang: Lang, path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match lang {
        Lang::Rust => "rust",
        Lang::C => "c",
        Lang::Cpp => "cpp",
        Lang::Python => "python",
        Lang::JavaScript if ext == "jsx" => "javascriptreact",
        Lang::JavaScript => "javascript",
        Lang::TypeScript if ext == "tsx" => "typescriptreact",
        Lang::TypeScript => "typescript",
        Lang::Go => "go",
        Lang::Php => "php",
        Lang::Java => "java",
        Lang::Bash => "shellscript",
        Lang::Markdown => "markdown",
        Lang::Sql => "sql",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Yaml => "yaml",
        Lang::Vue => "vue",
        Lang::Plain => "plaintext",
    }
}

fn find_in_path(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(cmd)).find(|p| p.is_file()))
}

/// The first installed command of `spec`.
pub fn find_command(spec: &ServerSpec) -> Option<(PathBuf, Vec<String>)> {
    spec.commands
        .iter()
        .find_map(|(cmd, args)| find_in_path(cmd).map(|p| (p, args.iter().map(|a| a.to_string()).collect())))
}

pub fn path_to_uri(path: &Path) -> String {
    glib::filename_to_uri(path, None).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", path.display()))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    glib::filename_from_uri(uri).ok().map(|(p, _)| p)
}

/// Column in UTF-16 code units (as LSP counts) of character `col` of `line`.
pub fn utf16_col(line: &str, col: usize) -> u32 {
    line.chars().take(col).map(|c| c.len_utf16() as u32).sum()
}

/// Character column of UTF-16 column `u16col` of `line`.
pub fn char_col(line: &str, u16col: u32) -> u32 {
    let mut units = 0;
    let mut chars = 0;
    for c in line.chars() {
        if units >= u16col {
            break;
        }
        units += c.len_utf16() as u32;
        chars += 1;
    }
    chars
}

/// A location returned by the server: file, line and UTF-16 column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: u32,
    pub u16col: u32,
}

/// Locations in a `definition` / `references` answer (Location,
/// Location[] or LocationLink[]).
pub fn parse_locations(v: &Value) -> Vec<Location> {
    let one = |l: &Value| -> Option<Location> {
        let uri = l.get("uri").or_else(|| l.get("targetUri"))?.as_str()?;
        let range = l.get("range").or_else(|| l.get("targetSelectionRange"))?;
        let start = range.get("start")?;
        Some(Location {
            path: uri_to_path(uri)?,
            line: start.get("line")?.as_u64()? as u32,
            u16col: start.get("character")?.as_u64()? as u32,
        })
    };
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Severity of a diagnostic (LSP values).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

/// A problem reported by a server: range (lines and UTF-16 columns),
/// severity and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u32,
    pub u16col: u32,
    pub end_line: u32,
    pub end_u16col: u32,
    pub severity: Severity,
    pub message: String,
    /// Who found it (`rustc`, `clang`, `ts`…) and its code, if given.
    pub source: String,
}

/// Parameters of `textDocument/publishDiagnostics`: the document URI and
/// its problems.
pub fn parse_diagnostics(params: &Value) -> Option<(String, Vec<Diagnostic>)> {
    let uri = params.get("uri")?.as_str()?.to_string();
    let pos = |v: Option<&Value>| -> Option<(u32, u32)> {
        let v = v?;
        Some((v.get("line")?.as_u64()? as u32, v.get("character")?.as_u64()? as u32))
    };
    let items = params.get("diagnostics").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let diags = items
        .iter()
        .filter_map(|d| {
            let range = d.get("range")?;
            let (line, u16col) = pos(range.get("start"))?;
            let (end_line, end_u16col) = pos(range.get("end")).unwrap_or((line, u16col));
            let severity = match d.get("severity").and_then(Value::as_u64) {
                Some(2) => Severity::Warning,
                Some(3) => Severity::Info,
                Some(4) => Severity::Hint,
                _ => Severity::Error,
            };
            let mut source = d.get("source").and_then(Value::as_str).unwrap_or("").to_string();
            let code = match d.get("code") {
                Some(Value::String(c)) => c.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            };
            if !code.is_empty() {
                source = if source.is_empty() { code } else { format!("{source} {code}") };
            }
            let mut message = d.get("message")?.as_str()?.trim().to_string();
            // Notes ("expected `i32`, found `&str`", "declared here") on
            // the following lines.
            for r in d.get("relatedInformation").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                if let Some(m) = r.get("message").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()) {
                    message.push('\n');
                    message.push_str(m);
                }
            }
            Some(Diagnostic { line, u16col, end_line, end_u16col, severity, message, source })
        })
        .collect();
    Some((uri, diags))
}

/// Text of a snippet (`${1:name}`, `$0`…) without its placeholders.
pub fn strip_snippet(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                // ${1:default} or ${1|a,b|}: keep the default text.
                let mut depth = 1;
                let mut inner = String::new();
                for n in chars.by_ref() {
                    match n {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    inner.push(n);
                }
                if let Some((_, def)) = inner.split_once(':') {
                    out.push_str(&strip_snippet(def));
                } else if let Some(choices) = inner.split_once('|').map(|x| x.1.trim_end_matches('|')) {
                    out.push_str(choices.split(',').next().unwrap_or(""));
                }
            }
            '$' if chars.peek().is_some_and(|c| c.is_ascii_digit()) => {
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Name of a CompletionItemKind.
pub fn completion_kind(kind: u64) -> &'static str {
    match kind {
        2 => "metodo",
        3 => "funzione",
        4 => "costruttore",
        5 => "campo",
        6 => "variabile",
        7 => "classe",
        8 => "interfaccia",
        9 => "modulo",
        10 => "proprietà",
        12 => "valore",
        13 => "enum",
        14 => "parola chiave",
        15 => "snippet",
        20 => "variante",
        21 => "costante",
        22 => "struct",
        23 => "evento",
        24 => "operatore",
        25 => "tipo",
        _ => "",
    }
}

/// Takes one framed message out of `buf`, if complete.
fn take_message(buf: &mut Vec<u8>) -> Option<Value> {
    loop {
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let headers = String::from_utf8_lossy(&buf[..end]).to_string();
        let len = headers
            .lines()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim().to_string()))
            .and_then(|v| v.parse::<usize>().ok());
        let Some(len) = len else {
            // Not a valid header: drop it.
            buf.drain(..end + 4);
            continue;
        };
        if buf.len() < end + 4 + len {
            return None;
        }
        let body: Vec<u8> = buf.drain(..end + 4 + len).skip(end + 4).collect();
        match serde_json::from_slice(&body) {
            Ok(v) => return Some(v),
            Err(_) => continue,
        }
    }
}

type Reply = Box<dyn FnOnce(Result<Value, String>)>;

pub type NotificationHandler = Box<dyn Fn(&str, &Value)>;

/// What the client tells the editor.
pub struct Events {
    /// The server finished starting.
    pub ready: Box<dyn Fn()>,
    /// Progress text ("indicizzazione 40%"), empty when done.
    pub status: Box<dyn Fn(&str)>,
    /// The server stopped; the text explains why when it failed.
    pub exited: Box<dyn Fn(String)>,
    /// Other notifications from the server (method, params).
    pub notification: NotificationHandler,
}

pub struct Client {
    pub spec: &'static ServerSpec,
    pub root: PathBuf,
    proc: gio::Subprocess,
    stdin: gio::OutputStream,
    next_id: Cell<i64>,
    pending: RefCell<HashMap<i64, Reply>>,
    /// Messages waiting for the answer to `initialize`.
    queued: RefCell<Vec<Value>>,
    outbox: RefCell<VecDeque<Vec<u8>>>,
    writing: Cell<bool>,
    pub ready: Cell<bool>,
    pub stopping: Cell<bool>,
    /// Characters that open the completion list (`.`, `::`…).
    pub trigger_chars: RefCell<Vec<String>>,
    /// Open documents (URI) and their version.
    pub docs: RefCell<HashMap<String, i32>>,
    /// Last lines of the error output (to explain a failure).
    stderr: RefCell<String>,
    /// Running progress operations (token -> title).
    progress: RefCell<HashMap<String, String>>,
    events: Events,
}

impl Client {
    /// Starts `spec` with root folder `root`, extra command line arguments
    /// and initialization options (`Value::Null` for none).
    pub fn start(
        spec: &'static ServerSpec,
        root: &Path,
        extra_args: &[String],
        init_options: Value,
        events: Events,
    ) -> Result<Rc<Client>, String> {
        let (cmd, mut args) = find_command(spec).ok_or_else(|| format!("{} non è installato ({})", spec.name, spec.package))?;
        args.extend(extra_args.iter().cloned());
        let launcher = gio::SubprocessLauncher::new(
            gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
        );
        launcher.set_cwd(root);
        let mut argv: Vec<&std::ffi::OsStr> = vec![cmd.as_os_str()];
        argv.extend(args.iter().map(std::ffi::OsStr::new));
        let proc = launcher.spawn(&argv).map_err(|e| format!("impossibile avviare {}: {e}", spec.name))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (proc.stdin_pipe(), proc.stdout_pipe(), proc.stderr_pipe()) else {
            return Err("pipe non disponibili".into());
        };
        let c = Rc::new(Client {
            spec,
            root: root.to_path_buf(),
            proc,
            stdin,
            next_id: Cell::new(1),
            pending: RefCell::new(HashMap::new()),
            queued: RefCell::new(Vec::new()),
            outbox: RefCell::new(VecDeque::new()),
            writing: Cell::new(false),
            ready: Cell::new(false),
            stopping: Cell::new(false),
            trigger_chars: RefCell::new(Vec::new()),
            docs: RefCell::new(HashMap::new()),
            stderr: RefCell::new(String::new()),
            progress: RefCell::new(HashMap::new()),
            events,
        });
        c.read_loop(stdout);
        c.stderr_loop(stderr);
        c.initialize(init_options);
        Ok(c)
    }

    fn read_loop(self: &Rc<Self>, stdout: gio::InputStream) {
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let bytes = stdout.read_bytes_future(65536, glib::Priority::DEFAULT).await;
                let Some(c) = w.upgrade() else { return };
                match bytes {
                    Ok(b) if !b.is_empty() => {
                        buf.extend_from_slice(&b);
                        while let Some(msg) = take_message(&mut buf) {
                            c.handle(msg);
                        }
                    }
                    _ => {
                        c.finished();
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
                let Some(c) = w.upgrade() else { return };
                match bytes {
                    Ok(b) if !b.is_empty() => {
                        let mut e = c.stderr.borrow_mut();
                        e.push_str(&String::from_utf8_lossy(&b));
                        let len = e.len();
                        if len > 4000 {
                            let mut cut = len - 2000;
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

    /// The server closed its output (it exited).
    fn finished(self: &Rc<Self>) {
        self.ready.set(false);
        let pending: Vec<Reply> = self.pending.borrow_mut().drain().map(|(_, r)| r).collect();
        for reply in pending {
            reply(Err("il server è terminato".into()));
        }
        // Its last error lines arrive just after: wait for them.
        let c = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || c.report_exit());
    }

    fn report_exit(&self) {
        let why = if self.stopping.get() {
            String::new()
        } else {
            let e = self.stderr.borrow();
            let last: Vec<&str> = e.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = last[last.len().saturating_sub(4)..].join("\n");
            let hint = format!("\n\nSe non è installato: pacchetto «{}».", self.spec.package);
            if tail.is_empty() { format!("{} è terminato.{hint}", self.spec.name) } else { format!("{} è terminato:\n{tail}{hint}", self.spec.name) }
        };
        (self.events.exited)(why);
    }

    fn write(self: &Rc<Self>, msg: &Value) {
        let body = msg.to_string();
        let mut data = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        data.extend_from_slice(body.as_bytes());
        self.outbox.borrow_mut().push_back(data);
        if self.writing.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            loop {
                let Some(c) = w.upgrade() else { return };
                let Some(data) = c.outbox.borrow_mut().pop_front() else {
                    c.writing.set(false);
                    return;
                };
                let stdin = c.stdin.clone();
                drop(c);
                if stdin.write_all_future(data, glib::Priority::DEFAULT).await.is_err() {
                    if let Some(c) = w.upgrade() {
                        c.outbox.borrow_mut().clear();
                        c.writing.set(false);
                    }
                    return;
                }
            }
        });
    }

    /// Sends now, or after `initialize` for normal messages.
    fn send(self: &Rc<Self>, msg: Value, early: bool) {
        if self.ready.get() || early {
            self.write(&msg);
        } else {
            self.queued.borrow_mut().push(msg);
        }
    }

    pub fn request(self: &Rc<Self>, method: &str, params: Value, reply: impl FnOnce(Result<Value, String>) + 'static) {
        self.request_inner(method, params, false, Box::new(reply));
    }

    fn request_inner(self: &Rc<Self>, method: &str, params: Value, early: bool, reply: Reply) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.pending.borrow_mut().insert(id, reply);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), early);
    }

    pub fn notify(self: &Rc<Self>, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}), false);
    }

    fn respond(self: &Rc<Self>, id: Value, result: Value) {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn initialize(self: &Rc<Self>, init_options: Value) {
        let root_uri = path_to_uri(&self.root);
        let name = self.root.file_name().map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned());
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "macaco", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": self.root,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "synchronization": {"didSave": true, "dynamicRegistration": false},
                    "completion": {
                        "completionItem": {"snippetSupport": false, "insertReplaceSupport": false},
                        "contextSupport": true
                    },
                    "definition": {"linkSupport": true},
                    "references": {},
                    // Notes ("declared here") stay attached to their problem.
                    "publishDiagnostics": {"relatedInformation": true},
                },
                "workspace": {"configuration": true, "workspaceFolders": true},
                "window": {"workDoneProgress": true},
            },
        });
        let mut params = params;
        if !init_options.is_null() {
            params["initializationOptions"] = init_options;
        }
        let w = Rc::downgrade(self);
        self.request_inner(
            "initialize",
            params,
            true,
            Box::new(move |res| {
                let Some(c) = w.upgrade() else { return };
                let Ok(v) = res else { return };
                if let Some(chars) = v.pointer("/capabilities/completionProvider/triggerCharacters").and_then(Value::as_array) {
                    *c.trigger_chars.borrow_mut() = chars.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                }
                c.write(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
                c.ready.set(true);
                let queued: Vec<Value> = c.queued.borrow_mut().drain(..).collect();
                for m in queued {
                    c.write(&m);
                }
                (c.events.ready)();
            }),
        );
    }

    fn handle(self: &Rc<Self>, msg: Value) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        let id = msg.get("id").cloned();
        match (method, id) {
            // Answer to one of our requests.
            (None, Some(id)) => {
                let Some(reply) = id.as_i64().and_then(|i| self.pending.borrow_mut().remove(&i)) else { return };
                match msg.get("error") {
                    Some(e) => reply(Err(e.get("message").and_then(Value::as_str).unwrap_or("errore").to_string())),
                    None => reply(Ok(msg.get("result").cloned().unwrap_or(Value::Null))),
                }
            }
            // Request from the server: answer so that it does not wait.
            (Some(m), Some(id)) => {
                let result = match m.as_str() {
                    "workspace/configuration" => {
                        let n = msg.pointer("/params/items").and_then(Value::as_array).map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; n])
                    }
                    "workspace/workspaceFolders" => {
                        json!([{"uri": path_to_uri(&self.root), "name": self.root.file_name().map(|n| n.to_string_lossy().into_owned())}])
                    }
                    _ => Value::Null,
                };
                self.respond(id, result);
            }
            // Notifications.
            (Some(m), None) if m == "$/progress" => self.progress_event(msg.get("params").unwrap_or(&Value::Null)),
            (Some(m), None) => (self.events.notification)(&m, msg.get("params").unwrap_or(&Value::Null)),
            _ => {}
        }
    }

    fn progress_event(&self, p: &Value) {
        let token = p.get("token").map(|t| t.to_string()).unwrap_or_default();
        let v = p.get("value").unwrap_or(&Value::Null);
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
        {
            let mut prog = self.progress.borrow_mut();
            match kind {
                "begin" | "report" => {
                    let title = v.get("title").and_then(Value::as_str).map(str::to_string).or_else(|| prog.get(&token).cloned()).unwrap_or_default();
                    let pct = v.get("percentage").and_then(Value::as_u64).map(|p| format!(" {p}%")).unwrap_or_default();
                    let text = format!("{title}{pct}");
                    if kind == "begin" || !prog.contains_key(&token) {
                        prog.insert(token, title);
                    }
                    drop(prog);
                    (self.events.status)(&text);
                    return;
                }
                "end" => {
                    prog.remove(&token);
                }
                _ => return,
            }
        }
        let next = self.progress.borrow().values().next().cloned().unwrap_or_default();
        (self.events.status)(&next);
    }

    // ----------------------------------------------------------- documents

    pub fn did_open(self: &Rc<Self>, uri: &str, language_id: &str, text: &str) {
        if self.docs.borrow().contains_key(uri) {
            self.did_change(uri, text);
            return;
        }
        self.docs.borrow_mut().insert(uri.to_string(), 1);
        self.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}));
    }

    /// Sends the whole text (accepted whatever sync kind the server asks).
    pub fn did_change(self: &Rc<Self>, uri: &str, text: &str) {
        let version = {
            let mut docs = self.docs.borrow_mut();
            let Some(v) = docs.get_mut(uri) else { return };
            *v += 1;
            *v
        };
        self.notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}));
    }

    pub fn did_save(self: &Rc<Self>, uri: &str) {
        if self.docs.borrow().contains_key(uri) {
            self.notify("textDocument/didSave", json!({"textDocument": {"uri": uri}}));
        }
    }

    pub fn did_close(self: &Rc<Self>, uri: &str) {
        if self.docs.borrow_mut().remove(uri).is_some() {
            self.notify("textDocument/didClose", json!({"textDocument": {"uri": uri}}));
        }
    }

    /// Asks the server to exit; it is killed if it does not within 2 s.
    pub fn stop(self: &Rc<Self>) {
        if self.stopping.replace(true) {
            return;
        }
        if self.ready.get() {
            let w = Rc::downgrade(self);
            self.request("shutdown", Value::Null, move |_| {
                if let Some(c) = w.upgrade() {
                    c.write(&json!({"jsonrpc": "2.0", "method": "exit"}));
                }
            });
        } else {
            self.proc.force_exit();
        }
        let proc = self.proc.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
            if !proc.has_exited() {
                proc.force_exit();
            }
        });
    }

    /// Kills the server at once (the editor is closing).
    pub fn kill(&self) {
        self.stopping.set(true);
        self.proc.force_exit();
    }

    /// Process id, for the status and the memory measure.
    pub fn pid(&self) -> Option<String> {
        self.proc.identifier().map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        let mut buf = b"Content-Length: 17\r\n\r\n{\"id\":1,\"a\":\"\xc3\xa8\"}Content-Length: 2\r\n\r\n{}".to_vec();
        let a = take_message(&mut buf).unwrap();
        assert_eq!(a["a"], "è");
        assert_eq!(take_message(&mut buf).unwrap(), json!({}));
        assert!(take_message(&mut buf).is_none());
        let mut part = b"Content-Length: 10\r\n\r\n{\"x\":".to_vec();
        assert!(take_message(&mut part).is_none());
        part.extend_from_slice(b"12}  ");
        assert!(take_message(&mut part).is_some());
    }

    #[test]
    fn positions_and_snippets() {
        assert_eq!(utf16_col("a😀b", 2), 3);
        assert_eq!(char_col("a😀b", 3), 2);
        assert_eq!(char_col("àb", 1), 1);
        assert_eq!(strip_snippet("foo(${1:a}, ${2:b})$0"), "foo(a, b)");
        assert_eq!(strip_snippet("x${1|uno,due|}"), "xuno");
        let locs = parse_locations(&json!([{"targetUri": "file:///tmp/a%20b.rs", "targetSelectionRange": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 8}}}]));
        assert_eq!(locs, vec![Location { path: PathBuf::from("/tmp/a b.rs"), line: 3, u16col: 4 }]);
        assert_eq!(parse_locations(&json!({"uri": "file:///x", "range": {"start": {"line": 1, "character": 2}}})).len(), 1);
    }

    #[test]
    fn diagnostics() {
        let (uri, d) = parse_diagnostics(&json!({"uri": "file:///a.c", "version": 3, "diagnostics": [
            {"range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 9}}, "severity": 1,
             "source": "clang", "code": "expected_semi", "message": "expected ';'\n"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 2, "code": 6133, "message": "unused"},
            {"range": {"start": {"line": 1, "character": 0}}, "message": "no severity",
             "relatedInformation": [{"location": {"uri": "file:///a.c", "range": {}}, "message": "declared here"}]}
        ]}))
        .unwrap();
        assert_eq!(uri, "file:///a.c");
        assert_eq!(d.len(), 3);
        assert_eq!((d[0].severity, d[0].source.as_str(), d[0].message.as_str(), d[0].end_u16col), (Severity::Error, "clang expected_semi", "expected ';'", 9));
        assert_eq!((d[1].severity, d[1].source.as_str()), (Severity::Warning, "6133"));
        assert_eq!((d[2].severity, d[2].end_line, d[2].message.as_str()), (Severity::Error, 1, "no severity\ndeclared here"));
    }
}
