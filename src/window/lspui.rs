//! Language servers in the working folder (opt-in per folder): started when
//! a document of their language is opened, kept in sync with the open
//! documents and stopped when the folder is closed. Definitions, references
//! and completions come from them when they answer, otherwise from the
//! symbols of `inspect`.

use super::Editor;
use super::inspect::Item;
use crate::config;
use crate::doc::Doc;
use crate::lang::Lang;
use crate::finder::{self, TextMatch, TextResults};
use crate::lsp::{self, Client, Location};
use gtk::prelude::*;
use gtk::{gio, glib};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Changes are sent to the server this long after the last key press.
const CHANGE_DELAY: Duration = Duration::from_millis(300);
/// Completion rows from the server.
const MAX_ITEMS: usize = 80;
/// Above this many buffered edits (or bytes in one insertion), send the whole text.
const MAX_INCREMENTAL_EDITS: usize = 256;
const MAX_INCREMENTAL_TEXT: usize = 65_536;

/// Unsent edits of a document.
pub(super) enum PendingChanges {
    Incremental(Vec<lsp::Change>),
    Full,
}

/// LSP position `(line, UTF-16 column)` of `it` in `buf`.
fn iter_position(buf: &gtk::TextBuffer, it: &gtk::TextIter) -> (u32, u32) {
    let col = it.line_offset() as usize;
    if col == 0 {
        return (it.line() as u32, 0);
    }
    // Pure ASCII up to `it`: 1 byte == 1 char == 1 UTF-16 code unit.
    if it.line_index() as usize == col {
        return (it.line() as u32, col as u32);
    }
    let mut start = *it;
    start.set_line_offset(0);
    let line = buf.text(&start, it, true);
    (it.line() as u32, lsp::utf16_col(&line, col))
}

/// LSP position of the cursor of `d`: (line, UTF-16 column).
fn cursor_position(d: &Doc) -> (u32, u32) {
    iter_position(&d.buffer, &d.cursor_iter())
}

/// Whether the cursor of the Vue document `d` is in its `<script>` block.
fn in_vue_script(d: &Doc) -> bool {
    matches!(super::Editor::lang_at(d, &d.cursor_iter()), Lang::JavaScript | Lang::TypeScript)
}

/// Folder of the installed Vue language server: the Vue TypeScript plugin
/// is found from there.
fn vue_plugin_location() -> Option<String> {
    let spec = lsp::spec_named(lsp::VUE_SERVER)?;
    let (cmd, _) = lsp::find_command(spec)?;
    // …/node_modules/@vue/language-server/bin/vue-language-server.js
    let real = std::fs::canonicalize(cmd).ok()?;
    let pkg = real.parent()?.parent()?;
    Some(pkg.to_string_lossy().into_owned())
}

/// Text of line `n` of `path`: from the open document if any.
fn line_of(ed: &Editor, path: &Path, n: u32) -> String {
    let doc = ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path) && d.is_loaded()).cloned();
    match doc {
        Some(d) => d.line_text(n as i32),
        None => finder::read_text(path).and_then(|t| t.lines().nth(n as usize).map(str::to_string)).unwrap_or_default(),
    }
}

impl Editor {
    pub(super) fn setup_lsp_actions(self: &Rc<Self>) {
        let a = gio::SimpleAction::new_stateful("lsp-folder", None, &false.to_variant());
        let ed = self.clone();
        a.connect_activate(move |a, _| {
            let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            ed.set_lsp_folder(on);
        });
        self.win.add_action(&a);
    }

    /// Whether the servers are enabled for the working folder (asked
    /// several times per key press: the saved list is read once per folder).
    pub(super) fn lsp_enabled(&self) -> bool {
        let Some(w) = self.workdir.borrow().clone() else { return false };
        if let Some((dir, on)) = self.lsp_enabled_cache.borrow().as_ref()
            && *dir == w
        {
            return *on;
        }
        let on = config::load_lsp_folders().contains(&w);
        *self.lsp_enabled_cache.borrow_mut() = Some((w, on));
        on
    }

    fn set_lsp_folder(self: &Rc<Self>, on: bool) {
        let Some(root) = self.workdir.borrow().clone() else { return };
        let mut list = config::load_lsp_folders();
        list.retain(|p| *p != root);
        if on {
            list.push(root);
        }
        config::save_lsp_folders(&list);
        *self.lsp_enabled_cache.borrow_mut() = None;
        if on {
            self.message("Analisi del codice attiva: i language server partono quando apri un file del loro linguaggio");
        } else {
            self.message("Analisi del codice disattivata: language server fermati");
        }
        self.sync_lsp();
    }

    /// Follows the working folder: stops the servers of a closed folder,
    /// starts the ones needed by the open documents.
    pub(super) fn sync_lsp(self: &Rc<Self>) {
        let enabled = self.lsp_enabled();
        if let Some(a) = self.action("lsp-folder") {
            a.set_enabled(self.workdir.borrow().is_some());
            a.set_state(&enabled.to_variant());
        }
        let root = self.workdir.borrow().clone();
        let stale: Vec<Rc<Client>> =
            self.lsp_clients.borrow().iter().filter(|c| !enabled || Some(&c.root) != root.as_ref()).cloned().collect();
        for c in stale {
            c.stop();
        }
        self.lsp_clients.borrow_mut().retain(|c| !c.stopping.get());
        if !enabled {
            self.lsp_failed.borrow_mut().clear();
            self.update_lsp_status();
            return;
        }
        let docs: Vec<Rc<Doc>> = self.docs.borrow().iter().filter(|d| d.is_loaded()).cloned().collect();
        for d in docs {
            self.lsp_open(&d);
        }
        self.update_lsp_status();
    }

    /// Servers of a document: Vue files use the Vue server and the
    /// TypeScript one (which loads the Vue plugin).
    pub(super) fn lsp_specs_for(&self, d: &Doc) -> Vec<&'static lsp::ServerSpec> {
        match d.lang.get() {
            Lang::Vue => [lsp::VUE_SERVER, lsp::TS_SERVER].iter().filter_map(|n| lsp::spec_named(n)).collect(),
            l => lsp::spec_for(l).into_iter().collect(),
        }
    }

    /// Whether `d` belongs to the working folder with servers enabled.
    fn lsp_applies(&self, d: &Doc) -> bool {
        let Some(root) = self.workdir.borrow().clone() else { return false };
        self.lsp_enabled() && d.path.borrow().as_ref().is_some_and(|p| p.starts_with(&root))
    }

    /// The main server for `d` (started if needed).
    fn lsp_client(self: &Rc<Self>, d: &Doc) -> Option<Rc<Client>> {
        if !self.lsp_applies(d) {
            return None;
        }
        let spec = *self.lsp_specs_for(d).first()?;
        self.lsp_start(spec)
    }

    /// All the servers for `d` (started if needed).
    pub(super) fn lsp_clients_for(self: &Rc<Self>, d: &Doc) -> Vec<Rc<Client>> {
        if !self.lsp_applies(d) {
            return Vec::new();
        }
        self.lsp_specs_for(d).into_iter().filter_map(|s| self.lsp_start(s)).collect()
    }

    /// The running server `spec`, started if needed.
    fn lsp_start(self: &Rc<Self>, spec: &'static lsp::ServerSpec) -> Option<Rc<Client>> {
        if let Some(c) = self.lsp_clients.borrow().iter().find(|c| std::ptr::eq(c.spec, spec) && !c.stopping.get()) {
            return Some(c.clone());
        }
        if self.lsp_failed.borrow().contains(spec.name) {
            return None;
        }
        let root = self.workdir.borrow().clone()?;
        let (args, init) = self.lsp_options(spec, &root);
        let w = Rc::downgrade(self);
        let w2 = w.clone();
        let w3 = w.clone();
        let w4 = w.clone();
        let name = spec.name;
        let events = lsp::Events {
            ready: Box::new(move || {
                if let Some(ed) = w.upgrade() {
                    ed.lsp_status.borrow_mut().insert(name, String::new());
                    // Documents opened while it was starting.
                    let docs: Vec<Rc<Doc>> = ed.docs.borrow().iter().filter(|d| d.is_loaded()).cloned().collect();
                    for d in docs {
                        ed.lsp_open(&d);
                    }
                    ed.update_lsp_status();
                }
            }),
            status: Box::new(move |text| {
                if let Some(ed) = w2.upgrade() {
                    ed.lsp_status.borrow_mut().insert(name, text.to_string());
                    ed.update_lsp_status();
                }
            }),
            exited: Box::new(move |why| {
                let Some(ed) = w3.upgrade() else { return };
                ed.lsp_clients.borrow_mut().retain(|c| c.spec.name != name);
                ed.lsp_status.borrow_mut().remove(name);
                ed.forget_diagnostics(name);
                if !why.is_empty() {
                    ed.lsp_failed.borrow_mut().insert(name);
                    ed.error_dialog("Language server terminato", &why);
                }
                ed.update_lsp_status();
                ed.release_memory();
            }),
            notification: Box::new(move |method, params| {
                let Some(ed) = w4.upgrade() else { return };
                match method {
                    "tsserver/request" => ed.vue_tsserver_request(params),
                    "textDocument/publishDiagnostics" => ed.lsp_diagnostics(name, params),
                    _ => {}
                }
            }),
        };
        match Client::start(spec, &root, &args, init, events) {
            Ok(c) => {
                self.lsp_status.borrow_mut().insert(name, "avvio…".into());
                self.lsp_clients.borrow_mut().push(c.clone());
                self.update_lsp_status();
                Some(c)
            }
            Err(e) => {
                self.lsp_failed.borrow_mut().insert(name);
                self.message(&format!("Analisi del codice: {e}"));
                None
            }
        }
    }

    /// Extra arguments and initialization options of a server.
    fn lsp_options(&self, spec: &lsp::ServerSpec, root: &Path) -> (Vec<String>, Value) {
        // The TypeScript of the project, when it has one.
        let project_ts = root.join("node_modules/typescript/lib");
        match spec.name {
            lsp::VUE_SERVER if project_ts.join("typescript.js").exists() => (vec![format!("--tsdk={}", project_ts.display())], Value::Null),
            lsp::TS_SERVER => {
                // Lighter: one tsserver instead of two (no separate syntax
                // server) and no automatic download of @types packages.
                let mut init = json!({
                    "tsserver": {"useSyntaxServer": "never"},
                    "disableAutomaticTypingAcquisition": true,
                });
                // With the Vue plugin when the project has Vue components.
                if let Some(loc) = vue_plugin_location()
                    && self.project_has_vue(root)
                {
                    init["plugins"] = json!([{
                        "name": "@vue/typescript-plugin",
                        "location": loc,
                        "languages": ["vue"],
                        "configNamespace": "typescript"
                    }]);
                }
                (Vec::new(), init)
            }
            _ => (Vec::new(), Value::Null),
        }
    }

    fn project_has_vue(&self, root: &Path) -> bool {
        if self.docs.borrow().iter().any(|d| d.lang.get() == Lang::Vue) {
            return true;
        }
        // The file index already lists the project.
        if let Some(paths) = self.index_paths(root).filter(|p| !p.is_empty()) {
            return (0..paths.len()).any(|i| paths.get(i).ends_with(".vue"));
        }
        // A quick look at the folder (without dependencies and hidden folders).
        let mut stack = vec![root.to_path_buf()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                seen += 1;
                if seen > 20_000 {
                    return false;
                }
                let name = e.file_name();
                let name = name.to_string_lossy();
                match e.file_type() {
                    Ok(t) if t.is_dir() => {
                        if !name.starts_with('.') && name != "node_modules" && name != "target" && name != "dist" {
                            stack.push(e.path());
                        }
                    }
                    Ok(_) if name.ends_with(".vue") => return true,
                    _ => {}
                }
            }
        }
        false
    }

    /// The Vue server asks the TypeScript one (through its Vue plugin):
    /// `[[id, command, args]]`, answered with `tsserver/response`
    /// `[[id, body]]` (the JSON-RPC library of the Vue server wraps the
    /// array in another one).
    fn vue_tsserver_request(self: &Rc<Self>, params: &Value) {
        let params = match params.get(0) {
            Some(inner) if inner.is_array() => inner,
            _ => params,
        };
        let (Some(id), Some(command)) = (params.get(0).cloned(), params.get(1).and_then(Value::as_str)) else { return };
        let args = params.get(2).cloned().unwrap_or(Value::Null);
        let vue = self.lsp_clients.borrow().iter().find(|c| c.spec.name == lsp::VUE_SERVER).cloned();
        let Some(vue) = vue else { return };
        let ts = lsp::spec_named(lsp::TS_SERVER).and_then(|s| self.lsp_start(s));
        let Some(ts) = ts else {
            vue.notify("tsserver/response", json!([[id, null]]));
            return;
        };
        let vue = Rc::downgrade(&vue);
        ts.request(
            "workspace/executeCommand",
            json!({"command": "typescript.tsserverRequest", "arguments": [command, args]}),
            move |res| {
                let body = match res {
                    Ok(v) => v.get("body").cloned().unwrap_or(v),
                    Err(_) => Value::Null,
                };
                if let Some(vue) = vue.upgrade() {
                    vue.notify("tsserver/response", json!([[id, body]]));
                }
            },
        );
    }

    /// Tells the servers of `d` (started if needed) that the document is
    /// open. Servers that already have it get its changes from
    /// `lsp_changed` / `lsp_edit_*`, not here.
    pub(super) fn lsp_open(self: &Rc<Self>, d: &Rc<Doc>) {
        if !d.is_loaded() {
            return;
        }
        let Some(path) = d.path.borrow().clone() else { return };
        let uri = lsp::path_to_uri(&path);
        let lang_id = lsp::language_id(d.lang.get(), &path);
        let need: Vec<Rc<Client>> =
            self.lsp_clients_for(d).into_iter().filter(|c| c.ready.get() && !c.docs.borrow().contains_key(&uri)).collect();
        if need.is_empty() {
            return;
        }
        let text = d.text();
        let last = need.len() - 1;
        for (i, c) in need.into_iter().enumerate() {
            if i == last {
                c.did_open(&uri, lang_id, text);
                break;
            }
            c.did_open(&uri, lang_id, text.as_str());
        }
    }

    fn schedule_lsp_flush(self: &Rc<Self>) {
        if self.lsp_flush_scheduled.replace(true) {
            return;
        }
        let ed = Rc::downgrade(self);
        glib::timeout_add_local_once(CHANGE_DELAY, move || {
            if let Some(ed) = ed.upgrade() {
                ed.lsp_flush_scheduled.set(false);
                ed.lsp_flush();
            }
        });
    }

    /// Whole content of `d` was replaced (reload, reopen): sent to the server shortly.
    pub(super) fn lsp_changed(self: &Rc<Self>, d: &Rc<Doc>) {
        if self.lsp_clients.borrow().is_empty() {
            return;
        }
        self.lsp_dirty.borrow_mut().insert(d.id.borrow().clone(), PendingChanges::Full);
        self.schedule_lsp_flush();
    }

    /// Incremental insertion in `d` (recorded before the buffer mutates).
    pub(super) fn lsp_edit_insert(self: &Rc<Self>, d: &Rc<Doc>, at: &gtk::TextIter, text: &str) {
        if text.is_empty() || self.lsp_clients.borrow().is_empty() || !d.is_loaded() {
            return;
        }
        if d.is_loading() || text.len() > MAX_INCREMENTAL_TEXT {
            self.lsp_changed(d);
            return;
        }
        let id = d.id.borrow().clone();
        {
            let mut dirty = self.lsp_dirty.borrow_mut();
            let entry = dirty.entry(id).or_insert_with(|| PendingChanges::Incremental(Vec::with_capacity(4)));
            match entry {
                PendingChanges::Full => {}
                PendingChanges::Incremental(changes) if changes.len() >= MAX_INCREMENTAL_EDITS => {
                    *entry = PendingChanges::Full;
                }
                PendingChanges::Incremental(changes) => {
                    let (line, u16col) = iter_position(&d.buffer, at);
                    lsp::push_change(changes, lsp::Change { line, u16col, end_line: line, end_u16col: u16col, text: text.to_string() });
                }
            }
        }
        self.schedule_lsp_flush();
    }

    /// Incremental deletion in `d` (recorded before the buffer mutates).
    pub(super) fn lsp_edit_delete(self: &Rc<Self>, d: &Rc<Doc>, start: &gtk::TextIter, end: &gtk::TextIter) {
        if start == end || self.lsp_clients.borrow().is_empty() || !d.is_loaded() {
            return;
        }
        if d.is_loading() {
            self.lsp_changed(d);
            return;
        }
        let id = d.id.borrow().clone();
        {
            let mut dirty = self.lsp_dirty.borrow_mut();
            let entry = dirty.entry(id).or_insert_with(|| PendingChanges::Incremental(Vec::with_capacity(4)));
            match entry {
                PendingChanges::Full => {}
                PendingChanges::Incremental(changes) if changes.len() >= MAX_INCREMENTAL_EDITS => {
                    *entry = PendingChanges::Full;
                }
                PendingChanges::Incremental(changes) => {
                    let (line, u16col) = iter_position(&d.buffer, start);
                    let (end_line, end_u16col) = iter_position(&d.buffer, end);
                    lsp::push_change(changes, lsp::Change { line, u16col, end_line, end_u16col, text: String::new() });
                }
            }
        }
        self.schedule_lsp_flush();
    }

    /// Sends the pending changes.
    fn lsp_flush(self: &Rc<Self>) {
        let dirty: Vec<(String, PendingChanges)> = self.lsp_dirty.borrow_mut().drain().collect();
        for (id, pending) in dirty {
            let Some(d) = self.docs.borrow().iter().find(|d| *d.id.borrow() == id).cloned() else { continue };
            // Unloaded meanwhile: sent again when it is loaded.
            if !d.is_loaded() {
                continue;
            }
            let Some(path) = d.path.borrow().clone() else { continue };
            let clients = self.lsp_clients_for(&d);
            if clients.is_empty() {
                continue;
            }
            let uri = lsp::path_to_uri(&path);
            let lang_id = lsp::language_id(d.lang.get(), &path);
            let needs_full = clients
                .iter()
                .filter(|c| {
                    if c.docs.borrow().contains_key(&uri) {
                        !matches!(pending, PendingChanges::Incremental(_)) || !c.is_incremental()
                    } else {
                        c.ready.get()
                    }
                })
                .count();
            let mut full_text = (needs_full > 0).then(|| d.text());
            let mut rem_full = needs_full;
            for c in clients {
                if c.docs.borrow().contains_key(&uri) {
                    if let PendingChanges::Incremental(ref changes) = pending
                        && c.is_incremental()
                    {
                        c.did_change_incremental(&uri, changes);
                    } else if let Some(ref t) = full_text {
                        rem_full -= 1;
                        if rem_full == 0 {
                            c.did_change(&uri, full_text.take().unwrap());
                        } else {
                            c.did_change(&uri, t.as_str());
                        }
                    }
                } else if c.ready.get()
                    && let Some(ref t) = full_text
                {
                    rem_full -= 1;
                    if rem_full == 0 {
                        c.did_open(&uri, lang_id, full_text.take().unwrap());
                    } else {
                        c.did_open(&uri, lang_id, t.as_str());
                    }
                }
            }
        }
    }

    pub(super) fn lsp_saved(self: &Rc<Self>, d: &Rc<Doc>) {
        self.lsp_flush();
        let Some(path) = d.path.borrow().clone() else { return };
        let uri = lsp::path_to_uri(&path);
        let mut missing = false;
        for c in self.lsp_clients_for(d) {
            if c.docs.borrow().contains_key(&uri) {
                c.did_save(&uri);
            } else {
                missing = true;
            }
        }
        if missing {
            self.lsp_open(d);
        }
    }

    pub(super) fn lsp_closed(&self, d: &Doc) {
        self.lsp_dirty.borrow_mut().remove(&*d.id.borrow());
        let Some(path) = d.path.borrow().clone() else { return };
        let uri = lsp::path_to_uri(&path);
        for c in self.lsp_clients.borrow().iter() {
            c.did_close(&uri);
        }
        self.lsp_diags.borrow_mut().retain(|(_, u), _| *u != uri);
    }

    fn update_lsp_status(&self) {
        let label = &self.status.lsp;
        let clients = self.lsp_clients.borrow();
        if clients.is_empty() {
            label.set_visible(false);
            return;
        }
        let status = self.lsp_status.borrow();
        let parts: Vec<String> = clients
            .iter()
            .map(|c| match status.get(c.spec.name).map(String::as_str) {
                Some("") | None if c.ready.get() => format!("{} ✓", c.spec.name),
                Some(s) if !s.is_empty() => format!("{}: {s}", c.spec.name),
                _ => format!("{}: avvio…", c.spec.name),
            })
            .collect();
        label.set_text(&parts.join(" · "));
        label.set_tooltip_text(Some("Language server attivi per la cartella di lavoro (File → Analisi del codice)"));
        label.set_visible(true);
    }

    /// Memory (kB) of the running language servers.
    pub(super) fn lsp_memory_kb(&self) -> u64 {
        self.lsp_clients
            .borrow()
            .iter()
            .filter_map(|c| c.pid())
            .map(|pid| {
                // The server and its children (e.g. node workers).
                let mut total = 0;
                let mut stack = vec![pid];
                while let Some(p) = stack.pop() {
                    total += crate::proc::rss_kb(&p);
                    if let Ok(children) = std::fs::read_to_string(format!("/proc/{p}/task/{p}/children")) {
                        stack.extend(children.split_whitespace().map(str::to_string));
                    }
                }
                total
            })
            .sum()
    }

    // ------------------------------------------------------------- requests

    /// The server to ask about the cursor of `d`: in Vue files the
    /// TypeScript one for definitions, references and the script, the Vue
    /// one for completions in the template.
    fn lsp_server_for(self: &Rc<Self>, d: &Rc<Doc>, completion: bool) -> Option<Rc<Client>> {
        if d.lang.get() != Lang::Vue {
            return self.lsp_client(d);
        }
        let clients = self.lsp_clients_for(d);
        let named = |n: &str| clients.iter().find(|c| c.spec.name == n).cloned();
        let in_script = in_vue_script(d);
        if completion && !in_script { named(lsp::VUE_SERVER) } else { named(lsp::TS_SERVER).or_else(|| named(lsp::VUE_SERVER)) }
    }

    /// Text position of the cursor of `d` for a request to `c` (which
    /// gets the current text first).
    fn lsp_target(self: &Rc<Self>, c: &Rc<Client>, d: &Rc<Doc>) -> Option<Value> {
        let path = d.path.borrow().clone()?;
        // The server must see the current text (every change marks the
        // document as dirty).
        if self.lsp_dirty.borrow().contains_key(&*d.id.borrow()) {
            self.lsp_flush();
        }
        if !c.docs.borrow().contains_key(&lsp::path_to_uri(&path)) {
            if !c.ready.get() {
                return None;
            }
            self.lsp_open(d);
        }
        let (line, character) = cursor_position(d);
        Some(json!({"textDocument": {"uri": lsp::path_to_uri(&path)}, "position": {"line": line, "character": character}}))
    }

    pub(super) fn lsp_trigger_char(&self, d: &Doc, text: &str) -> bool {
        let specs = self.lsp_specs_for(d);
        self.lsp_clients.borrow().iter().any(|c| {
            specs.iter().any(|s| std::ptr::eq(c.spec, *s)) && c.ready.get() && c.trigger_chars.borrow().iter().any(|t| t.ends_with(text))
        })
    }

    pub(super) fn lsp_definition(self: &Rc<Self>, d: &Rc<Doc>, word: &str) -> bool {
        let Some(c) = self.lsp_server_for(d, false) else { return false };
        let Some(params) = self.lsp_target(&c, d) else { return false };
        self.message(&format!("{}: ricerca della definizione…", c.spec.name));
        let ed = Rc::downgrade(self);
        let (d, word) = (d.clone(), word.to_string());
        c.request("textDocument/definition", params, move |res| {
            let Some(ed) = ed.upgrade() else { return };
            let locs = res.map(|v| lsp::parse_locations(&v)).unwrap_or_default();
            match locs.len() {
                // Nothing from the server: by name.
                0 => ed.local_definition(&d, &word),
                1 => {
                    ed.message("");
                    let l = &locs[0];
                    let col = lsp::char_col(&line_of(&ed, &l.path, l.line), l.u16col);
                    ed.jump_to(&d, Some(&l.path), l.line, col);
                }
                _ => {
                    ed.message("");
                    let root = ed.code_root(&d).unwrap_or_else(|| PathBuf::from("/"));
                    let res = location_results(&ed, &root, &locs, &word);
                    ed.show_text_results(&root, &format!("Definizioni di «{word}»"), &word, res);
                }
            }
        });
        true
    }

    pub(super) fn lsp_references(self: &Rc<Self>, d: &Rc<Doc>, word: &str) -> bool {
        let Some(c) = self.lsp_server_for(d, false) else { return false };
        let Some(mut params) = self.lsp_target(&c, d) else { return false };
        params["context"] = json!({"includeDeclaration": true});
        self.message(&format!("{}: ricerca degli utilizzi…", c.spec.name));
        let ed = Rc::downgrade(self);
        let (d, word) = (d.clone(), word.to_string());
        c.request("textDocument/references", params, move |res| {
            let Some(ed) = ed.upgrade() else { return };
            let locs = res.map(|v| lsp::parse_locations(&v)).unwrap_or_default();
            let Some(root) = ed.code_root(&d) else { return };
            if locs.is_empty() {
                // Nothing from the server: by name.
                ed.search_usages(&root, &word);
                return;
            }
            ed.message("");
            let res = location_results(&ed, &root, &locs, &word);
            ed.show_text_results(&root, &format!("Utilizzi di «{word}»"), &word, res);
        });
        true
    }

    /// Servers asked for completions: in the template of a Vue file both
    /// (TypeScript completes the expressions through the Vue plugin, Vue
    /// the tags, directives and events), otherwise one.
    fn lsp_completion_servers(self: &Rc<Self>, d: &Rc<Doc>) -> Vec<Rc<Client>> {
        let servers: Vec<Rc<Client>> = if d.lang.get() == Lang::Vue {
            let in_script = in_vue_script(d);
            self.lsp_clients_for(d).into_iter().filter(|c| !in_script || c.spec.name == lsp::TS_SERVER).collect()
        } else {
            self.lsp_client(d).into_iter().collect()
        };
        servers.into_iter().filter(|c| c.ready.get()).collect()
    }

    pub(super) fn lsp_completion(self: &Rc<Self>, d: &Rc<Doc>, generation: u32) -> bool {
        let servers = self.lsp_completion_servers(d);
        if servers.is_empty() {
            return false;
        }
        // Typed a trigger character (`.`, `::`…)?
        let buf = &d.buffer;
        let it = buf.iter_at_mark(&buf.get_insert());
        let mut before = it;
        before.backward_char();
        let prev = buf.text(&before, &it, true).to_string();
        // Answers to the previous key press are not needed any more.
        if let Some(c) = self.completion.borrow().as_ref() {
            for (client, id) in c.requests.borrow_mut().drain(..) {
                if let Some(client) = client.upgrade() {
                    client.cancel(id);
                }
            }
        }
        // (answers still expected, answers received)
        let answers: Rc<RefCell<(usize, Vec<Value>)>> = Rc::new(RefCell::new((0, Vec::new())));
        for c in servers {
            let Some(mut params) = self.lsp_target(&c, d) else { continue };
            params["context"] = if c.trigger_chars.borrow().iter().any(|t| t.ends_with(prev.as_str())) {
                json!({"triggerKind": 2, "triggerCharacter": prev})
            } else {
                json!({"triggerKind": 1})
            };
            answers.borrow_mut().0 += 1;
            let ed = Rc::downgrade(self);
            let (d, answers) = (d.clone(), answers.clone());
            let client = Rc::downgrade(&c);
            let id = c.request("textDocument/completion", params, move |res| {
                let all = {
                    let mut a = answers.borrow_mut();
                    a.0 -= 1;
                    if let Ok(v) = res {
                        a.1.push(v);
                    }
                    if a.0 > 0 {
                        return;
                    }
                    std::mem::take(&mut a.1)
                };
                let Some(ed) = ed.upgrade() else { return };
                // Typed something else meanwhile?
                let current = ed.completion.borrow().as_ref().is_some_and(|c| c.generation.get() == generation);
                if !current {
                    return;
                }
                let prefix = super::inspect::word_at_cursor(&d).map_or(String::new(), |(w, a, _)| {
                    // Only the part before the cursor.
                    let cur = d.cursor_iter().offset();
                    w.chars().take((cur - a).max(0) as usize).collect()
                });
                let mut items: Vec<Item> = Vec::new();
                for v in &all {
                    for it in completion_items(&d, v, &prefix) {
                        if !items.iter().any(|x| x.label == it.label) {
                            items.push(it);
                        }
                    }
                }
                items.truncate(MAX_ITEMS);
                if items.is_empty() && !prefix.is_empty() {
                    items = ed.local_items(&d, &prefix);
                }
                ed.show_completion(&d, items);
            });
            if let Some(comp) = self.completion.borrow().as_ref() {
                comp.requests.borrow_mut().push((client, id));
            }
        }
        answers.borrow().0 > 0
    }
}

/// Completion proposals of a server answer, filtered by `prefix`.
fn completion_items(d: &Doc, v: &Value, prefix: &str) -> Vec<Item> {
    let list = v.get("items").unwrap_or(v);
    let Some(arr) = list.as_array() else { return Vec::new() };
    let lower = prefix.to_lowercase();
    // Servers send thousands of proposals: they are filtered and sorted
    // first, and only the rows shown are built.
    let mut keyed: Vec<(bool, &str, &Value)> = arr
        .iter()
        .filter_map(|it| {
            let label = it.get("label")?.as_str()?.trim();
            let filter = it.get("filterText").and_then(Value::as_str).unwrap_or(label).to_lowercase();
            let starts = filter.starts_with(&lower);
            if !lower.is_empty() && !starts && !filter.contains(&lower) {
                return None;
            }
            let sort = it.get("sortText").and_then(Value::as_str).unwrap_or(label);
            // Prefix matches before the others.
            Some((!starts, sort, it))
        })
        .collect();
    keyed.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    // Text of the lines of the edit ranges (usually all the same line).
    let mut line_cache: Option<(i32, String)> = None;
    keyed
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|(_, _, it)| {
            let label = it.get("label")?.as_str()?.trim().to_string();
            // Snippets are not announced (`snippetSupport: false`), but some
            // servers send them anyway.
            let snippet = it.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
            let edit = it.get("textEdit");
            let raw = edit.and_then(|e| e.get("newText")).or_else(|| it.get("insertText")).and_then(Value::as_str).unwrap_or(&label);
            let insert = if snippet { lsp::strip_snippet(raw) } else { raw.to_string() };
            // Start of the replaced text, from the edit range.
            let replace_from = edit.and_then(|e| e.get("range")).and_then(|r| {
                let line = r.pointer("/start/line")?.as_u64()? as i32;
                let u16 = r.pointer("/start/character")?.as_u64()? as u32;
                let start = d.buffer.iter_at_line(line)?;
                if line_cache.as_ref().is_none_or(|(l, _)| *l != line) {
                    line_cache = Some((line, d.line_text(line)));
                }
                let text = &line_cache.as_ref()?.1;
                Some(start.offset() + lsp::char_col(text, u16) as i32)
            });
            let kind = it.get("kind").and_then(Value::as_u64).map_or("", lsp::completion_kind);
            let detail = it.get("detail").and_then(Value::as_str).unwrap_or("").trim();
            let detail = match (kind.is_empty(), detail.is_empty()) {
                (false, false) => format!("{kind} · {detail}"),
                (false, true) => kind.to_string(),
                _ => detail.to_string(),
            };
            Some(Item { label, insert, detail, replace_from })
        })
        .collect()
}

/// Locations from a server, shown like search results.
fn location_results(ed: &Editor, root: &Path, locs: &[Location], word: &str) -> TextResults {
    // The lines needed from each file, read once: from the open document
    // (the server has its unsaved text), else from the disk.
    let mut wanted: HashMap<&Path, HashMap<u32, String>> = HashMap::new();
    for l in locs {
        wanted.entry(l.path.as_path()).or_default().insert(l.line, String::new());
    }
    for (path, lines) in &mut wanted {
        let doc = ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(*path) && d.is_loaded()).cloned();
        match doc {
            Some(d) => {
                for (n, text) in lines.iter_mut() {
                    *text = d.line_text(*n as i32);
                }
            }
            None => {
                let Some(text) = finder::read_text(path) else { continue };
                let last = lines.keys().max().copied().unwrap_or(0) as usize;
                for (n, l) in text.lines().enumerate().take(last + 1) {
                    if let Some(t) = lines.get_mut(&(n as u32)) {
                        *t = l.to_string();
                    }
                }
            }
        }
    }
    let mut res = TextResults::default();
    for l in locs {
        let line = wanted.get(l.path.as_path()).and_then(|m| m.get(&l.line)).map_or("", String::as_str);
        let col = lsp::char_col(line, l.u16col) as usize;
        let start = line.char_indices().nth(col).map_or(line.len(), |(b, _)| b);
        let (snippet, hl, col) = finder::snippet(line, start, start + word.len());
        let file = res.file_index(finder::rel_or_full(root, &l.path));
        res.matches.push(TextMatch { file, line: l.line, col, snippet, hl });
    }
    res
}
