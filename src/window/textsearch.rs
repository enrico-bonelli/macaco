//! Text search in the files of the working folder (Ctrl+Shift+F): a modal
//! window where the search is started with a button; the results are
//! listed by line, a click shows the file around the match in a preview,
//! "Apri" opens it in a tab. Closing the window frees everything.

use super::Editor;
use crate::config;
use crate::doc::Doc;
use crate::finder::{self, Progress, TextResults};
use crate::lang;
use crate::search::{Options, Query};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(super) struct TextSearch {
    ed: Weak<Editor>,
    pub(super) win: gtk::Window,
    root: PathBuf,
    entry: gtk::Entry,
    case: gtk::ToggleButton,
    word: gtk::ToggleButton,
    regex: gtk::ToggleButton,
    /// Skip matches in comments and strings.
    code_only: gtk::ToggleButton,
    button: gtk::Button,
    status: gtk::Label,
    store: gtk::StringList,
    sel: gtk::SingleSelection,
    view: gtk::ListView,
    preview_title: gtk::Label,
    preview_open: gtk::Button,
    /// Read-only document showing the selected file.
    preview: Rc<Doc>,
    preview_file: RefCell<Option<PathBuf>>,
    results: RefCell<TextResults>,
    /// Query of the results shown (to highlight the preview).
    query: RefCell<Option<Query>>,
    /// Search running.
    running: RefCell<Option<Arc<Progress>>>,
    started: Cell<Option<std::time::Instant>>,
    generation: Cell<u32>,
}

/// Pango markup of `text` with characters `hl` (start, length) in bold.
fn snippet_markup(text: &str, hl: (u32, u32)) -> String {
    let chars: Vec<char> = text.chars().collect();
    let (s, l) = (hl.0 as usize, hl.1 as usize);
    let part = |a: usize, b: usize| glib::markup_escape_text(&chars[a.min(chars.len())..b.min(chars.len())].iter().collect::<String>()).to_string();
    format!("{}<b>{}</b>{}", part(0, s), part(s, s + l), part(s + l, chars.len()))
}

impl Editor {
    pub(super) fn setup_textsearch_actions(self: &Rc<Self>) {
        let a = gio::SimpleAction::new("find-in-folder", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| ed.find_in_folder());
        self.win.add_action(&a);
        self.app.set_accels_for_action("win.find-in-folder", &["<Control><Shift>f"]);
    }

    fn find_in_folder(self: &Rc<Self>) {
        let Some(root) = self.workdir.borrow().clone() else {
            self.message("La ricerca nei file funziona nella cartella di lavoro (File → Apri cartella di lavoro…)");
            return;
        };
        let open = self.text_search.borrow().clone();
        if let Some(t) = open {
            t.win.present();
            return;
        }
        let t = TextSearch::new(self, root);
        *self.text_search.borrow_mut() = Some(t.clone());
        // Start from the selected text, if any.
        if let Some(d) = self.active_doc()
            && let Some((a, b)) = d.buffer.selection_bounds()
            && a.line() == b.line()
        {
            t.entry.set_text(&d.buffer.text(&a, &b, false));
        }
        t.win.present();
        t.entry.grab_focus();
    }

    /// The search window for `root` (opened if needed).
    fn text_search_window(self: &Rc<Self>, root: &Path) -> Rc<TextSearch> {
        let open = self.text_search.borrow().clone();
        match open {
            Some(t) if t.root == root => t,
            other => {
                if let Some(t) = other {
                    t.win.close();
                }
                let t = TextSearch::new(self, root.to_path_buf());
                *self.text_search.borrow_mut() = Some(t.clone());
                t
            }
        }
    }

    /// "Find usages" by name: whole word, case sensitive, outside comments
    /// and strings, in the files of `root`.
    pub(super) fn search_usages(self: &Rc<Self>, root: &Path, word: &str) {
        let t = self.text_search_window(root);
        t.entry.set_text(word);
        t.case.set_active(true);
        t.word.set_active(true);
        t.regex.set_active(false);
        t.code_only.set_active(true);
        t.win.set_title(Some(&format!("Utilizzi di «{word}»")));
        t.win.present();
        t.start();
    }

    /// Shows ready results (definitions, references from a language server)
    /// in the search window; `word` is highlighted in the preview.
    pub(super) fn show_text_results(self: &Rc<Self>, root: &Path, title: &str, word: &str, res: TextResults) {
        let t = self.text_search_window(root);
        t.stop();
        t.entry.set_text(word);
        t.case.set_active(true);
        t.word.set_active(true);
        t.regex.set_active(false);
        t.win.set_title(Some(title));
        let q = Query::new(word, t.options()).ok();
        t.show(res, q);
        t.win.present();
    }

    /// Opens `path` in a tab with the cursor at `line`, `col` (from 0).
    pub(super) fn open_at(self: &Rc<Self>, path: &Path, line: u32, col: u32) {
        self.open_path(path);
        let Some(d) = self.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path)).cloned() else { return };
        if d.ensure_loaded().is_err() {
            return;
        }
        let buf = &d.buffer;
        let mut it = buf.iter_at_line(line as i32).unwrap_or_else(|| buf.end_iter());
        let mut end = it;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        it.set_line_offset((col as i32).min(end.line_offset()));
        buf.place_cursor(&it);
        let view = d.view.clone();
        // Scroll once the view has its size.
        glib::idle_add_local_once(move || {
            view.scroll_to_mark(&view.buffer().get_insert(), 0.1, true, 0.0, 0.4);
        });
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_text_search(&self, action: &str, n: u32) {
        let t = self.text_search.borrow().clone();
        if let Some(t) = t {
            match action {
                "search" => t.start(),
                "select" => t.sel.set_selected(n),
                "open" => t.open(n),
                "close" => t.win.close(),
                _ => {
                    let r = t.results.borrow();
                    eprintln!("TEXTSEARCH {} risultati in {} file ({} cercati)", r.matches.len(), r.files.len(), r.searched);
                }
            }
        }
    }
}

impl TextSearch {
    fn new(ed: &Rc<Editor>, root: PathBuf) -> Rc<TextSearch> {
        let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
        let win = gtk::Window::builder()
            .title(format!("Cerca nei file — {name}"))
            .transient_for(&ed.win)
            .modal(true)
            .default_width(1050)
            .default_height(680)
            .build();

        let entry = gtk::Entry::builder().placeholder_text("Parola o frase da cercare").hexpand(true).build();
        let toggle = |label: &str, tip: &str| {
            let b = gtk::ToggleButton::with_label(label);
            b.set_tooltip_text(Some(tip));
            b
        };
        let case = toggle("Aa", "Distingui maiuscole e minuscole");
        let word = toggle("Parola", "Solo parole intere");
        let regex = toggle(".*", "Espressione regolare");
        let code_only = toggle("Solo codice", "Esclude le occorrenze nei commenti e nelle stringhe");
        let button = gtk::Button::with_label("Ricerca");
        button.add_css_class("suggested-action");
        button.set_sensitive(false);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for w in [
            entry.upcast_ref::<gtk::Widget>(),
            case.upcast_ref(),
            word.upcast_ref(),
            regex.upcast_ref(),
            code_only.upcast_ref(),
            button.upcast_ref(),
        ] {
            top.append(w);
        }
        let status = gtk::Label::new(Some("Scrivi il testo da cercare e premi «Ricerca»"));
        status.set_xalign(0.0);
        status.add_css_class("dim-label");

        // Results.
        let store = gtk::StringList::new(&[]);
        let sel = gtk::SingleSelection::new(Some(store.clone()));
        sel.set_autoselect(false);
        sel.set_can_unselect(true);
        let view = gtk::ListView::new(Some(sel.clone()), None::<gtk::ListItemFactory>);
        view.add_css_class("text-results");
        let results_scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .width_request(300)
            .build();
        results_scroll.add_css_class("frame");

        // Preview: a read-only document.
        let preview = Doc::new(&ed.shared, config::new_id());
        preview.view.set_editable(false);
        preview.apply_settings();
        let preview_title = gtk::Label::new(Some("Anteprima"));
        preview_title.set_xalign(0.0);
        preview_title.set_hexpand(true);
        preview_title.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        let preview_open = gtk::Button::with_label("Apri");
        preview_open.set_tooltip_text(Some("Apre il file in una scheda, alla riga trovata"));
        preview_open.set_sensitive(false);
        let preview_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        preview_head.append(&preview_title);
        preview_head.append(&preview_open);
        let preview_frame = gtk::Frame::new(None);
        preview_frame.set_child(Some(&preview.scroller));
        let right = gtk::Box::new(gtk::Orientation::Vertical, 6);
        right.append(&preview_head);
        right.append(&preview_frame);

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&results_scroll));
        paned.set_end_child(Some(&right));
        paned.set_position(440);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_vexpand(true);

        let b = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        b.append(&top);
        b.append(&status);
        b.append(&paned);
        win.set_child(Some(&b));

        let t = Rc::new(TextSearch {
            ed: Rc::downgrade(ed),
            win,
            root,
            entry,
            case,
            word,
            regex,
            code_only,
            button,
            status,
            store,
            sel,
            view,
            preview_title,
            preview_open,
            preview,
            preview_file: RefCell::new(None),
            results: RefCell::new(TextResults::default()),
            query: RefCell::new(None),
            running: RefCell::new(None),
            started: Cell::new(None),
            generation: Cell::new(0),
        });
        t.view.set_factory(Some(&t.factory()));

        let w = t.button.downgrade();
        t.entry.connect_changed(move |e| {
            if let Some(b) = w.upgrade() {
                b.set_sensitive(!e.text().trim().is_empty() || b.label().as_deref() != Some("Ricerca"));
            }
        });
        let w = Rc::downgrade(&t);
        t.entry.connect_activate(move |_| {
            if let Some(t) = w.upgrade() {
                t.start();
            }
        });
        let w = Rc::downgrade(&t);
        t.button.connect_clicked(move |_| {
            let Some(t) = w.upgrade() else { return };
            if t.running.borrow().is_some() {
                t.stop();
            } else {
                t.start();
            }
        });
        let w = Rc::downgrade(&t);
        t.sel.connect_selected_notify(move |s| {
            if let Some(t) = w.upgrade() {
                t.show_preview(s.selected());
            }
        });
        let w = Rc::downgrade(&t);
        t.view.connect_activate(move |_, pos| {
            if let Some(t) = w.upgrade() {
                t.open(pos);
            }
        });
        let w = Rc::downgrade(&t);
        t.preview_open.connect_clicked(move |_| {
            if let Some(t) = w.upgrade() {
                t.open(t.sel.selected());
            }
        });
        let esc = gtk::EventControllerKey::new();
        let w = t.win.downgrade();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(w) = w.upgrade() {
                    w.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        t.win.add_controller(esc);
        let w = Rc::downgrade(&t);
        t.win.connect_close_request(move |_| {
            if let Some(t) = w.upgrade() {
                t.stop();
                if let Some(ed) = t.ed.upgrade() {
                    *ed.text_search.borrow_mut() = None;
                    ed.release_memory();
                }
            }
            glib::Propagation::Proceed
        });
        t
    }

    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let f = gtk::SignalListItemFactory::new();
        let w = Rc::downgrade(self);
        f.connect_setup(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let file = gtk::Label::new(None);
            file.set_xalign(0.0);
            file.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            let text = gtk::Label::new(None);
            text.set_xalign(0.0);
            text.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.add_css_class("monospace");
            let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
            col.set_hexpand(true);
            col.append(&file);
            col.append(&text);
            let open = gtk::Button::with_label("Apri");
            open.set_valign(gtk::Align::Center);
            open.set_tooltip_text(Some("Apre il file in una scheda, alla riga trovata"));
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.set_margin_top(3);
            row.set_margin_bottom(3);
            row.append(&col);
            row.append(&open);
            li.set_child(Some(&row));
            let w = w.clone();
            let liw = li.downgrade();
            open.connect_clicked(move |_| {
                if let (Some(t), Some(li)) = (w.upgrade(), liw.upgrade()) {
                    t.open(li.position());
                }
            });
        });
        let w = Rc::downgrade(self);
        f.connect_bind(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let (Some(t), Some(row)) = (w.upgrade(), li.child()) else { return };
            let res = t.results.borrow();
            let Some(m) = res.matches.get(li.position() as usize) else { return };
            let rel = &res.files[m.file as usize];
            let Some(col) = row.first_child() else { return };
            if let Some(l) = col.first_child().and_downcast::<gtk::Label>() {
                let (dir, name) = rel.rsplit_once('/').map_or(("", rel.as_str()), |(d, n)| (d, n));
                l.set_markup(&format!(
                    "<b>{}</b>:{}  <span alpha=\"60%\">{}</span>",
                    glib::markup_escape_text(name),
                    m.line + 1,
                    glib::markup_escape_text(dir)
                ));
            }
            if let Some(l) = col.last_child().and_downcast::<gtk::Label>() {
                l.set_markup(&snippet_markup(&m.snippet, m.hl));
            }
        });
        f
    }

    fn options(&self) -> Options {
        Options { case_sensitive: self.case.is_active(), whole_word: self.word.is_active(), regex: self.regex.is_active() }
    }

    /// Starts the search in a thread; the results replace the list when it
    /// ends.
    fn start(self: &Rc<Self>) {
        let text = self.entry.text().to_string();
        if text.trim().is_empty() || self.running.borrow().is_some() {
            return;
        }
        let q = match Query::new(&text, self.options()) {
            Ok(q) => q,
            Err(e) => {
                self.status.set_text(&format!("Espressione non valida: {e}"));
                return;
            }
        };
        let progress = Arc::new(Progress::default());
        *self.running.borrow_mut() = Some(progress.clone());
        self.started.set(Some(std::time::Instant::now()));
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.button.set_label("Interrompi");
        self.button.set_sensitive(true);
        self.status.set_text("Ricerca in corso…");

        // Progress while the search runs.
        let w = Rc::downgrade(self);
        let p = progress.clone();
        glib::timeout_add_local(Duration::from_millis(150), move || {
            let Some(t) = w.upgrade().filter(|t| t.generation.get() == generation && t.running.borrow().is_some()) else {
                return glib::ControlFlow::Break;
            };
            let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
            t.status.set_text(&if total == 0 { "Elenco dei file…".to_string() } else { format!("Ricerca in corso… {done} di {total} file") });
            glib::ControlFlow::Continue
        });

        let w = Rc::downgrade(self);
        let root = self.root.clone();
        let query = text.clone();
        let opts = self.options();
        let code_only = self.code_only.is_active();
        glib::spawn_future_local(async move {
            let p = progress.clone();
            // The query is compiled again in the thread (a Regex is cheap to
            // build and keeps this code simple).
            let res = gio::spawn_blocking(move || match Query::new(&query, opts) {
                Ok(q) => finder::grep(&root, &q, code_only, &p),
                Err(_) => TextResults::default(),
            })
            .await
            .unwrap_or_default();
            let Some(t) = w.upgrade().filter(|t| t.generation.get() == generation) else { return };
            t.finish(res, q);
        });
    }

    fn stop(&self) {
        if let Some(p) = self.running.borrow().as_ref() {
            p.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn finish(self: &Rc<Self>, res: TextResults, q: Query) {
        self.show(res, Some(q));
    }

    fn show(self: &Rc<Self>, res: TextResults, q: Option<Query>) {
        *self.running.borrow_mut() = None;
        self.button.set_label("Ricerca");
        self.button.set_sensitive(!self.entry.text().trim().is_empty());
        let secs = self.started.take().map(|t| t.elapsed().as_secs_f64());
        let mut status = match (res.matches.len(), secs) {
            (0, Some(secs)) => format!("Nessun risultato ({} file cercati in {secs:.1} s)", res.searched),
            (n, Some(secs)) => format!("{n} righe trovate in {} file ({} file cercati in {secs:.1} s)", res.files.len(), res.searched),
            // Ready results (definitions, references).
            (0, None) => "Nessun risultato".to_string(),
            (n, None) => format!("{n} risultati in {} file", res.files.len()),
        };
        if res.truncated {
            status.push_str(&format!(" · risultati limitati a {}", finder::MAX_TEXT_MATCHES));
        }
        if res.cancelled {
            status.push_str(" · ricerca interrotta");
        }
        self.status.set_text(&status);
        let n = res.matches.len();
        // Results first: binding the rows reads them.
        *self.results.borrow_mut() = res;
        *self.query.borrow_mut() = q;
        let rows: Vec<String> = (0..n).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        self.store.splice(0, self.store.n_items(), &refs);
        self.sel.set_selected(gtk::INVALID_LIST_POSITION);
        if n > 0 {
            self.sel.set_selected(0);
        } else {
            self.clear_preview();
        }
    }

    fn clear_preview(&self) {
        *self.preview_file.borrow_mut() = None;
        self.preview.set_content("", false, 0);
        self.preview_title.set_text("Anteprima");
        self.preview_open.set_sensitive(false);
    }

    /// Path, line and column of result `pos`.
    fn result(&self, pos: u32) -> Option<(PathBuf, u32, u32)> {
        let res = self.results.borrow();
        let m = res.matches.get(pos as usize)?;
        Some((self.root.join(&res.files[m.file as usize]), m.line, m.col))
    }

    /// Shows the file of result `pos` around the match.
    fn show_preview(&self, pos: u32) {
        let Some((path, line, col)) = self.result(pos) else { return };
        let d = &self.preview;
        if self.preview_file.borrow().as_deref() != Some(path.as_path()) {
            let Some(text) = finder::read_text(&path) else {
                self.clear_preview();
                self.preview_title.set_text("File non leggibile");
                return;
            };
            d.set_content(&text, false, 0);
            let head: String = text.chars().take(256).collect();
            d.set_lang(lang::detect(Some(&path), &head));
            // Highlight every occurrence in the file.
            if let Some(q) = self.query.borrow().as_ref() {
                let buf = &d.buffer;
                let tag = &d.shared.tags.search;
                let mut last = (0usize, 0usize);
                for m in q.re.find_iter(&text).take(5000) {
                    if m.start() == m.end() {
                        continue;
                    }
                    let s = crate::search::char_offset(&text, last, m.start());
                    let e = s + text[m.start()..m.end()].chars().count();
                    last = (m.end(), e);
                    buf.apply_tag(tag, &buf.iter_at_offset(s as i32), &buf.iter_at_offset(e as i32));
                }
            }
            *self.preview_file.borrow_mut() = Some(path.clone());
        }
        let rel = path.strip_prefix(&self.root).unwrap_or(&path).display().to_string();
        self.preview_title.set_text(&format!("{rel}:{}", line + 1));
        self.preview_title.set_tooltip_text(Some(&rel));
        self.preview_open.set_sensitive(true);
        let buf = &d.buffer;
        let mut it = buf.iter_at_line(line as i32).unwrap_or_else(|| buf.end_iter());
        it.set_line_offset(col as i32);
        buf.place_cursor(&it);
        let view = d.view.clone();
        glib::idle_add_local_once(move || {
            view.scroll_to_mark(&view.buffer().get_insert(), 0.0, true, 0.0, 0.35);
        });
    }

    /// Opens result `pos` in a tab of the editor (the window stays open).
    fn open(&self, pos: u32) {
        let (Some(ed), Some((path, line, col))) = (self.ed.upgrade(), self.result(pos)) else { return };
        ed.open_at(&path, line, col);
        ed.message(&format!("Aperto {}:{}", path.display(), line + 1));
    }
}
