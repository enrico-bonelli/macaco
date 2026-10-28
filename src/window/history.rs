//! History of a file in its git repository (Git → Cronologia del file):
//! the commits that changed it, renames followed, and for the selected one
//! the changes it made, the whole file at that version and its differences
//! from the current text. A version can be restored into the editor.

use super::Editor;
use crate::config;
use crate::diff;
use crate::doc::Doc;
use crate::git::{self, LogEntry};
use crate::lang;
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Commits listed (the most recent ones).
const MAX_COMMITS: usize = 500;
/// Larger outputs are cut (a huge generated file).
const MAX_TEXT: usize = 4 << 20;

pub(super) struct History {
    ed: Weak<Editor>,
    pub(super) win: gtk::Window,
    root: PathBuf,
    /// The file (absolute) and its current path in the repository.
    file: PathBuf,
    rel: String,
    entries: RefCell<Vec<LogEntry>>,
    list: gtk::ListBox,
    filter: gtk::SearchEntry,
    title: gtk::Label,
    /// Full message of the selected commit.
    message: gtk::Label,
    stack: gtk::Stack,
    patch_view: gtk::TextView,
    current_view: gtk::TextView,
    /// The file at the selected version (read-only, highlighted).
    version: Rc<Doc>,
    restore: gtk::Button,
    copy: gtk::Button,
    selected: Cell<Option<usize>>,
    /// Commit message and patch by hash.
    patches: RefCell<HashMap<String, (String, String)>>,
    /// File content by hash.
    contents: RefCell<HashMap<String, String>>,
    /// Answers for an older selection are dropped.
    generation: Cell<u32>,
}

fn cut(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut n = MAX_TEXT;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
        s.push_str("\n… (troncato)\n");
    }
    s
}

/// A read-only monospace view for diffs, sharing the editor tags.
fn diff_view(tags: &gtk::TextTagTable) -> gtk::TextView {
    let buf = gtk::TextBuffer::new(Some(tags));
    let v = gtk::TextView::with_buffer(&buf);
    v.add_css_class("macaco");
    v.set_monospace(true);
    v.set_editable(false);
    v.set_cursor_visible(false);
    v.set_left_margin(8);
    v.set_top_margin(4);
    v.set_bottom_margin(20);
    v
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder().child(child).vexpand(true).hexpand(true).build()
}

impl Editor {
    /// Opens the history of `file` (an open document or a file of the
    /// panel).
    pub(super) fn file_history(self: &Rc<Self>, file: &Path) {
        let Some(st) = self.git_status() else {
            self.message("La cronologia è disponibile per i file di un repository git");
            return;
        };
        let Ok(rel) = file.strip_prefix(&st.root) else {
            self.message("Il file non è nel repository della cartella di lavoro");
            return;
        };
        let rel = rel.to_string_lossy().into_owned();
        if let Some(h) = self.history.borrow().as_ref().filter(|h| h.file == file) {
            h.win.present();
            return;
        }
        if let Some(old) = self.history.borrow_mut().take() {
            old.win.close();
        }
        let root = st.root.clone();
        let (ed, file) = (Rc::downgrade(self), file.to_path_buf());
        self.message(&format!("Lettura della cronologia di {rel}…"));
        let args = git::log_args(&rel, MAX_COMMITS);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        git::run_str(&root.clone(), &argv, move |out| {
            let Some(ed) = ed.upgrade() else { return };
            let entries = git::parse_log(&String::from_utf8_lossy(&out.stdout), &rel);
            if !out.ok || entries.is_empty() {
                let why = if out.ok { "nessun commit: il file non è ancora nel repository".to_string() } else { out.summary() };
                ed.message(&format!("Cronologia di {rel}: {why}"));
                return;
            }
            ed.message("");
            let h = History::new(&ed, root, file, rel, entries);
            *ed.history.borrow_mut() = Some(h.clone());
            h.win.present();
        });
    }

    /// The file of the active tab.
    pub(super) fn active_file_history(self: &Rc<Self>) {
        match self.active_doc().and_then(|d| d.path.borrow().clone()) {
            Some(p) => self.file_history(&p),
            None => self.message("Salva il file per vederne la cronologia"),
        }
    }
}

impl History {
    fn new(ed: &Rc<Editor>, root: PathBuf, file: PathBuf, rel: String, entries: Vec<LogEntry>) -> Rc<History> {
        let name = file.file_name().map_or_else(|| rel.clone(), |n| n.to_string_lossy().into_owned());
        let win = gtk::Window::builder()
            .title(format!("Cronologia — {name}"))
            .transient_for(&ed.win)
            .default_width(1150)
            .default_height(720)
            .build();

        // Left: the commits.
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(pango::EllipsizeMode::Start);
        title.add_css_class("heading");
        let filter = gtk::SearchEntry::builder().placeholder_text("Filtra per messaggio, autore, data o hash").build();
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        let list_scroll = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
        list_scroll.add_css_class("frame");
        let left = gtk::Box::new(gtk::Orientation::Vertical, 6);
        left.set_width_request(320);
        left.append(&title);
        left.append(&filter);
        left.append(&list_scroll);

        // Right: the selected commit.
        let message = gtk::Label::new(None);
        message.set_xalign(0.0);
        message.set_wrap(true);
        message.set_wrap_mode(pango::WrapMode::WordChar);
        message.set_selectable(true);
        message.set_lines(6);
        message.set_ellipsize(pango::EllipsizeMode::End);
        let restore = gtk::Button::with_label("Ripristina questa versione");
        restore.set_tooltip_text(Some("Sostituisce il testo nell'editor con questa versione (Ctrl+Z per annullare; non salva)"));
        let copy = gtk::Button::with_label("Copia hash");
        let stack = gtk::Stack::new();
        let tags = &ed.shared.tags.table;
        let patch_view = diff_view(tags);
        let current_view = diff_view(tags);
        let version = Doc::new(&ed.shared, config::new_id());
        version.view.set_editable(false);
        version.apply_settings();
        stack.add_titled(&scrolled(&patch_view), Some("patch"), "Modifiche del commit");
        stack.add_titled(&version.scroller, Some("version"), "Versione completa");
        stack.add_titled(&scrolled(&current_view), Some("current"), "Confronto con il file attuale");
        let switcher = gtk::StackSwitcher::builder().stack(&stack).build();
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&switcher);
        bar.append(&spacer);
        bar.append(&copy);
        bar.append(&restore);
        let frame = gtk::Frame::new(None);
        frame.set_child(Some(&stack));
        let right = gtk::Box::new(gtk::Orientation::Vertical, 6);
        right.append(&message);
        right.append(&bar);
        right.append(&frame);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&left));
        paned.set_end_child(Some(&right));
        paned.set_position(360);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_margin_top(10);
        paned.set_margin_bottom(10);
        paned.set_margin_start(10);
        paned.set_margin_end(10);
        win.set_child(Some(&paned));

        let h = Rc::new(History {
            ed: Rc::downgrade(ed),
            win,
            root,
            file,
            rel,
            entries: RefCell::new(entries),
            list,
            filter,
            title,
            message,
            stack,
            patch_view,
            current_view,
            version,
            restore,
            copy,
            selected: Cell::new(None),
            patches: RefCell::new(HashMap::new()),
            contents: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
        });
        h.fill();
        h.connect();
        h
    }

    fn fill(&self) {
        let entries = self.entries.borrow();
        let n = entries.len();
        let more = if n >= MAX_COMMITS { format!(" (gli ultimi {MAX_COMMITS})") } else { String::new() };
        self.title.set_text(&format!("{} — {n} commit{more}", self.rel));
        for e in entries.iter() {
            let subject = gtk::Label::new(Some(&e.subject));
            subject.set_xalign(0.0);
            subject.set_ellipsize(pango::EllipsizeMode::End);
            let renamed = if e.path != self.rel { format!(" · era {}", e.path) } else { String::new() };
            let info = gtk::Label::new(Some(&format!("{} · {} · {}{renamed}", e.short, e.date, e.author)));
            info.set_xalign(0.0);
            info.set_ellipsize(pango::EllipsizeMode::End);
            info.add_css_class("dim-label");
            let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
            b.set_margin_top(4);
            b.set_margin_bottom(4);
            b.set_margin_start(6);
            b.set_margin_end(6);
            b.append(&subject);
            b.append(&info);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&b));
            row.set_tooltip_text(Some(&format!("{}\n{}", e.hash, e.subject)));
            self.list.append(&row);
        }
    }

    fn connect(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.list.connect_row_selected(move |_, row| {
            if let (Some(h), Some(row)) = (w.upgrade(), row) {
                h.select(row.index() as usize);
            }
        });
        let w = Rc::downgrade(self);
        self.stack.connect_visible_child_name_notify(move |_| {
            if let Some(h) = w.upgrade() {
                h.show_page();
            }
        });
        let w = Rc::downgrade(self);
        self.list.set_filter_func(move |row| {
            let Some(h) = w.upgrade() else { return true };
            let q = h.filter.text().to_lowercase();
            if q.is_empty() {
                return true;
            }
            h.entries.borrow().get(row.index() as usize).is_some_and(|e| {
                [&e.subject, &e.author, &e.date, &e.hash].iter().any(|s| s.to_lowercase().contains(&q))
            })
        });
        let l = self.list.clone();
        self.filter.connect_search_changed(move |_| l.invalidate_filter());
        let w = Rc::downgrade(self);
        self.copy.connect_clicked(move |b| {
            if let Some(h) = w.upgrade()
                && let Some(e) = h.current_entry()
            {
                b.clipboard().set_text(&e.hash);
                if let Some(ed) = h.ed.upgrade() {
                    ed.message(&format!("Hash copiato: {}", e.hash));
                }
            }
        });
        let w = Rc::downgrade(self);
        self.restore.connect_clicked(move |_| {
            if let Some(h) = w.upgrade() {
                h.restore_version();
            }
        });
        let w = Rc::downgrade(self);
        self.win.connect_close_request(move |_| {
            if let Some(h) = w.upgrade()
                && let Some(ed) = h.ed.upgrade()
                && ed.history.borrow().as_ref().is_some_and(|x| Rc::ptr_eq(x, &h))
            {
                *ed.history.borrow_mut() = None;
            }
            glib::Propagation::Proceed
        });
        // Escape closes.
        let keys = gtk::EventControllerKey::new();
        let w = self.win.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape
                && let Some(w) = w.upgrade()
            {
                w.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.win.add_controller(keys);
        if let Some(r) = self.list.row_at_index(0) {
            self.list.select_row(Some(&r));
        }
    }

    fn current_entry(&self) -> Option<LogEntry> {
        self.entries.borrow().get(self.selected.get()?).cloned()
    }

    fn select(self: &Rc<Self>, i: usize) {
        self.selected.set(Some(i));
        self.generation.set(self.generation.get() + 1);
        let Some(e) = self.current_entry() else { return };
        self.message.set_text(&e.subject);
        self.show_page();
        // Message and patch are needed by every page (the patch marks the
        // changed lines of the whole version).
        if !self.patches.borrow().contains_key(&e.hash) {
            let (w, wanted, hash) = (Rc::downgrade(self), self.generation.get(), e.hash.clone());
            // With the name in the previous commit too, a rename is shown
            // as such and not as a whole new file.
            let before = self.entries.borrow().get(i + 1).map(|o| o.path.clone()).filter(|p| *p != e.path);
            let mut args = vec!["show", "--no-color", "-M", "-U3", "--format=%B%x00", &e.hash, "--", &e.path];
            if let Some(b) = before.as_deref() {
                args.push(b);
            }
            git::run_str(&self.root, &args, move |out| {
                let Some(h) = w.upgrade() else { return };
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                let (msg, patch) = text.split_once('\0').map_or((String::new(), text.clone()), |(m, p)| (m.trim().to_string(), p.trim_start_matches('\n').to_string()));
                let patch = if out.ok { cut(patch) } else { format!("git show non riuscito: {}", out.summary()) };
                h.patches.borrow_mut().insert(hash, (msg, patch));
                if h.generation.get() == wanted {
                    h.show_page();
                }
            });
        }
    }

    /// Fetch the file at the selected version, then call `then`.
    fn with_content(self: &Rc<Self>, e: &LogEntry, then: impl FnOnce(&Rc<History>, &str) + 'static) {
        if let Some(c) = self.contents.borrow().get(&e.hash).cloned() {
            then(self, &c);
            return;
        }
        let (w, wanted, hash) = (Rc::downgrade(self), self.generation.get(), e.hash.clone());
        let spec = format!("{}:{}", e.hash, e.path);
        git::run_str(&self.root, &["show", &spec], move |out| {
            let Some(h) = w.upgrade() else { return };
            let text = if !out.ok {
                format!("(versione non disponibile: {})", out.summary())
            } else if out.stdout.contains(&0) {
                "(file binario)".to_string()
            } else {
                cut(String::from_utf8_lossy(&out.stdout).into_owned())
            };
            h.contents.borrow_mut().insert(hash, text.clone());
            if h.generation.get() == wanted {
                then(&h, &text);
            }
        });
    }

    /// Render the visible page for the selected commit.
    fn show_page(self: &Rc<Self>) {
        let Some(e) = self.current_entry() else { return };
        let patch = self.patches.borrow().get(&e.hash).cloned();
        if let Some((msg, _)) = &patch {
            self.message.set_text(if msg.is_empty() { &e.subject } else { msg });
        }
        match self.stack.visible_child_name().as_deref() {
            Some("version") => {
                let Some((_, patch)) = patch else { return };
                let entry = e.clone();
                self.with_content(&e, move |h, text| h.show_version(&entry, text, &patch));
            }
            Some("current") => {
                let entry = e.clone();
                self.with_content(&e, move |h, text| h.show_current(&entry, text));
            }
            _ => {
                let text = patch.map_or_else(|| "Caricamento…".to_string(), |(_, p)| p);
                show_diff(&self.patch_view, &text);
            }
        }
    }

    fn show_version(&self, e: &LogEntry, text: &str, patch: &str) {
        let d = &self.version;
        d.set_lang(lang::detect(Some(Path::new(&e.path)), &text.chars().take(256).collect::<String>()));
        d.set_content(text, false, 0);
        // The lines this commit added or changed.
        let buf = &d.buffer;
        let tag = &d.shared.tags.diff_add;
        let added = git::added_lines(patch);
        for &l in &added {
            if let Some(a) = buf.iter_at_line(l as i32) {
                let mut b = a;
                b.forward_line();
                buf.apply_tag(tag, &a, &b);
            }
        }
        let first = added.first().copied().unwrap_or(0) as i32;
        if let Some(it) = buf.iter_at_line(first) {
            buf.place_cursor(&it);
            let view = d.view.clone();
            glib::idle_add_local_once(move || {
                view.scroll_to_mark(&view.buffer().get_insert(), 0.1, true, 0.0, 0.3);
            });
        }
    }

    fn show_current(&self, e: &LogEntry, text: &str) {
        let current = self.current_text();
        let u = diff::unified(text, &current, &format!("{} ({})", e.path, e.short), &format!("{} (attuale)", self.rel));
        let shown = if u.is_empty() { "Nessuna differenza: il file attuale è uguale a questa versione.".to_string() } else { cut(u) };
        show_diff(&self.current_view, &shown);
    }

    /// The text in the editor (unsaved changes included) or on disk.
    fn current_text(&self) -> String {
        let open = self.ed.upgrade().and_then(|ed| ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(self.file.as_path())).cloned());
        match open {
            Some(d) if d.is_loaded() => d.text(),
            _ => crate::finder::read_text(&self.file).unwrap_or_default(),
        }
    }

    fn restore_version(self: &Rc<Self>) {
        let Some(e) = self.current_entry() else { return };
        let short = e.short.clone();
        self.with_content(&e, move |h, text| {
            let Some(ed) = h.ed.upgrade() else { return };
            if text.starts_with("(file binario)") || text.starts_with("(versione non disponibile") {
                ed.message("Questa versione non si può ripristinare");
                return;
            }
            ed.open_path(&h.file);
            let Some(d) = ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(h.file.as_path())).cloned() else { return };
            if d.ensure_loaded().is_err() {
                return;
            }
            d.replace_all_text(text);
            ed.message(&format!("Ripristinata la versione {short} di {}: Ctrl+Z per annullare, salva per confermare", h.rel));
            h.show_page();
        });
    }
}

/// Show a unified diff with added, removed and header lines colored.
fn show_diff(view: &gtk::TextView, text: &str) {
    let buf = view.buffer();
    buf.set_text(text);
    let table = buf.tag_table();
    let tag = |name: &str| table.lookup(name);
    let (add, del, chg) = (tag("diff-add"), tag("diff-del"), tag("diff-chg"));
    let comment = tag("Comment");
    let mut it = buf.start_iter();
    loop {
        let mut end = it;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        let line = buf.text(&it, &end, true);
        const HEADERS: [&str; 11] =
            ["+++", "---", "diff ", "index ", "similarity ", "dissimilarity ", "rename ", "copy ", "new file", "deleted file", "old mode"];
        let t = if HEADERS.iter().any(|h| line.starts_with(h)) || line.starts_with("new mode") {
            comment.as_ref()
        } else if line.starts_with('+') {
            add.as_ref()
        } else if line.starts_with('-') {
            del.as_ref()
        } else if line.starts_with("@@") {
            chg.as_ref()
        } else {
            None
        };
        if let Some(t) = t {
            let mut e = end;
            e.forward_char();
            buf.apply_tag(t, &it, &e);
        }
        if !it.forward_line() {
            break;
        }
    }
}

#[cfg(debug_assertions)]
impl History {
    pub(super) fn debug_page(&self, name: &str) {
        self.stack.set_visible_child_name(name);
    }
}
