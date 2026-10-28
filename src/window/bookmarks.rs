//! Bookmarks: favourite files (and folders) opened from the "Preferiti"
//! menu or from a small window with a filter (Ctrl+Shift+B), wherever
//! they are, without looking for them.

use super::Editor;
use crate::config;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// "name  —  ~/folder" for menus and lists.
fn short_dir(p: &Path) -> String {
    let dir = p.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && dir.starts_with(&home) { format!("~{}", &dir[home.len()..]) } else { dir }
}

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl Editor {
    pub(super) fn setup_bookmark_actions(self: &Rc<Self>) {
        let a = gio::SimpleAction::new_stateful("bookmark", None, &false.to_variant());
        let ed = self.clone();
        a.connect_activate(move |_, _| ed.toggle_bookmark());
        self.win.add_action(&a);
        self.app.set_accels_for_action("win.bookmark", &["<Control><Alt>b"]);
        let a = gio::SimpleAction::new("bookmarks", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| ed.bookmarks_window());
        self.win.add_action(&a);
        self.app.set_accels_for_action("win.bookmarks", &["<Control><Shift>b"]);
        let a = gio::SimpleAction::new("open-bookmark", Some(glib::VariantTy::STRING));
        let ed = self.clone();
        a.connect_activate(move |_, v| {
            if let Some(p) = v.and_then(|v| v.get::<String>()) {
                ed.open_bookmark(Path::new(&p));
            }
        });
        self.win.add_action(&a);
        self.rebuild_bookmarks_menu();
    }

    /// The "Preferiti" menu: one entry per bookmark.
    pub(super) fn rebuild_bookmarks_menu(&self) {
        let m = &self.bookmarks_menu;
        m.remove_all();
        let head = gio::Menu::new();
        head.append(Some("File corrente nei preferiti"), Some("win.bookmark"));
        head.append(Some("Apri un preferito…"), Some("win.bookmarks"));
        m.append_section(None, &head);
        let files = gio::Menu::new();
        for p in self.bookmarks.borrow().iter() {
            // '_' would be taken as a mnemonic marker.
            let label = format!("{}  —  {}", name_of(p), short_dir(p)).replace('_', "__");
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(Some("win.open-bookmark"), Some(&p.to_string_lossy().to_variant()));
            files.append_item(&item);
        }
        if self.bookmarks.borrow().is_empty() {
            files.append(Some("(nessun preferito)"), Some("win.none"));
        }
        m.append_section(None, &files);
    }

    /// Check mark of "File corrente nei preferiti" for the active tab.
    pub(super) fn update_bookmark_state(&self) {
        let Some(a) = self.win.lookup_action("bookmark").and_downcast::<gio::SimpleAction>() else { return };
        let path = self.active_doc().and_then(|d| d.path.borrow().clone());
        a.set_enabled(path.is_some());
        let on = path.is_some_and(|p| self.bookmarks.borrow().contains(&p));
        a.set_state(&on.to_variant());
    }

    fn set_bookmarks(&self, list: Vec<PathBuf>) {
        config::save_bookmarks(&list);
        *self.bookmarks.borrow_mut() = list;
        self.rebuild_bookmarks_menu();
        self.update_bookmark_state();
    }

    /// Add or remove the file of the active tab.
    fn toggle_bookmark(&self) {
        let Some(path) = self.active_doc().and_then(|d| d.path.borrow().clone()) else {
            self.message("Salva il file per poterlo aggiungere ai preferiti");
            return;
        };
        let mut list = self.bookmarks.borrow().clone();
        if let Some(i) = list.iter().position(|p| *p == path) {
            list.remove(i);
            self.message(&format!("{} tolto dai preferiti", name_of(&path)));
        } else {
            list.push(path.clone());
            self.message(&format!("{} aggiunto ai preferiti", name_of(&path)));
        }
        self.set_bookmarks(list);
    }

    /// From the file panel: add a file or a folder.
    pub(super) fn add_bookmark(&self, path: &Path) {
        if self.bookmarks.borrow().iter().any(|p| p == path) {
            self.message(&format!("{} è già nei preferiti", name_of(path)));
            return;
        }
        let mut list = self.bookmarks.borrow().clone();
        list.push(path.to_path_buf());
        self.set_bookmarks(list);
        self.message(&format!("{} aggiunto ai preferiti", name_of(path)));
    }

    /// A file or folder was renamed or moved in the file panel.
    pub(super) fn bookmarks_renamed(&self, old: &Path, new: &Path) {
        let list: Vec<PathBuf> = self
            .bookmarks
            .borrow()
            .iter()
            .map(|p| p.strip_prefix(old).map_or_else(|_| p.clone(), |rest| new.join(rest)))
            .collect();
        if list != *self.bookmarks.borrow() {
            self.set_bookmarks(list);
        }
    }

    /// Open a bookmark (a folder becomes the working folder). False if it
    /// no longer exists.
    fn open_bookmark(self: &Rc<Self>, path: &Path) -> bool {
        if !path.exists() {
            self.message(&format!(
                "{} non esiste più ({}): toglilo da Preferiti → Apri un preferito…",
                name_of(path),
                short_dir(path)
            ));
            return false;
        }
        self.open_path(path);
        true
    }

    /// A window listing the bookmarks with a filter: Enter opens, the
    /// button on each row removes it.
    fn bookmarks_window(self: &Rc<Self>) {
        if self.bookmarks.borrow().is_empty() {
            self.message("Nessun preferito: aggiungi il file corrente con Preferiti → File corrente nei preferiti (Ctrl+Alt+B)");
            return;
        }
        let win = gtk::Window::builder()
            .title("Preferiti")
            .transient_for(&self.win)
            .modal(true)
            .default_width(620)
            .default_height(420)
            .build();
        let entry = gtk::SearchEntry::builder().placeholder_text("Filtra i preferiti").hexpand(true).build();
        let list = gtk::ListBox::new();
        list.set_activate_on_single_click(true);
        list.add_css_class("bookmarks-list");
        let scroll = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
        scroll.add_css_class("frame");
        let hint = gtk::Label::new(Some("Invio apre · Esc chiude · il pulsante a destra toglie dai preferiti"));
        hint.add_css_class("dim-label");
        hint.set_xalign(0.0);
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
        b.append(&hint);
        win.set_child(Some(&b));

        // Rows and the bookmark each one shows.
        let rows: Rc<RefCell<Vec<(gtk::ListBoxRow, PathBuf)>>> = Rc::default();
        for p in self.bookmarks.borrow().iter() {
            let name = gtk::Label::new(Some(&name_of(p)));
            name.set_xalign(0.0);
            name.add_css_class("heading");
            let missing = !p.exists();
            let where_ = if missing { format!("non trovato — {}", short_dir(p)) } else { short_dir(p) };
            let dir = gtk::Label::new(Some(&where_));
            dir.set_xalign(0.0);
            dir.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            dir.add_css_class(if missing { "error" } else { "dim-label" });
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_hexpand(true);
            text.append(&name);
            text.append(&dir);
            let remove = gtk::Button::from_icon_name("list-remove-symbolic");
            remove.add_css_class("flat");
            remove.set_valign(gtk::Align::Center);
            remove.set_tooltip_text(Some("Togli dai preferiti"));
            let rb = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            rb.set_margin_top(4);
            rb.set_margin_bottom(4);
            rb.set_margin_start(6);
            rb.set_margin_end(6);
            if p.is_dir() {
                rb.append(&gtk::Image::from_icon_name("folder-symbolic"));
            }
            rb.append(&text);
            rb.append(&remove);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&rb));
            list.append(&row);
            rows.borrow_mut().push((row.clone(), p.clone()));
            let (ed, rows2, list2, path) = (Rc::downgrade(self), rows.clone(), list.downgrade(), p.clone());
            remove.connect_clicked(move |_| {
                let Some(ed) = ed.upgrade() else { return };
                let mut l = ed.bookmarks.borrow().clone();
                l.retain(|x| *x != path);
                ed.set_bookmarks(l);
                ed.message(&format!("{} tolto dai preferiti", name_of(&path)));
                let mut rows = rows2.borrow_mut();
                if let Some(i) = rows.iter().position(|(_, x)| *x == path) {
                    let (row, _) = rows.remove(i);
                    if let Some(list) = list2.upgrade() {
                        list.remove(&row);
                    }
                }
            });
        }

        let path_of = {
            let rows = rows.clone();
            move |row: &gtk::ListBoxRow| rows.borrow().iter().find(|(r, _)| r == row).map(|(_, p)| p.clone())
        };
        // Filter by name or folder.
        {
            let (entry, path_of) = (entry.clone(), path_of.clone());
            list.set_filter_func(move |row| {
                let q = entry.text().to_lowercase();
                q.is_empty() || path_of(row).is_some_and(|p| p.to_string_lossy().to_lowercase().contains(&q))
            });
        }
        let first_visible = |list: &gtk::ListBox| {
            let mut i = 0;
            while let Some(r) = list.row_at_index(i) {
                if r.is_child_visible() {
                    return Some(r);
                }
                i += 1;
            }
            None
        };
        if let Some(r) = first_visible(&list) {
            list.select_row(Some(&r));
        }
        let l = list.clone();
        entry.connect_search_changed(move |_| {
            l.invalidate_filter();
            if let Some(r) = first_visible(&l) {
                l.select_row(Some(&r));
            }
        });
        let open = {
            let (ed, win, path_of) = (Rc::downgrade(self), win.downgrade(), path_of.clone());
            move |row: &gtk::ListBoxRow| {
                let (Some(ed), Some(p)) = (ed.upgrade(), path_of(row)) else { return };
                if ed.open_bookmark(&p)
                    && let Some(w) = win.upgrade()
                {
                    w.close();
                }
            }
        };
        let o = open.clone();
        list.connect_row_activated(move |_, row| o(row));
        let l = list.clone();
        entry.connect_activate(move |_| {
            if let Some(r) = l.selected_row().filter(|r| r.is_child_visible()).or_else(|| first_visible(&l)) {
                open(&r);
            }
        });
        // Up/Down from the filter move the selection.
        let keys = gtk::EventControllerKey::new();
        let l = list.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            let step = match key {
                gdk::Key::Down => 1,
                gdk::Key::Up => -1,
                _ => return glib::Propagation::Proceed,
            };
            let mut i = l.selected_row().map_or(-1, |r| r.index()) + step;
            while let Some(r) = l.row_at_index(i) {
                if r.is_child_visible() {
                    l.select_row(Some(&r));
                    break;
                }
                i += step;
            }
            glib::Propagation::Stop
        });
        entry.add_controller(keys);
        let w = win.downgrade();
        entry.connect_stop_search(move |_| {
            if let Some(w) = w.upgrade() {
                w.close();
            }
        });
        win.present();
        entry.grab_focus();
    }
}
