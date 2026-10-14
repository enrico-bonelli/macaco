//! Quick file search (Ctrl+P) in the working folder, available once the
//! folder has been indexed. The list of files is kept in memory while the
//! folder is open, saved on disk and scanned again in the background each
//! time the search window opens.

use super::Editor;
use crate::config;
use crate::finder::{self, Hit, Paths};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// Results shown at most.
const MAX_RESULTS: usize = 300;
/// Opening the search window scans the folder again only after this time.
const RESCAN_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// Files of the indexed working folder.
pub(super) struct Index {
    root: PathBuf,
    paths: Rc<Paths>,
    scanning: Cell<bool>,
    /// The cache has been read (or found missing).
    loaded: Cell<bool>,
    scanned_at: Cell<Option<std::time::Instant>>,
}

/// List searched, normalized query and all the matching indices.
type LastSearch = (Rc<Paths>, String, Vec<u32>);

/// The search window.
pub(super) struct QuickOpen {
    ed: Weak<Editor>,
    pub(super) win: gtk::Window,
    entry: gtk::SearchEntry,
    store: gtk::StringList,
    sel: gtk::SingleSelection,
    view: gtk::ListView,
    status: gtk::Label,
    hits: RefCell<Vec<Hit>>,
    /// Last search: a longer query only looks among its matches.
    last: RefCell<Option<LastSearch>>,
}

fn is_indexed(dir: &Path) -> bool {
    config::load_indexed().iter().any(|p| p == dir)
}

/// Pango markup of `text` with the characters at `positions` (counted from
/// `offset`) in bold.
fn highlight(text: &str, positions: &[usize], offset: usize) -> String {
    let mut out = String::new();
    let mut bold = false;
    for (i, c) in text.chars().enumerate() {
        let hit = positions.binary_search(&(i + offset)).is_ok();
        if hit != bold {
            out.push_str(if hit { "<b>" } else { "</b>" });
            bold = hit;
        }
        out.push_str(&glib::markup_escape_text(&c.to_string()));
    }
    if bold {
        out.push_str("</b>");
    }
    out
}

impl Editor {
    pub(super) fn setup_quickopen_actions(self: &Rc<Self>) {
        let a = gio::SimpleAction::new("quick-open", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| ed.quick_open());
        self.win.add_action(&a);
        self.app.set_accels_for_action("win.quick-open", &["<Control>p"]);

        let a = gio::SimpleAction::new_stateful("index-folder", None, &false.to_variant());
        let ed = self.clone();
        a.connect_activate(move |a, _| {
            let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            ed.set_indexed(on);
        });
        self.win.add_action(&a);
    }

    /// Follows the working folder: loads its index if it is indexed, drops
    /// the index of the previous folder.
    pub(super) fn sync_index(self: &Rc<Self>) {
        let workdir = self.workdir.borrow().clone();
        let indexed = workdir.as_deref().is_some_and(is_indexed);
        if let Some(a) = self.win.lookup_action("index-folder").and_downcast::<gio::SimpleAction>() {
            a.set_enabled(workdir.is_some());
            a.set_state(&indexed.to_variant());
        }
        let Some(root) = workdir.filter(|_| indexed) else {
            *self.index.borrow_mut() = None;
            return;
        };
        if self.index.borrow().as_ref().is_some_and(|i| i.root == root) {
            return;
        }
        *self.index.borrow_mut() = Some(Index {
            root: root.clone(),
            paths: Rc::new(Paths::default()),
            scanning: Cell::new(false),
            loaded: Cell::new(false),
            scanned_at: Cell::new(None),
        });
        // The saved list is shown at once, then updated by a new scan.
        let ed = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let r = root.clone();
            let cached = gio::spawn_blocking(move || finder::load_cache(&r)).await.ok().flatten();
            let Some(ed) = ed.upgrade() else { return };
            if let Some(i) = ed.index.borrow_mut().as_mut().filter(|i| i.root == root) {
                if let Some(paths) = cached {
                    i.paths = Rc::new(paths);
                }
                i.loaded.set(true);
            }
            ed.refresh_quick();
            ed.rescan_index();
        });
    }

    /// Scans the indexed folder again in the background and saves the list.
    fn rescan_index(self: &Rc<Self>) {
        let root = {
            let index = self.index.borrow();
            let Some(i) = index.as_ref().filter(|i| !i.scanning.get()) else { return };
            i.scanning.set(true);
            i.root.clone()
        };
        self.refresh_quick();
        let ed = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let r = root.clone();
            let paths = gio::spawn_blocking(move || {
                let paths = finder::scan(&r);
                finder::save_cache(&r, &paths);
                paths
            })
            .await
            .unwrap_or_default();
            let Some(ed) = ed.upgrade() else { return };
            if let Some(i) = ed.index.borrow_mut().as_mut().filter(|i| i.root == root) {
                i.paths = Rc::new(paths);
                i.scanning.set(false);
                i.loaded.set(true);
                i.scanned_at.set(Some(std::time::Instant::now()));
            }
            ed.refresh_quick();
            // The scan allocated a lot in its thread.
            ed.release_memory();
        });
    }

    fn set_indexed(self: &Rc<Self>, on: bool) {
        let Some(root) = self.workdir.borrow().clone() else { return };
        let mut list = config::load_indexed();
        list.retain(|p| *p != root);
        if on {
            list.push(root.clone());
            self.message(&format!("Indicizzazione di {} (Ctrl+P per cercare i file)", root.display()));
        } else {
            finder::delete_cache(&root);
            self.message("Indicizzazione disattivata per la cartella di lavoro");
            let quick = self.quick.borrow().clone();
            if let Some(q) = quick {
                q.win.close();
            }
        }
        config::save_indexed(&list);
        self.sync_index();
        if !on {
            self.release_memory();
        }
    }

    fn quick_open(self: &Rc<Self>) {
        let Some(root) = self.workdir.borrow().clone() else {
            self.message("La ricerca dei file funziona nella cartella di lavoro (File → Apri cartella di lavoro…)");
            return;
        };
        if self.index.borrow().is_none() {
            let dialog = gtk::AlertDialog::builder()
                .modal(true)
                .message("Cartella non indicizzata")
                .detail(format!(
                    "Per cercare i file in «{}» la cartella va indicizzata: l'elenco dei file viene salvato e \
                     aggiornato in background. Le cartelle nascoste e i file ignorati da git sono esclusi.",
                    root.display()
                ))
                .buttons(["Annulla", "Indicizza ora"])
                .cancel_button(0)
                .default_button(1)
                .build();
            let ed = self.clone();
            dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| {
                if res == Ok(1) {
                    ed.set_indexed(true);
                    ed.quick_open();
                }
            });
            return;
        }
        let quick = self.quick.borrow().clone();
        if let Some(q) = quick {
            q.win.present();
            return;
        }
        let q = QuickOpen::new(self, &root);
        *self.quick.borrow_mut() = Some(q.clone());
        q.update();
        q.win.present();
        q.entry.grab_focus();
        // Pick up files created or deleted since the last scan.
        let stale = self.index.borrow().as_ref().is_some_and(|i| {
            i.loaded.get() && i.scanned_at.get().is_none_or(|t| t.elapsed() >= RESCAN_AFTER)
        });
        if stale {
            self.rescan_index();
        }
    }

    /// Updates the open search window after the index changed.
    fn refresh_quick(&self) {
        let quick = self.quick.borrow().clone();
        if let Some(q) = quick {
            q.update();
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_quick_open(&self, n: u32) {
        let quick = self.quick.borrow().clone();
        if let Some(q) = quick {
            q.open(n);
        }
    }
}

impl QuickOpen {
    fn new(ed: &Rc<Editor>, root: &Path) -> Rc<QuickOpen> {
        let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
        let win = gtk::Window::builder()
            .title(format!("Cerca file — {name}"))
            .transient_for(&ed.win)
            .modal(true)
            .default_width(640)
            .default_height(460)
            .build();
        let entry = gtk::SearchEntry::builder().placeholder_text("Nome del file…").search_delay(50).build();
        let store = gtk::StringList::new(&[]);
        let sel = gtk::SingleSelection::new(Some(store.clone()));
        sel.set_autoselect(true);
        let view = gtk::ListView::new(Some(sel.clone()), None::<gtk::ListItemFactory>);
        view.set_single_click_activate(true);
        view.add_css_class("quick-open");
        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroll.add_css_class("frame");
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        let b = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        b.append(&entry);
        b.append(&scroll);
        b.append(&status);
        win.set_child(Some(&b));

        let q = Rc::new(QuickOpen {
            ed: Rc::downgrade(ed),
            win,
            entry,
            store,
            sel,
            view,
            status,
            hits: RefCell::new(Vec::new()),
            last: RefCell::new(None),
        });
        q.view.set_factory(Some(&q.factory()));

        let w = Rc::downgrade(&q);
        q.entry.connect_search_changed(move |_| {
            if let Some(q) = w.upgrade() {
                q.update();
            }
        });
        let w = Rc::downgrade(&q);
        q.entry.connect_activate(move |_| {
            if let Some(q) = w.upgrade() {
                q.open(q.sel.selected());
            }
        });
        let w = q.win.downgrade();
        q.entry.connect_stop_search(move |_| {
            if let Some(w) = w.upgrade() {
                w.close();
            }
        });
        // Up/Down move in the results while typing.
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&q);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(q) = w.upgrade() else { return glib::Propagation::Proceed };
            let n = q.store.n_items();
            if n == 0 {
                return glib::Propagation::Proceed;
            }
            let cur = q.sel.selected().min(n - 1);
            let next = match key {
                gdk::Key::Down | gdk::Key::KP_Down => (cur + 1).min(n - 1),
                gdk::Key::Up | gdk::Key::KP_Up => cur.saturating_sub(1),
                gdk::Key::Page_Down => (cur + 10).min(n - 1),
                gdk::Key::Page_Up => cur.saturating_sub(10),
                _ => return glib::Propagation::Proceed,
            };
            q.sel.set_selected(next);
            q.view.scroll_to(next, gtk::ListScrollFlags::NONE, None);
            glib::Propagation::Stop
        });
        q.entry.add_controller(keys);
        let w = Rc::downgrade(&q);
        q.view.connect_activate(move |_, pos| {
            if let Some(q) = w.upgrade() {
                q.open(pos);
            }
        });
        let w = Rc::downgrade(&q);
        q.win.connect_close_request(move |_| {
            if let Some(ed) = w.upgrade().and_then(|q| q.ed.upgrade()) {
                *ed.quick.borrow_mut() = None;
                ed.release_memory();
            }
            glib::Propagation::Proceed
        });
        q
    }

    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let f = gtk::SignalListItemFactory::new();
        f.connect_setup(|_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let name = gtk::Label::new(None);
            name.set_xalign(0.0);
            let dir = gtk::Label::new(None);
            dir.set_xalign(0.0);
            dir.set_hexpand(true);
            dir.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            dir.add_css_class("dim-label");
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.set_margin_start(6);
            row.set_margin_end(6);
            row.set_margin_top(3);
            row.set_margin_bottom(3);
            row.append(&name);
            row.append(&dir);
            li.set_child(Some(&row));
        });
        let w = Rc::downgrade(self);
        f.connect_bind(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let (Some(q), Some(row)) = (w.upgrade(), li.child()) else { return };
            let Some(rel) = li.item().and_downcast::<gtk::StringObject>().map(|s| s.string()) else { return };
            let hits = q.hits.borrow();
            let positions = hits.get(li.position() as usize).map(|h| h.positions.as_slice()).unwrap_or(&[]);
            let split = rel.rfind('/').map_or(0, |i| i + 1);
            let name_start = rel[..split].chars().count();
            if let Some(l) = row.first_child().and_downcast::<gtk::Label>() {
                l.set_markup(&highlight(&rel[split..], positions, name_start));
            }
            if let Some(l) = row.last_child().and_downcast::<gtk::Label>() {
                l.set_markup(&highlight(rel[..split].trim_end_matches('/'), positions, 0));
            }
        });
        f
    }

    /// Searches the index for the text typed.
    fn update(&self) {
        let Some(ed) = self.ed.upgrade() else { return };
        let (paths, scanning, loaded) = match ed.index.borrow().as_ref() {
            Some(i) => (i.paths.clone(), i.scanning.get(), i.loaded.get()),
            None => (Rc::new(Paths::default()), false, true),
        };
        let query = self.entry.text();
        let norm = finder::normalize(&query);
        let found = {
            let last = self.last.borrow();
            let within = last
                .as_ref()
                .filter(|(p, q, _)| Rc::ptr_eq(p, &paths) && !q.is_empty() && norm.starts_with(q.as_str()))
                .map(|(_, _, all)| all.as_slice());
            finder::search(&paths, &query, MAX_RESULTS, within)
        };
        let hits = found.hits;
        if !norm.is_empty() {
            *self.last.borrow_mut() = Some((paths.clone(), norm, found.all));
        }
        let rels: Vec<&str> = hits.iter().map(|h| paths.get(h.index)).collect();
        // Hits first: binding the new rows reads them.
        *self.hits.borrow_mut() = hits;
        self.store.splice(0, self.store.n_items(), &rels);
        if !rels.is_empty() {
            self.sel.set_selected(0);
            self.view.scroll_to(0, gtk::ListScrollFlags::NONE, None);
        }
        let mut text = if paths.is_empty() && (scanning || !loaded) {
            "Indicizzazione in corso…".to_string()
        } else if query.trim().is_empty() {
            format!("{} file indicizzati", paths.len())
        } else {
            format!("{} risultati su {} file", rels.len(), paths.len())
        };
        if scanning && !paths.is_empty() {
            text.push_str(" · aggiornamento in corso…");
        }
        self.status.set_text(&text);
    }

    fn open(&self, pos: u32) {
        let (Some(ed), Some(rel)) = (self.ed.upgrade(), self.store.string(pos)) else { return };
        let Some(root) = ed.index.borrow().as_ref().map(|i| i.root.clone()) else { return };
        self.win.close();
        ed.open_path(&root.join(rel.as_str()));
    }
}
