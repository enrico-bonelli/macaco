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
        langs: &[Lang::JavaScript, Lang::TypeScript, Lang::Tsx],
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
        name: "C#",
        langs: &[Lang::CSharp],
        commands: &[("csharp-ls", &[]), ("OmniSharp", &["-lsp"]), ("omnisharp", &["-lsp"])],
        package: "csharp-ls (dotnet tool install -g csharp-ls) o omnisharp-roslyn (AUR)",
    },
    ServerSpec {
        name: "Perl Navigator",
        langs: &[Lang::Perl],
        commands: &[("perlnavigator", &["--stdio"])],
        package: "perlnavigator (npm install -g perlnavigator-server)",
    },
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
        Lang::Tsx => "typescriptreact",
        Lang::CSharp => "csharp",
        Lang::Perl => "perl",
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

/// The first installed command of `spec`.
pub fn find_command(spec: &ServerSpec) -> Option<(PathBuf, Vec<String>)> {
    spec.commands
        .iter()
        .find_map(|(cmd, args)| crate::proc::find_in_path(cmd).map(|p| (p, args.iter().map(|a| a.to_string()).collect())))
}

pub fn path_to_uri(path: &Path) -> String {
    glib::filename_to_uri(path, None).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", path.display()))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    glib::filename_from_uri(uri).ok().map(|(p, _)| p)
}

/// Length of `s` in UTF-16 code units.
pub fn utf16_len(s: &str) -> u32 {
    if s.is_ascii() {
        return s.len() as u32;
    }
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Column in UTF-16 code units (as LSP counts) of character `col` of `line`.
pub fn utf16_col(line: &str, col: usize) -> u32 {
    if line.is_ascii() {
        return col.min(line.len()) as u32;
    }
    line.chars().take(col).map(|c| c.len_utf16() as u32).sum()
}

/// An incremental edit in LSP UTF-16 coordinates (`start == end` for insertions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub line: u32,
    pub u16col: u32,
    pub end_line: u32,
    pub end_u16col: u32,
    pub text: String,
}

/// Appends `c` to `changes`, merging consecutive single-line insertions or
/// backward deletions into one event.
pub fn push_change(changes: &mut Vec<Change>, c: Change) {
    if let Some(last) = changes.last_mut() {
        let last_ins = last.line == last.end_line && last.u16col == last.end_u16col;
        let new_ins = c.line == c.end_line && c.u16col == c.end_u16col;
        if last_ins
            && new_ins
            && c.line == last.line
            && !last.text.contains(['\n', '\r'])
            && !c.text.contains(['\n', '\r'])
            && c.u16col == last.u16col + utf16_len(&last.text)
        {
            last.text.push_str(&c.text);
            return;
        }
        if last.text.is_empty() && c.text.is_empty() && c.end_line == last.line && c.end_u16col == last.u16col {
            last.line = c.line;
            last.u16col = c.u16col;
            return;
        }
    }
    changes.push(c);
}

/// `TextDocumentSyncKind` from an `initialize` result (`0`: None, `1`: Full, `2`: Incremental).
pub fn parse_sync_kind(init_result: &Value) -> u8 {
    let sync = init_result.pointer("/capabilities/textDocumentSync");
    sync.and_then(|s| s.as_u64().or_else(|| s.get("change").and_then(Value::as_u64)))
        .filter(|&k| k <= 2)
        .map_or(1, |k| k as u8)
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
        let parsed = serde_json::from_slice(&buf[end + 4..end + 4 + len]);
        buf.drain(..end + 4 + len);
        if let Ok(v) = parsed {
            return Some(v);
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
    /// `TextDocumentSyncKind` negotiated in `initialize` (default `1` = Full, `2` = Incremental).
    sync_kind: Cell<u8>,
    /// Open documents (URI) and their version.
    pub docs: RefCell<HashMap<String, i32>>,
    /// Last lines of the error output (to explain a failure).
    stderr: RefCell<String>,
    /// Running progress operations (token -> title).
    progress: RefCell<HashMap<String, String>>,
    /// When the status was last shown for a progress report (indexing
    /// sends hundreds of them).
    progress_shown: std::cell::Cell<Option<std::time::Instant>>,
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
            sync_kind: Cell::new(1),
            docs: RefCell::new(HashMap::new()),
            stderr: RefCell::new(String::new()),
            progress: RefCell::new(HashMap::new()),
            progress_shown: std::cell::Cell::new(None),
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
        // The body is serialized after room for the header, which is then
        // written in front of it: one buffer, no copy of the (whole) text.
        const ROOM: usize = 40;
        let mut data = vec![b' '; ROOM];
        if serde_json::to_writer(&mut data, msg).is_err() {
            return;
        }
        let header = format!("Content-Length: {}\r\n\r\n", data.len() - ROOM);
        let start = ROOM - header.len();
        data[start..ROOM].copy_from_slice(header.as_bytes());
        data.drain(..start);
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

    /// Sends a request; returns its id (for `cancel`).
    pub fn request(self: &Rc<Self>, method: &str, params: Value, reply: impl FnOnce(Result<Value, String>) + 'static) -> i64 {
        self.request_inner(method, params, false, Box::new(reply))
    }

    fn request_inner(self: &Rc<Self>, method: &str, params: Value, early: bool, reply: Reply) -> i64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.pending.borrow_mut().insert(id, reply);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), early);
        id
    }

    /// Request `id` is no more needed: the server can stop working on it
    /// and its answer is ignored.
    pub fn cancel(self: &Rc<Self>, id: i64) {
        if self.pending.borrow_mut().remove(&id).is_some() {
            self.notify("$/cancelRequest", json!({"id": id}));
        }
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
                c.sync_kind.set(parse_sync_kind(&v));
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

    fn handle(self: &Rc<Self>, mut msg: Value) {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
        let id = msg.get("id").cloned();
        match (method, id) {
            // Answer to one of our requests.
            (None, Some(id)) => {
                let Some(reply) = id.as_i64().and_then(|i| self.pending.borrow_mut().remove(&i)) else { return };
                match msg.get("error") {
                    Some(e) => reply(Err(e.get("message").and_then(Value::as_str).unwrap_or("errore").to_string())),
                    // Taken, not cloned: completion lists can be large.
                    None => reply(Ok(msg.get_mut("result").map(Value::take).unwrap_or(Value::Null))),
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
        // Reports at most five times a second (begin and end always).
        const REPORT_EVERY: std::time::Duration = std::time::Duration::from_millis(200);
        if kind == "report" && self.progress_shown.get().is_some_and(|t| t.elapsed() < REPORT_EVERY) {
            return;
        }
        {
            let mut prog = self.progress.borrow_mut();
            match kind {
                "begin" | "report" => {
                    self.progress_shown.set(Some(std::time::Instant::now()));
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

    /// Whether the server accepts incremental `didChange` ranges (`TextDocumentSyncKind::Incremental`).
    pub fn is_incremental(&self) -> bool {
        self.sync_kind.get() == 2
    }

    pub fn did_open(self: &Rc<Self>, uri: &str, language_id: &str, text: impl Into<String>) {
        if self.docs.borrow().contains_key(uri) {
            self.did_change(uri, text);
            return;
        }
        let text: String = text.into();
        self.docs.borrow_mut().insert(uri.to_string(), 1);
        self.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}));
    }

    /// Sends the current text of a document: a change if the server has it,
    /// else (when the server is ready) an open.
    pub fn sync_text(self: &Rc<Self>, uri: &str, language_id: &str, text: impl Into<String>) {
        if self.docs.borrow().contains_key(uri) {
            self.did_change(uri, text);
        } else if self.ready.get() {
            self.did_open(uri, language_id, text);
        }
    }

    /// Sends the whole text (accepted whatever sync kind the server asks).
    pub fn did_change(self: &Rc<Self>, uri: &str, text: impl Into<String>) {
        if self.sync_kind.get() == 0 {
            return;
        }
        let version = {
            let mut docs = self.docs.borrow_mut();
            let Some(v) = docs.get_mut(uri) else { return };
            *v += 1;
            *v
        };
        let text: String = text.into();
        self.notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}));
    }

    /// Sends incremental ranged changes (`TextDocumentSyncKind::Incremental`).
    pub fn did_change_incremental(self: &Rc<Self>, uri: &str, changes: &[Change]) {
        if changes.is_empty() || self.sync_kind.get() == 0 {
            return;
        }
        let version = {
            let mut docs = self.docs.borrow_mut();
            let Some(v) = docs.get_mut(uri) else { return };
            *v += 1;
            *v
        };
        let events: Vec<Value> = changes
            .iter()
            .map(|c| {
                json!({
                    "range": {
                        "start": {"line": c.line, "character": c.u16col},
                        "end": {"line": c.end_line, "character": c.end_u16col},
                    },
                    "text": c.text.as_str(),
                })
            })
            .collect();
        self.notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": events}));
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

    /// Reference UTF-16 incremental applier matching LSP 3.17 `contentChanges`.
    fn apply_lsp_changes(doc: &mut String, changes: &[Change]) {
        fn offset_of(s: &str, line: u32, u16col: u32) -> usize {
            let mut cur_line = 0;
            let mut line_start = 0;
            for (i, b) in s.bytes().enumerate() {
                if cur_line == line {
                    break;
                }
                if b == b'\n' {
                    cur_line += 1;
                    line_start = i + 1;
                }
            }
            let rest = &s[line_start..];
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let slice = &rest[..line_end];
            let ch_col = char_col(slice, u16col) as usize;
            let byte_in_line = slice.char_indices().nth(ch_col).map_or(slice.len(), |(b, _)| b);
            line_start + byte_in_line
        }
        for c in changes {
            let a = offset_of(doc, c.line, c.u16col);
            let b = offset_of(doc, c.end_line, c.end_u16col);
            doc.replace_range(a..b, &c.text);
        }
    }

    #[test]
    fn incremental_sync_and_coalescing() {
        assert_eq!(parse_sync_kind(&json!({"capabilities": {"textDocumentSync": 2}})), 2);
        assert_eq!(parse_sync_kind(&json!({"capabilities": {"textDocumentSync": {"change": 2, "openClose": true}}})), 2);
        assert_eq!(parse_sync_kind(&json!({"capabilities": {"textDocumentSync": {"openClose": true}}})), 1);
        assert_eq!(parse_sync_kind(&json!({"capabilities": {"textDocumentSync": 0}})), 0);
        assert_eq!(parse_sync_kind(&json!({})), 1);

        let mut doc = "let x = \"a😀b\";\nfn main() {}\n".to_string();
        let mut edits = Vec::new();
        // Type "12" + "😀" + "!" consecutively after "a😀" (col 11 in UTF-16: `let x = "a` = 10 + `😀` = 2 -> 12).
        push_change(&mut edits, Change { line: 0, u16col: 12, end_line: 0, end_u16col: 12, text: "1".into() });
        push_change(&mut edits, Change { line: 0, u16col: 13, end_line: 0, end_u16col: 13, text: "2".into() });
        push_change(&mut edits, Change { line: 0, u16col: 14, end_line: 0, end_u16col: 14, text: "😀".into() });
        push_change(&mut edits, Change { line: 0, u16col: 16, end_line: 0, end_u16col: 16, text: "!".into() });
        // All four single-line insertions coalesce into one event.
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].text, "12😀!");

        // Backspace across newline: delete `f` on line 1, then `\n` at end of line 0.
        push_change(&mut edits, Change { line: 1, u16col: 0, end_line: 1, end_u16col: 1, text: String::new() });
        push_change(&mut edits, Change { line: 0, u16col: 20, end_line: 1, end_u16col: 0, text: String::new() });
        assert_eq!(edits.len(), 2);
        assert_eq!((edits[1].line, edits[1].u16col, edits[1].end_line, edits[1].end_u16col), (0, 20, 1, 1));

        apply_lsp_changes(&mut doc, &edits);
        assert_eq!(doc, "let x = \"a😀12😀!b\";n main() {}\n");
    }
}
