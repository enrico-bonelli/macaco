//! Code awareness without a language server: symbols of the indexed
//! working folder, "go to definition", "find usages" and completion.
//! When a language server is running for the document (see `lspui`), its
//! answers are used instead.

use super::Editor;
use crate::doc::Doc;
use crate::finder::{self, TextMatch, TextResults};
use crate::lang::{self, Lang};
use crate::symbols::{self, FileSymbols, ProjectSymbols, Symbol};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Files bigger than this are not scanned for symbols.
const MAX_SYMBOL_FILE: u64 = 1 << 20;
/// Completion: characters typed before the list appears.
const MIN_PREFIX: usize = 2;
/// Completion: rows shown.
const MAX_ITEMS: usize = 60;
/// Characters around the cursor scanned for local words (~64 KB window).
const NEAR_CHARS: i32 = 32_768;
/// Coalesces completion updates during rapid typing bursts.
const COMPLETION_DELAY: Duration = Duration::from_millis(30);
/// Reuse window for active-document symbol definitions while typing.
const SYMBOL_CACHE_TTL: Duration = Duration::from_secs(3);

/// Symbols of the indexed working folder.
pub(super) struct Symbols {
    pub(super) root: PathBuf,
    pub(super) project: RefCell<ProjectSymbols>,
}

/// Cached definitions extracted from the active document.
pub(super) struct DocSymbols {
    doc_id: String,
    lang: Lang,
    edit_gen: u64,
    computed_at: Instant,
    symbols: Rc<[Symbol]>,
}

/// A completion proposal.
#[derive(Clone)]
pub(super) struct Item {
    pub(super) label: String,
    /// Text inserted (defaults to the label).
    pub(super) insert: String,
    pub(super) detail: String,
    /// Characters before the cursor replaced by `insert` (None: the word
    /// being typed).
    pub(super) replace_from: Option<i32>,
}

/// The completion popup (one, moved to the active document).
pub(super) struct Completion {
    popover: gtk::Popover,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    items: RefCell<Vec<Item>>,
    doc: RefCell<Weak<Doc>>,
    /// Offset where the word being completed starts.
    start: Cell<i32>,
    /// Requests started before this one are obsolete.
    pub(super) generation: Cell<u32>,
    /// Completion requests waiting for a server, cancelled by a new one.
    pub(super) requests: RefCell<Vec<(Weak<crate::lsp::Client>, i64)>>,
}

impl Completion {
    /// Scroll the list so that `row` is visible (the focus stays in the
    /// editor, so the list does not follow the selection by itself).
    fn scroll_to(&self, row: &gtk::ListBoxRow) {
        let Some(b) = row.compute_bounds(&self.list) else { return };
        let adj = self.scroll.vadjustment();
        let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
        if top < adj.value() {
            adj.set_value(top);
        } else if bottom > adj.value() + adj.page_size() {
            adj.set_value(bottom - adj.page_size());
        }
    }
}

/// Identifier around the cursor: (text, start offset, end offset).
pub(super) fn word_at_cursor(d: &Doc) -> Option<(String, i32, i32)> {
    let buf = &d.buffer;
    let it = buf.iter_at_mark(&buf.get_insert());
    let mut a = it;
    while a.backward_char() {
        if !symbols::is_ident_char(a.char()) {
            a.forward_char();
            break;
        }
    }
    let mut b = it;
    while symbols::is_ident_char(b.char()) && b.forward_char() {}
    let w = buf.text(&a, &b, true).to_string();
    (!w.is_empty() && !w.starts_with(|c: char| c.is_ascii_digit())).then(|| (w, a.offset(), b.offset()))
}

/// Name of the component whose tag is under the cursor in a Vue or HTML
/// template (`<carrello-riga` or `</CarrelloRiga`), in PascalCase.
fn component_tag_at_cursor(d: &Doc) -> Option<String> {
    if !matches!(d.lang.get(), Lang::Vue | Lang::Html) {
        return None;
    }
    let buf = &d.buffer;
    let it = buf.iter_at_mark(&buf.get_insert());
    let tag_char = |c: char| symbols::is_ident_char(c) || c == '-';
    let mut a = it;
    while a.backward_char() {
        if !tag_char(a.char()) {
            break;
        }
    }
    // `a` is on the character before the name: '<' or '/' of '</'.
    let lt = a.char() == '<' || (a.char() == '/' && { let mut p = a; p.backward_char() && p.char() == '<' });
    if !lt {
        return None;
    }
    a.forward_char();
    let mut b = it;
    while tag_char(b.char()) && b.forward_char() {}
    let name = buf.text(&a, &b, true).to_string();
    // Components have an upper case letter or a dash (HTML tags have neither).
    (name.contains('-') || name.starts_with(|c: char| c.is_uppercase())).then(|| symbols::pascal_case(&name))
}

/// Text of the word being typed before the cursor and its start offset.
fn prefix_at_cursor(d: &Doc) -> (String, i32) {
    let buf = &d.buffer;
    let it = buf.iter_at_mark(&buf.get_insert());
    let mut a = it;
    while a.backward_char() {
        if !symbols::is_ident_char(a.char()) {
            a.forward_char();
            break;
        }
    }
    (buf.text(&a, &it, true).to_string(), a.offset())
}

/// A row of the completion list: name and detail.
fn completion_row() -> gtk::Box {
    let name = gtk::Label::new(None);
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_width_chars(16);
    name.set_max_width_chars(48);
    name.add_css_class("monospace");
    let detail = gtk::Label::new(None);
    detail.add_css_class("dim-label");
    detail.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    detail.set_max_width_chars(30);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_start(4);
    row.set_margin_end(4);
    row.append(&name);
    row.append(&detail);
    row
}

/// Text of the document around the cursor (~64 KB), for the local words
/// proposed by the completion without copying the whole buffer.
fn text_near_cursor(d: &Doc) -> glib::GString {
    let buf = &d.buffer;
    let cur = buf.iter_at_mark(&buf.get_insert()).offset();
    let a = buf.iter_at_offset((cur - NEAR_CHARS).max(0));
    let b = buf.iter_at_offset(cur + NEAR_CHARS);
    buf.text(&a, &b, true)
}

impl Editor {
    pub(super) fn setup_inspect_actions(self: &Rc<Self>) {
        let add = |name: &str, accels: &[&str], f: fn(&Rc<Editor>)| self.add_action(name, accels, f);
        add("goto-definition", &["F4"], |ed| ed.goto_definition());
        add("find-usages", &["<Shift>F4"], |ed| ed.find_usages());
        add("complete", &["<Control>space"], |ed| {
            if let Some(d) = ed.active_doc() {
                ed.cancel_completion_timer();
                ed.update_completion(&d, true);
            }
        });
    }

    /// Menu entries added to the context menu of the documents.
    pub(super) fn code_menu() -> gio::Menu {
        let m = gio::Menu::new();
        let s = gio::Menu::new();
        s.append(Some("Vai alla definizione"), Some("win.goto-definition"));
        s.append(Some("Trova utilizzi"), Some("win.find-usages"));
        s.append(Some("Completa"), Some("win.complete"));
        m.append_section(None, &s);
        m
    }

    /// Context menu, Ctrl+click and completion for a document.
    pub(super) fn wire_inspect(self: &Rc<Self>, d: &Rc<Doc>) {
        // Setting the menu marks the buffer as modified (GTK): keep its state.
        let modified = d.buffer.is_modified();
        d.view.set_extra_menu(Some(&Self::code_menu()));
        d.buffer.set_modified(modified);
        // Right click outside the selection moves the cursor, so that the
        // menu entries act on the word clicked.
        let g = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        g.set_propagation_phase(gtk::PropagationPhase::Capture);
        let wd = d.weak();
        g.connect_pressed(move |_, _, x, y| {
            let Some(d) = wd.upgrade() else { return };
            let (bx, by) = d.view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let Some(it) = d.view.iter_at_location(bx, by) else { return };
            let inside = d.buffer.selection_bounds().is_some_and(|(a, b)| it.in_range(&a, &b) || it == b);
            if !inside {
                d.buffer.place_cursor(&it);
            }
        });
        d.view.add_controller(g);
        // Ctrl+click: go to the definition.
        let g = gtk::GestureClick::builder().button(gdk::BUTTON_PRIMARY).build();
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        g.connect_released(move |g, _, x, y| {
            if !g.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                return;
            }
            let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) else { return };
            let (bx, by) = d.view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            if let Some(it) = d.view.iter_at_location(bx, by) {
                d.buffer.place_cursor(&it);
                ed.goto_definition();
            }
        });
        d.view.add_controller(g);

        // Programmatic full-buffer replacements for the language server.
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.buffer.connect_changed(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade())
                && d.is_loading()
            {
                ed.lsp_changed(&d);
            }
        });

        // Incremental LSP sync and completion while typing.
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.buffer.connect_insert_text(move |_, iter, text| {
            let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) else { return };
            ed.lsp_edit_insert(&d, iter, text);
            if d.is_loading() || !d.is_loaded() || !ed.is_active(&d) {
                return;
            }
            let mut chars = text.chars();
            let one = chars.next().filter(|_| chars.next().is_none());
            let ident = one.is_some_and(symbols::is_ident_char);
            let trigger = one.is_some() && ed.lsp_trigger_char(&d, text);
            if ident || trigger {
                ed.schedule_completion(&d, trigger);
            } else {
                ed.hide_completion();
            }
        });
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.buffer.connect_delete_range(move |_, start, end| {
            let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) else { return };
            ed.lsp_edit_delete(&d, start, end);
            if !d.is_loading() && ed.completion_visible() {
                ed.schedule_completion(&d, false);
            }
        });
        let ed = Rc::downgrade(self);
        *d.key_hook.borrow_mut() = Some(Box::new(move |key, state| ed.upgrade().is_some_and(|ed| ed.completion_key(key, state))));
        let ed = Rc::downgrade(self);
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            if let Some(ed) = ed.upgrade() {
                ed.hide_completion();
            }
        });
        d.view.add_controller(focus);
    }

    // ------------------------------------------------------------ symbols

    /// Scans the files of the indexed working folder for definitions, in
    /// the background. Only new files and files changed since the last scan
    /// are read again.
    pub(super) fn rebuild_symbols(self: &Rc<Self>, root: PathBuf, paths: finder::Paths) {
        if self.symbols_building.replace(true) {
            return;
        }
        let old = self.symbols.borrow().as_ref().filter(|s| s.root == root).map(|s| s.project.borrow().stamps()).unwrap_or_default();
        let ed = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let r = root.clone();
            let scan = gio::spawn_blocking(move || {
                let total = paths.len();
                let workers = finder::worker_count(total);
                let next_idx = std::sync::atomic::AtomicUsize::new(0);
                let run_worker = || -> Vec<(usize, symbols::Stamp, FileSymbols)> {
                    let mut pbuf = finder::root_prefix(&r);
                    let base_len = pbuf.len();
                    let mut buf = Vec::new();
                    let mut scratch = String::new();
                    let mut out = Vec::new();
                    loop {
                        let start = next_idx.fetch_add(finder::WORKER_CHUNK, std::sync::atomic::Ordering::Relaxed);
                        if start >= total {
                            break;
                        }
                        let end = (start + finder::WORKER_CHUNK).min(total);
                        for idx in start..end {
                            let rel = paths.get(idx);
                            let path = finder::join_rel(&mut pbuf, base_len, rel);
                            let meta = std::fs::metadata(path).ok();
                            let stamp = meta.as_ref().map_or((0, 0), symbols::stamp_of);
                            if let Some(&(s, prev)) = old.get(rel)
                                && s == stamp
                            {
                                out.push((idx, stamp, FileSymbols::Same(prev)));
                                continue;
                            }
                            let l = lang::detect(Some(path), "");
                            if l == Lang::Plain || meta.is_none_or(|m| m.len() > MAX_SYMBOL_FILE) {
                                out.push((idx, stamp, FileSymbols::New(Vec::new())));
                                continue;
                            }
                            let syms = finder::read_text_buf(path, &mut buf, &mut scratch)
                                .map(|t| symbols::extract_file(l, path, t))
                                .unwrap_or_default();
                            out.push((idx, stamp, FileSymbols::New(syms)));
                        }
                    }
                    out
                };
                let parts = if workers <= 1 {
                    vec![run_worker()]
                } else {
                    std::thread::scope(|s| {
                        let handles: Vec<_> = (1..workers).map(|_| s.spawn(run_worker)).collect();
                        let mut res = Vec::with_capacity(workers);
                        res.push(run_worker());
                        for h in handles {
                            if let Ok(part) = h.join() {
                                res.push(part);
                            }
                        }
                        res
                    })
                };
                let mut stamps = vec![(0, 0); total];
                let mut per_file: Vec<FileSymbols> = (0..total).map(|_| FileSymbols::New(Vec::new())).collect();
                for part in parts {
                    for (idx, stamp, fs) in part {
                        stamps[idx] = stamp;
                        per_file[idx] = fs;
                    }
                }
                let sorted = symbols::sort_new(&mut per_file);
                (paths, stamps, per_file, sorted)
            })
            .await;
            let Some(ed) = ed.upgrade() else { return };
            ed.symbols_building.set(false);
            let Ok((files, stamps, per_file, sorted)) = scan else { return };
            // The folder may have been closed meanwhile.
            if ed.workdir.borrow().as_deref() != Some(root.as_path()) {
                return;
            }
            let current = ed.symbols.borrow().clone().filter(|s| s.root == root);
            match current {
                Some(s) => {
                    let mut p = s.project.borrow_mut();
                    if !p.unchanged(&files, &per_file) {
                        p.apply_scan(files, stamps, &per_file, sorted);
                    }
                }
                None => {
                    let mut p = ProjectSymbols::default();
                    p.apply_scan(files, stamps, &per_file, sorted);
                    *ed.symbols.borrow_mut() = Some(Rc::new(Symbols { root, project: RefCell::new(p) }));
                }
            }
            ed.release_memory();
        });
    }

    /// Drops the symbols when the folder is closed or no more indexed.
    pub(super) fn drop_symbols(&self) {
        let keep = {
            let wd = self.workdir.borrow();
            self.symbols.borrow().as_ref().is_some_and(|s| Some(s.root.as_path()) == wd.as_deref()) && self.index_active()
        };
        if !keep {
            *self.symbols.borrow_mut() = None;
        }
    }

    /// Updates the symbols of a saved document.
    pub(super) fn symbols_saved(&self, d: &Doc) {
        let Some(s) = self.symbols.borrow().clone() else { return };
        let Some(path) = d.path.borrow().clone() else { return };
        let Ok(rel) = path.strip_prefix(&s.root) else { return };
        let syms = symbols::extract_file(d.lang.get(), &path, &d.text());
        let stamp = std::fs::metadata(&path).map_or((0, 0), |m| symbols::stamp_of(&m));
        s.project.borrow_mut().update_file(&rel.to_string_lossy(), stamp, syms);
    }

    // --------------------------------------------------- definition, usages

    /// Folder for project-wide answers: the working folder or the folder of
    /// the document.
    pub(super) fn code_root(&self, d: &Doc) -> Option<PathBuf> {
        self.workdir
            .borrow()
            .clone()
            .or_else(|| d.path.borrow().as_deref().and_then(Path::parent).map(Path::to_path_buf))
    }

    pub(super) fn goto_definition(self: &Rc<Self>) {
        let Some(d) = self.active_doc() else { return };
        // A component in a Vue template: <carrello-riga> → CarrelloRiga.
        if let Some(name) = component_tag_at_cursor(&d) {
            if !self.lsp_definition(&d, &name) {
                self.local_definition(&d, &name);
            }
            return;
        }
        let Some((word, _, _)) = word_at_cursor(&d) else {
            self.message("Nessun nome sotto il cursore");
            return;
        };
        if self.lsp_definition(&d, &word) {
            return;
        }
        self.local_definition(&d, &word);
    }

    /// Definitions of `d`: reused across keystrokes (`!exact`) within
    /// `SYMBOL_CACHE_TTL`, or refreshed immediately when `exact` is requested
    /// and the buffer changed.
    fn doc_symbols_for(&self, d: &Doc, exact: bool) -> Rc<[Symbol]> {
        if !exact && d.buffer.char_count() > MAX_SYMBOL_FILE as i32 {
            return Rc::from([]);
        }
        let id = d.id.borrow();
        let lang = d.lang.get();
        let edit_gen = d.edit_gen();
        if let Some(c) = self.doc_symbols.borrow().as_ref()
            && c.doc_id == *id
            && c.lang == lang
            && (c.edit_gen == edit_gen || (!exact && c.computed_at.elapsed() < SYMBOL_CACHE_TTL))
        {
            return c.symbols.clone();
        }
        drop(id);
        let doc_file = d.path.borrow().clone().unwrap_or_default();
        let syms: Rc<[Symbol]> = symbols::extract_file(lang, &doc_file, &d.text()).into();
        *self.doc_symbols.borrow_mut() = Some(DocSymbols {
            doc_id: d.id.borrow().clone(),
            lang,
            edit_gen,
            computed_at: Instant::now(),
            symbols: syms.clone(),
        });
        syms
    }

    /// Definitions by name: in the document, then in the project.
    pub(super) fn local_definition(self: &Rc<Self>, d: &Rc<Doc>, word: &str) {
        let doc_path = d.path.borrow().clone();
        let mut found: Vec<(Option<PathBuf>, Symbol)> = self
            .doc_symbols_for(d, true)
            .iter()
            .filter(|s| s.name == word)
            .map(|s| (doc_path.clone(), s.clone()))
            .collect();
        let project = self.symbols.borrow().clone();
        if let Some(p) = &project {
            let proj = p.project.borrow();
            for e in proj.find(word) {
                let path = p.root.join(proj.files.get(e.file as usize));
                // The open document is more recent than the index.
                if Some(&path) != doc_path.as_ref() {
                    found.push((Some(path), e.to_symbol()));
                }
            }
        }
        match found.len() {
            0 => self.message(&if project.is_some() {
                format!("Definizione di «{word}» non trovata")
            } else {
                format!("Definizione di «{word}» non trovata nel documento (indicizza la cartella di lavoro per cercarla nel progetto)")
            }),
            1 => {
                let (path, s) = &found[0];
                self.jump_to(d, path.as_deref(), s.line, s.col);
            }
            _ => {
                let root = self.code_root(d).unwrap_or_else(|| PathBuf::from("/"));
                let res = definition_results(&root, d, &found);
                self.show_text_results(&root, &format!("Definizioni di «{word}»"), word, res);
            }
        }
    }

    /// Moves to `line`, `col` of `path` (or of `d` when `path` is None or
    /// is the path of `d`).
    pub(super) fn jump_to(self: &Rc<Self>, d: &Rc<Doc>, path: Option<&Path>, line: u32, col: u32) {
        match path {
            Some(p) if d.path.borrow().as_deref() != Some(p) => self.open_at(p, line, col),
            _ => {
                let buf = &d.buffer;
                buf.place_cursor(&d.iter_at_line_col(line as i32, col as i32));
                d.view.scroll_to_mark(&buf.get_insert(), 0.1, true, 0.0, 0.4);
                d.view.grab_focus();
            }
        }
    }

    pub(super) fn find_usages(self: &Rc<Self>) {
        let Some(d) = self.active_doc() else { return };
        let Some((word, _, _)) = word_at_cursor(&d) else {
            self.message("Nessun nome sotto il cursore");
            return;
        };
        if self.lsp_references(&d, &word) {
            return;
        }
        let Some(root) = self.code_root(&d) else {
            self.message("Salva il documento o apri una cartella di lavoro per cercare gli utilizzi");
            return;
        };
        self.search_usages(&root, &word);
    }

    // ---------------------------------------------------------- completion

    fn completion(self: &Rc<Self>) -> Rc<Completion> {
        if let Some(c) = self.completion.borrow().clone() {
            return c;
        }
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Browse);
        list.set_can_focus(false);
        list.add_css_class("completion");
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .max_content_height(260)
            .build();
        let popover = gtk::Popover::builder().child(&scroll).autohide(false).has_arrow(false).position(gtk::PositionType::Bottom).build();
        popover.set_can_focus(false);
        let c = Rc::new(Completion {
            popover,
            list,
            scroll,
            items: RefCell::new(Vec::new()),
            doc: RefCell::new(Weak::new()),
            start: Cell::new(0),
            generation: Cell::new(0),
            requests: RefCell::new(Vec::new()),
        });
        let ed = Rc::downgrade(self);
        c.list.connect_row_activated(move |_, row| {
            if let Some(ed) = ed.upgrade() {
                ed.accept_completion(row.index().max(0) as usize);
            }
        });
        *self.completion.borrow_mut() = Some(c.clone());
        c
    }

    fn cancel_completion_timer(&self) {
        if let Some(id) = self.completion_timer.take() {
            id.remove();
        }
    }

    fn schedule_completion(self: &Rc<Self>, d: &Rc<Doc>, forced: bool) {
        self.cancel_completion_timer();
        let (ed, d) = (self.clone(), d.clone());
        let id = if forced {
            glib::idle_add_local_once(move || {
                ed.completion_timer.set(None);
                ed.update_completion(&d, true);
            })
        } else {
            glib::timeout_add_local_once(COMPLETION_DELAY, move || {
                ed.completion_timer.set(None);
                ed.update_completion(&d, false);
            })
        };
        self.completion_timer.set(Some(id));
    }

    pub(super) fn completion_visible(&self) -> bool {
        self.completion.borrow().as_ref().is_some_and(|c| c.popover.is_visible())
    }

    pub(super) fn hide_completion(&self) {
        self.cancel_completion_timer();
        if let Some(c) = self.completion.borrow().as_ref() {
            c.generation.set(c.generation.get() + 1);
            c.popover.popdown();
        }
    }

    /// Keys while the list is shown: move, accept, close.
    fn completion_key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> bool {
        if !self.completion_visible() || state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
            return false;
        }
        let Some(c) = self.completion.borrow().clone() else { return false };
        let n = c.items.borrow().len() as i32;
        let cur = c.list.selected_row().map_or(0, |r| r.index());
        let select = |i: i32| {
            if let Some(row) = c.list.row_at_index(i.clamp(0, n - 1)) {
                c.list.select_row(Some(&row));
                c.scroll_to(&row);
            }
        };
        match key {
            gdk::Key::Down | gdk::Key::KP_Down => select(cur + 1),
            gdk::Key::Up | gdk::Key::KP_Up => select(cur - 1),
            gdk::Key::Page_Down => select(cur + 8),
            gdk::Key::Page_Up => select(cur - 8),
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab => self.accept_completion(cur.max(0) as usize),
            gdk::Key::Escape => self.hide_completion(),
            _ => return false,
        }
        true
    }

    /// Shows (or updates) the list for the word before the cursor.
    /// `forced`: Ctrl+Space or a trigger character such as `.`.
    pub(super) fn update_completion(self: &Rc<Self>, d: &Rc<Doc>, forced: bool) {
        if !forced && !self.shared.settings.borrow().autocomplete {
            return;
        }
        if !self.is_active(d) || matches!(d.lang.get(), Lang::Plain) {
            self.hide_completion();
            return;
        }
        let (prefix, start) = prefix_at_cursor(d);
        if !forced && prefix.chars().count() < MIN_PREFIX && !self.completion_visible() {
            return;
        }
        let c = self.completion();
        c.generation.set(c.generation.get() + 1);
        c.start.set(start);
        *c.doc.borrow_mut() = Rc::downgrade(d);
        if self.lsp_completion(d, c.generation.get()) {
            return;
        }
        if prefix.is_empty() && !forced {
            self.hide_completion();
            return;
        }
        let items = self.local_items(d, &prefix);
        self.show_completion(d, items);
    }

    /// Proposals without a language server: definitions and words of the
    /// document, then symbols of the project, then keywords.
    pub(super) fn local_items(&self, d: &Doc, prefix: &str) -> Vec<Item> {
        let lower = prefix.to_lowercase();
        // name -> (rank, detail)
        let mut best: HashMap<String, (u8, String)> = HashMap::new();
        let mut add = |name: &str, rank: u8, detail: &dyn Fn() -> String| {
            if name == prefix || !symbols::starts_with_lower(name, &lower) {
                return;
            }
            let rank = rank + if name.starts_with(prefix) { 0 } else { 4 };
            if let Some(e) = best.get_mut(name) {
                if rank < e.0 {
                    *e = (rank, detail());
                }
            } else {
                best.insert(name.to_string(), (rank, detail()));
            }
        };
        for s in self.doc_symbols_for(d, false).iter() {
            add(&s.name, 0, &|| s.kind.label().to_string());
        }
        if let Some(p) = self.symbols.borrow().as_ref() {
            let project = p.project.borrow();
            for e in project.with_prefix(prefix, 300) {
                add(e.name, 2, &|| {
                    let file = project.files.get(e.file as usize).rsplit('/').next().unwrap_or("");
                    format!("{} · {file}", e.kind.label())
                });
            }
        }
        let near = text_near_cursor(d);
        for w in symbols::words(&near, MIN_PREFIX, &lower) {
            add(w, 1, &String::new);
        }
        let spec = d.lang.get().spec();
        for (list, label) in [(spec.keywords, "parola chiave"), (spec.types, "tipo"), (spec.constants, "costante"), (spec.builtins, "funzione")] {
            for k in list {
                add(k, 3, &|| label.to_string());
            }
        }
        let mut items: Vec<(u8, String, String)> = best.into_iter().map(|(n, (r, det))| (r, n, det)).collect();
        items.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(&b.1)));
        items
            .into_iter()
            .take(MAX_ITEMS)
            .map(|(_, label, detail)| Item { insert: label.clone(), label, detail, replace_from: None })
            .collect()
    }

    /// Fills and shows the popup under the cursor of `d`.
    pub(super) fn show_completion(self: &Rc<Self>, d: &Rc<Doc>, items: Vec<Item>) {
        if items.is_empty() || !self.is_active(d) {
            self.hide_completion();
            return;
        }
        let c = self.completion();
        // The rows are kept and reused: only the texts change while typing.
        for (i, it) in items.iter().enumerate() {
            let row = match c.list.row_at_index(i as i32) {
                Some(r) => r,
                None => {
                    c.list.append(&completion_row());
                    c.list.row_at_index(i as i32).expect("row just added")
                }
            };
            let Some(b) = row.child() else { continue };
            if let Some(l) = b.first_child().and_downcast::<gtk::Label>() {
                l.set_text(&it.label);
            }
            if let Some(l) = b.last_child().and_downcast::<gtk::Label>() {
                l.set_text(&it.detail);
            }
            row.set_visible(true);
        }
        let mut extra = c.list.row_at_index(items.len() as i32);
        while let Some(r) = extra {
            r.set_visible(false);
            extra = r.next_sibling().and_downcast::<gtk::ListBoxRow>();
        }
        *c.items.borrow_mut() = items;
        if let Some(r) = c.list.row_at_index(0) {
            c.list.select_row(Some(&r));
        }
        c.scroll.vadjustment().set_value(0.0);
        // Under the cursor.
        let buf = &d.buffer;
        let it = buf.iter_at_mark(&buf.get_insert());
        let loc = d.view.iter_location(&it);
        let (x, y) = d.view.buffer_to_window_coords(gtk::TextWindowType::Widget, loc.x(), loc.y());
        if c.popover.parent().as_ref() != Some(d.view.upcast_ref()) {
            if c.popover.parent().is_some() {
                c.popover.unparent();
            }
            c.popover.set_parent(&d.view);
        }
        c.popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, loc.height())));
        if !c.popover.is_visible() {
            c.popover.popup();
        }
        d.view.grab_focus();
    }

    fn accept_completion(self: &Rc<Self>, i: usize) {
        let Some(c) = self.completion.borrow().clone() else { return };
        let item = c.items.borrow().get(i).cloned();
        let d = c.doc.borrow().upgrade();
        self.hide_completion();
        let (Some(item), Some(d)) = (item, d) else { return };
        let buf = &d.buffer;
        let cur = buf.iter_at_mark(&buf.get_insert());
        let mut a = buf.iter_at_offset(item.replace_from.unwrap_or(c.start.get()).min(cur.offset()));
        let mut b = cur;
        // Replace the rest of the word too.
        while symbols::is_ident_char(b.char()) && b.forward_char() {}
        buf.begin_user_action();
        buf.delete(&mut a, &mut b);
        buf.insert(&mut a, &item.insert);
        buf.end_user_action();
    }

    /// Releases the completion popup (e.g. when its document closes).
    pub(super) fn forget_completion(&self, d: &Doc) {
        self.cancel_completion_timer();
        if self.doc_symbols.borrow().as_ref().is_some_and(|s| s.doc_id == *d.id.borrow()) {
            *self.doc_symbols.borrow_mut() = None;
        }
        let c = self.completion.borrow().clone();
        if let Some(c) = c
            && c.popover.parent().as_ref() == Some(d.view.upcast_ref())
        {
            c.popover.popdown();
            c.popover.unparent();
        }
    }
}

/// Results for several definitions, shown like search results.
fn definition_results(root: &Path, d: &Doc, found: &[(Option<PathBuf>, Symbol)]) -> TextResults {
    let mut res = TextResults::default();
    let doc_path = d.path.borrow().clone();
    // Texts of the other files, read once each.
    let mut texts: HashMap<&Path, String> = HashMap::new();
    for (path, s) in found {
        let line = match path {
            Some(p) if doc_path.as_deref() != Some(p.as_path()) => {
                let text = texts.entry(p.as_path()).or_insert_with(|| finder::read_text(p).unwrap_or_default());
                text.lines().nth(s.line as usize).unwrap_or("").to_string()
            }
            _ => d.line_text(s.line as i32),
        };
        // The file may have changed since its symbols were read: `snippet`
        // keeps the positions inside the line.
        let start = line.char_indices().nth(s.col as usize).map_or(line.len(), |(b, _)| b);
        let (snippet, hl, col) = finder::snippet(&line, start, start + s.name.len());
        let rel = match path {
            Some(p) => finder::rel_or_full(root, p),
            None => d.title(),
        };
        let file = res.file_index(rel);
        res.matches.push(TextMatch { file, line: s.line, col, snippet, hl });
    }
    res
}

#[cfg(debug_assertions)]
impl Editor {
    pub(super) fn debug_inspect(&self) {
        let syms = self.symbols.borrow().as_ref().map_or(0, |s| s.project.borrow().len());
        let items: Vec<String> = self
            .completion
            .borrow()
            .as_ref()
            .filter(|c| c.popover.is_visible())
            .map(|c| c.items.borrow().iter().map(|i| format!("{} [{}]", i.label, i.detail)).collect())
            .unwrap_or_default();
        let pos = self.active_doc().map(|d| {
            let it = d.cursor_iter();
            (d.title(), it.line() + 1, it.line_offset() + 1)
        });
        let focus = gtk::prelude::GtkWindowExt::focus(&self.win).map(|w| w.type_().name().to_string());
        let popup = self.completion.borrow().as_ref().filter(|c| c.popover.is_visible()).map(|c| {
            let r = c.popover.pointing_to().1;
            (c.popover.width(), c.popover.height(), r.x(), r.y())
        });
        eprintln!(
            "INSPECT simboli={syms} cursore={pos:?} completamento={items:?} focus={focus:?} popup(l,a,x,y)={popup:?} lsp={:?} mem={:?} msg={:?}",
            self.status.lsp.text(),
            self.status.mem.text(),
            self.status.message.text()
        );
    }

    pub(super) fn debug_shot_popup(&self, path: &str) {
        let Some(c) = self.completion.borrow().clone() else { return };
        let Some(child) = c.popover.child() else { return };
        let (w, h) = (child.width().max(1), child.height().max(1));
        let paintable = gtk::WidgetPaintable::new(Some(&child));
        let snap = gtk::Snapshot::new();
        snap.append_color(&gdk::RGBA::new(0.2, 0.2, 0.22, 1.0), &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
        paintable.snapshot(&snap, w as f64, h as f64);
        let (Some(node), Some(renderer)) = (snap.to_node(), self.win.native().and_then(|n| n.renderer())) else { return };
        let tex = renderer.render_texture(node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
        let _ = tex.save_to_png(path);
        eprintln!("popup {w}x{h} -> {path}");
    }
}
