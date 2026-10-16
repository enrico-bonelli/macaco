//! Language servers in the working folder (opt-in per folder): started when
//! a document of their language is opened, kept in sync with the open
//! documents and stopped when the folder is closed. Definitions, references
//! and completions come from them when they answer, otherwise from the
//! symbols of `inspect`.

use super::Editor;
use super::inspect::Item;
use crate::config;
use crate::doc::Doc;
use crate::finder::{self, TextMatch, TextResults};
use crate::lsp::{self, Client, Location};
use gtk::prelude::*;
use gtk::{gio, glib};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Changes are sent to the server this long after the last key press.
const CHANGE_DELAY: Duration = Duration::from_millis(300);
/// Completion rows from the server.
const MAX_ITEMS: usize = 80;

/// LSP position of the cursor of `d`: (line, UTF-16 column).
fn cursor_position(d: &Doc) -> (u32, u32) {
    let buf = &d.buffer;
    let it = buf.iter_at_mark(&buf.get_insert());
    let mut start = it;
    start.set_line_offset(0);
    let line = buf.text(&start, &it, false);
    (it.line() as u32, lsp::utf16_col(&line, it.line_offset() as usize))
}

/// Text of line `n` of `path`: from the open document if any.
fn line_of(ed: &Editor, path: &Path, n: u32) -> String {
    let doc = ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path) && d.is_loaded()).cloned();
    match doc {
        Some(d) => d.buffer.iter_at_line(n as i32).map_or_else(String::new, |a| {
            let mut b = a;
            if !b.ends_line() {
                b.forward_to_line_end();
            }
            d.buffer.text(&a, &b, false).to_string()
        }),
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

    fn lsp_enabled(&self) -> bool {
        self.workdir.borrow().as_ref().is_some_and(|w| config::load_lsp_folders().contains(w))
    }

    fn set_lsp_folder(self: &Rc<Self>, on: bool) {
        let Some(root) = self.workdir.borrow().clone() else { return };
        let mut list = config::load_lsp_folders();
        list.retain(|p| *p != root);
        if on {
            list.push(root);
        }
        config::save_lsp_folders(&list);
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
        if let Some(a) = self.win.lookup_action("lsp-folder").and_downcast::<gio::SimpleAction>() {
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

    /// The running server for `d` (started if needed).
    fn lsp_client(self: &Rc<Self>, d: &Doc) -> Option<Rc<Client>> {
        if !self.lsp_enabled() {
            return None;
        }
        let root = self.workdir.borrow().clone()?;
        let path = d.path.borrow().clone()?;
        if !path.starts_with(&root) {
            return None;
        }
        let spec = lsp::spec_for(d.lang.get())?;
        if let Some(c) = self.lsp_clients.borrow().iter().find(|c| std::ptr::eq(c.spec, spec) && !c.stopping.get()) {
            return Some(c.clone());
        }
        if self.lsp_failed.borrow().contains(spec.name) {
            return None;
        }
        let w = Rc::downgrade(self);
        let w2 = w.clone();
        let w3 = w.clone();
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
                if !why.is_empty() {
                    ed.lsp_failed.borrow_mut().insert(name);
                    ed.error_dialog("Language server terminato", &why);
                }
                ed.update_lsp_status();
                ed.release_memory();
            }),
        };
        match Client::start(spec, &root, events) {
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

    /// Tells the server of `d` that the document is open.
    pub(super) fn lsp_open(self: &Rc<Self>, d: &Rc<Doc>) {
        if !d.is_loaded() {
            return;
        }
        let Some(c) = self.lsp_client(d) else { return };
        let Some(path) = d.path.borrow().clone() else { return };
        if c.ready.get() {
            c.did_open(&lsp::path_to_uri(&path), lsp::language_id(d.lang.get(), &path), &d.text());
        }
    }

    /// The text of `d` changed: sent to the server shortly.
    pub(super) fn lsp_changed(self: &Rc<Self>, d: &Rc<Doc>) {
        if self.lsp_clients.borrow().is_empty() || !d.is_loaded() {
            return;
        }
        self.lsp_dirty.borrow_mut().insert(d.id.borrow().clone());
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

    /// Sends the pending changes.
    fn lsp_flush(self: &Rc<Self>) {
        let dirty: Vec<String> = self.lsp_dirty.borrow_mut().drain().collect();
        for id in dirty {
            let Some(d) = self.docs.borrow().iter().find(|d| *d.id.borrow() == id).cloned() else { continue };
            let (Some(path), Some(c)) = (d.path.borrow().clone(), self.lsp_client(&d)) else { continue };
            let uri = lsp::path_to_uri(&path);
            if c.docs.borrow().contains_key(&uri) {
                c.did_change(&uri, &d.text());
            } else if c.ready.get() {
                c.did_open(&uri, lsp::language_id(d.lang.get(), &path), &d.text());
            }
        }
    }

    pub(super) fn lsp_saved(self: &Rc<Self>, d: &Rc<Doc>) {
        self.lsp_flush();
        if let (Some(path), Some(c)) = (d.path.borrow().clone(), self.lsp_client(d)) {
            let uri = lsp::path_to_uri(&path);
            if c.docs.borrow().contains_key(&uri) {
                c.did_save(&uri);
            } else {
                self.lsp_open(d);
            }
        }
    }

    pub(super) fn lsp_closed(&self, d: &Doc) {
        let Some(path) = d.path.borrow().clone() else { return };
        let uri = lsp::path_to_uri(&path);
        for c in self.lsp_clients.borrow().iter() {
            c.did_close(&uri);
        }
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
                    let status = std::fs::read_to_string(format!("/proc/{p}/status")).unwrap_or_default();
                    total += status
                        .lines()
                        .find_map(|l| l.strip_prefix("VmRSS:"))
                        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                        .unwrap_or(0);
                    if let Ok(children) = std::fs::read_to_string(format!("/proc/{p}/task/{p}/children")) {
                        stack.extend(children.split_whitespace().map(str::to_string));
                    }
                }
                total
            })
            .sum()
    }

    // ------------------------------------------------------------- requests

    /// Server and text position for a request about the cursor of `d`.
    fn lsp_target(self: &Rc<Self>, d: &Rc<Doc>) -> Option<(Rc<Client>, Value)> {
        let c = self.lsp_client(d)?;
        let path = d.path.borrow().clone()?;
        // The server must see the current text.
        if d.is_unsaved() || self.lsp_dirty.borrow().contains(&*d.id.borrow()) {
            self.lsp_dirty.borrow_mut().insert(d.id.borrow().clone());
            self.lsp_flush();
        }
        if !c.docs.borrow().contains_key(&lsp::path_to_uri(&path)) {
            if !c.ready.get() {
                return None;
            }
            self.lsp_open(d);
        }
        let (line, character) = cursor_position(d);
        Some((c, json!({"textDocument": {"uri": lsp::path_to_uri(&path)}, "position": {"line": line, "character": character}})))
    }

    pub(super) fn lsp_trigger_char(&self, d: &Doc, text: &str) -> bool {
        let Some(spec) = lsp::spec_for(d.lang.get()) else { return false };
        self.lsp_clients
            .borrow()
            .iter()
            .any(|c| std::ptr::eq(c.spec, spec) && c.ready.get() && c.trigger_chars.borrow().iter().any(|t| t.ends_with(text)))
    }

    pub(super) fn lsp_definition(self: &Rc<Self>, d: &Rc<Doc>, word: &str) -> bool {
        let Some((c, params)) = self.lsp_target(d) else { return false };
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
        let Some((c, mut params)) = self.lsp_target(d) else { return false };
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

    pub(super) fn lsp_completion(self: &Rc<Self>, d: &Rc<Doc>, generation: u32) -> bool {
        let Some(c) = self.lsp_client(d) else { return false };
        if !c.ready.get() {
            return false;
        }
        let Some((c, mut params)) = self.lsp_target(d) else { return false };
        // Typed a trigger character (`.`, `::`…)?
        let buf = &d.buffer;
        let it = buf.iter_at_mark(&buf.get_insert());
        let mut before = it;
        before.backward_char();
        let prev = buf.text(&before, &it, false).to_string();
        if c.trigger_chars.borrow().iter().any(|t| t.ends_with(prev.as_str())) {
            params["context"] = json!({"triggerKind": 2, "triggerCharacter": prev});
        } else {
            params["context"] = json!({"triggerKind": 1});
        }
        let ed = Rc::downgrade(self);
        let d = d.clone();
        c.request("textDocument/completion", params, move |res| {
            let Some(ed) = ed.upgrade() else { return };
            // Typed something else meanwhile?
            let current = ed.completion.borrow().as_ref().is_some_and(|c| c.generation.get() == generation);
            if !current {
                return;
            }
            let (prefix, _) = super::inspect::word_at_cursor(&d).map_or((String::new(), 0), |(w, a, _)| {
                // Only the part before the cursor.
                let cur = d.buffer.iter_at_mark(&d.buffer.get_insert()).offset();
                (w.chars().take((cur - a).max(0) as usize).collect(), a)
            });
            let mut items = res.map(|v| completion_items(&d, &v, &prefix)).unwrap_or_default();
            if items.is_empty() && !prefix.is_empty() {
                items = ed.local_items(&d, &prefix);
            }
            ed.show_completion(&d, items);
        });
        true
    }
}

/// Completion proposals of a server answer, filtered by `prefix`.
fn completion_items(d: &Doc, v: &Value, prefix: &str) -> Vec<Item> {
    let list = v.get("items").unwrap_or(v);
    let Some(arr) = list.as_array() else { return Vec::new() };
    let lower = prefix.to_lowercase();
    let mut items: Vec<(String, Item)> = arr
        .iter()
        .filter_map(|it| {
            let label = it.get("label")?.as_str()?.trim().to_string();
            let filter = it.get("filterText").and_then(Value::as_str).unwrap_or(&label).to_lowercase();
            if !lower.is_empty() && !filter.starts_with(&lower) && !filter.contains(&lower) {
                return None;
            }
            let snippet = it.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
            let edit = it.get("textEdit");
            let raw = edit
                .and_then(|e| e.get("newText"))
                .or_else(|| it.get("insertText"))
                .and_then(Value::as_str)
                .unwrap_or(&label)
                .to_string();
            let insert = if snippet { lsp::strip_snippet(&raw) } else { raw };
            // Start of the replaced text, from the edit range.
            let replace_from = edit.and_then(|e| e.get("range").or_else(|| e.get("replace"))).and_then(|r| {
                let line = r.pointer("/start/line")?.as_u64()? as i32;
                let u16 = r.pointer("/start/character")?.as_u64()? as u32;
                let start = d.buffer.iter_at_line(line)?;
                let mut end = start;
                if !end.ends_line() {
                    end.forward_to_line_end();
                }
                let text = d.buffer.text(&start, &end, false);
                Some(start.offset() + lsp::char_col(&text, u16) as i32)
            });
            let kind = it.get("kind").and_then(Value::as_u64).map_or("", lsp::completion_kind);
            let detail = it.get("detail").and_then(Value::as_str).unwrap_or("").trim();
            let detail = match (kind.is_empty(), detail.is_empty()) {
                (false, false) => format!("{kind} · {detail}"),
                (false, true) => kind.to_string(),
                _ => detail.to_string(),
            };
            let sort = it.get("sortText").and_then(Value::as_str).unwrap_or(&label).to_string();
            // Prefix matches before the others.
            let rank = if filter.starts_with(&lower) { "0" } else { "1" };
            Some((format!("{rank}{sort}"), Item { label, insert, detail, replace_from }))
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.into_iter().take(MAX_ITEMS).map(|(_, i)| i).collect()
}

/// Locations from a server, shown like search results.
fn location_results(ed: &Editor, root: &Path, locs: &[Location], word: &str) -> TextResults {
    let mut res = TextResults::default();
    let mut lines: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for l in locs {
        let line = match lines.get(&l.path) {
            Some(ls) => ls.get(l.line as usize).cloned().unwrap_or_default(),
            None => {
                let ls: Vec<String> = finder::read_text(&l.path).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
                let one = ls.get(l.line as usize).cloned().unwrap_or_else(|| line_of(ed, &l.path, l.line));
                lines.insert(l.path.clone(), ls);
                one
            }
        };
        let col = lsp::char_col(&line, l.u16col) as usize;
        let start = line.char_indices().nth(col).map_or(line.len(), |(b, _)| b);
        let end = (start + word.len()).min(line.len());
        let end = if line.is_char_boundary(end) { end } else { start };
        let (snippet, hl, col) = finder::snippet(&line, start, end);
        let rel = l.path.strip_prefix(root).map_or_else(|_| l.path.to_string_lossy().into_owned(), |r| r.to_string_lossy().into_owned());
        let file = match res.files.iter().position(|f| *f == rel) {
            Some(i) => i as u32,
            None => {
                res.files.push(rel);
                (res.files.len() - 1) as u32
            }
        };
        res.matches.push(TextMatch { file, line: l.line, col, snippet, hl });
    }
    res
}
