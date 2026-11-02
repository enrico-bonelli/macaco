//! Working folder and file panel.

use super::*;

impl Editor {
    pub(super) fn sidebar_refresh(&self) {
        if let Some(s) = self.sidebar.borrow().clone() {
            s.refresh();
        }
    }

    pub(super) fn sidebar_visible(&self) -> bool {
        self.sidebar.borrow().as_ref().is_some_and(|s| s.widget.get_visible())
    }

    pub(super) fn set_sidebar(self: &Rc<Self>, on: bool) {
        if on && self.sidebar.borrow().is_none() {
            // Handlers holding a weak reference to the editor.
            macro_rules! on_editor {
                (|$ed:ident $(, $arg:ident)*| $body:expr) => {{
                    let w = Rc::downgrade(self);
                    Box::new(move |$($arg),*| {
                        if let Some($ed) = w.upgrade() {
                            $body;
                        }
                    })
                }};
            }
            let s = Sidebar::new(sidebar::Handlers {
                open: on_editor!(|ed, p| ed.open_path(p)),
                terminal: on_editor!(|ed, p| ed.terminal_at(p.to_path_buf())),
                set_workdir: on_editor!(|ed, p| match p {
                    Some(p) => ed.set_workdir(Some(p)),
                    None => ed.close_workdir(),
                }),
                renamed: on_editor!(|ed, old, new| ed.renamed(old, new)),
                error: on_editor!(|ed, title, detail| ed.error_dialog(title, detail)),
                git: on_editor!(|ed, action, p| ed.git_path_action(action, p)),
                bookmark: on_editor!(|ed, p| ed.add_bookmark(p)),
                history: on_editor!(|ed, p| ed.file_history(p)),
            });
            s.set_git(self.git_status());
            self.hpaned.set_start_child(Some(&s.widget));
            self.hpaned.set_position(self.shared.settings.borrow().sidebar_width);
            *self.sidebar.borrow_mut() = Some(s);
        }
        let Some(s) = self.sidebar.borrow().clone() else { return };
        if !on && s.widget.get_visible() {
            self.shared.settings.borrow_mut().sidebar_width = self.hpaned.position();
        }
        s.widget.set_visible(on);
        if on {
            self.sync_sidebar();
        } else if s.widget.focus_child().is_some()
            && let Some(d) = self.active_doc()
        {
            d.view.grab_focus();
        }
        self.shared.settings.borrow_mut().sidebar = on;
        self.set_action_state("sidebar", on);
    }

    /// Shows the working folder, or the folder of the active document, and
    /// selects the active document.
    pub(super) fn sync_sidebar(self: &Rc<Self>) {
        if !self.sidebar_visible() {
            return;
        }
        let Some(s) = self.sidebar.borrow().clone() else { return };
        let file = self.active_doc().and_then(|d| d.path.borrow().clone());
        let workdir = self.workdir.borrow().clone();
        match (&workdir, &file) {
            (Some(w), _) => s.set_root(w, true),
            // Keep the folder shown (the user may have gone up) while it
            // contains the document.
            (None, Some(f)) => {
                let root = s.root();
                if (root.as_os_str().is_empty() || !f.starts_with(&root))
                    && let Some(dir) = f.parent()
                {
                    s.set_root(dir, false);
                }
            }
            (None, None) => {
                if s.root().as_os_str().is_empty() {
                    s.set_root(&self.doc_dir(), false);
                }
            }
        }
        if let Some(f) = file {
            s.reveal(&f);
        }
    }

    pub(super) fn set_workdir(self: &Rc<Self>, dir: Option<PathBuf>) {
        let dir = dir.map(|d| std::fs::canonicalize(&d).unwrap_or(d));
        let old = self.workdir.replace(dir.clone());
        match &dir {
            Some(d) => {
                self.message(&format!("Cartella di lavoro: {}", d.display()));
                self.set_sidebar(true);
            }
            None => {
                self.message("Cartella di lavoro chiusa");
                // Release the listing of the folder (and its monitors); the
                // panel goes back to the folder of the active document.
                if let Some(s) = self.sidebar.borrow().clone() {
                    s.clear();
                }
                // Close its git changes windows (they remove themselves
                // from the list when closed).
                if let Some(old) = old {
                    let inside: Vec<_> = self.changes.borrow().iter().filter(|c| c.folder().starts_with(&old)).cloned().collect();
                    for c in inside {
                        c.win.close();
                    }
                }
                self.release_memory();
            }
        }
        self.sync_index();
        self.sync_lsp();
        self.sync_sidebar();
        self.git_refresh();
        self.schedule_autosave();
    }

    pub(super) fn open_folder_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title("Apri cartella di lavoro").modal(true).build();
        if let Some(w) = self.workdir.borrow().as_ref() {
            dialog.set_initial_folder(Some(&gio::File::for_path(w)));
        } else {
            dialog.set_initial_folder(Some(&gio::File::for_path(self.doc_dir())));
        }
        let ed = self.clone();
        dialog.select_folder(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            if let Ok(p) = res.map(|f| f.path()) {
                ed.set_workdir(p);
            }
        });
    }

    /// Updates open documents after a file or folder was renamed.
    pub(super) fn renamed(self: &Rc<Self>, old: &Path, new: &Path) {
        self.bookmarks_renamed(old, new);
        for d in self.docs.borrow().iter() {
            let moved = d.path.borrow().as_ref().and_then(|p| p.strip_prefix(old).ok().map(|rest| new.join(rest)));
            if let Some(p) = moved {
                let p = if p.as_os_str().is_empty() { new.to_path_buf() } else { p };
                *d.path.borrow_mut() = Some(p);
                d.refresh_title();
                if self.is_active(d) {
                    self.update_status_for(d);
                }
            }
        }
        if self.workdir.borrow().as_ref().is_some_and(|w| w.starts_with(old)) {
            let w = self.workdir.borrow().as_ref().and_then(|w| w.strip_prefix(old).ok().map(|r| new.join(r)));
            *self.workdir.borrow_mut() = w;
        }
        self.schedule_layout();
        self.git_refresh();
    }

    /// "Close working folder": closes every document (asking to save the
    /// modified ones; cancelling stops here), then the folder, its index
    /// and the side panel.
    pub(super) fn close_workdir(self: &Rc<Self>) {
        if self.workdir.borrow().is_none() {
            self.message("Nessuna cartella di lavoro aperta");
            return;
        }
        let mut docs: Vec<Rc<Doc>> = self.docs.borrow().clone();
        // Documents are taken from the end: the modified ones first, so
        // that cancelling leaves everything open.
        docs.sort_by_key(|d| d.is_unsaved());
        self.close_docs_then_workdir(docs);
    }

    pub(super) fn close_docs_then_workdir(self: &Rc<Self>, mut docs: Vec<Rc<Doc>>) {
        // Documents still open (one may have been closed meanwhile).
        let Some(d) = docs.pop() else {
            self.set_sidebar(false);
            self.set_workdir(None);
            return;
        };
        if !self.docs.borrow().iter().any(|x| Rc::ptr_eq(x, &d)) {
            self.close_docs_then_workdir(docs);
            return;
        }
        self.close_doc_then(&d, move |ed, closed| {
            if closed {
                ed.close_docs_then_workdir(docs);
            } else {
                ed.message("Chiusura della cartella di lavoro annullata");
            }
        });
    }
}
