//! Session, autosave and the cache of unsaved documents.

use super::*;

/// Delay between an edit and writing unsaved content to the cache.
pub(super) const AUTOSAVE_DELAY: Duration = Duration::from_millis(1500);

/// Serializes background autosaves and the final synchronous `persist` at
/// window shutdown so they never write or clean `~/.macaco/cache` at the
/// same time.
static PERSIST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Editor {
    pub(super) fn schedule_autosave(self: &Rc<Self>) {
        if self.restoring.get() || self.shut_down.get() {
            return;
        }
        if self.autosave_writing.get() {
            self.autosave_again.set(true);
            return;
        }
        if self.autosave_pending.replace(true) {
            return;
        }
        let ed = Rc::downgrade(self);
        glib::timeout_add_local_once(AUTOSAVE_DELAY, move || {
            if let Some(ed) = ed.upgrade() {
                ed.autosave_pending.set(false);
                ed.persist_async();
            }
        });
    }

    fn collect_session(&self) -> (Session, Vec<String>) {
        let mut session = Session {
            split: self.notebooks[1].is_visible(),
            workdir: self.workdir.borrow().clone(),
            ..Default::default()
        };
        let mut keep: Vec<String> = Vec::new();
        for (pane, nb) in self.notebooks.iter().enumerate() {
            session.active[pane] = nb.current_page().unwrap_or(0) as usize;
            for i in 0..nb.n_pages() {
                let Some(d) = nb.nth_page(Some(i)).and_then(|w| self.doc_for_widget(&w)) else { continue };
                if d.is_unsaved() {
                    let id = d.id.borrow().clone();
                    keep.push(id.clone());
                    session.tabs.push((pane, TabRef::Cache(id)));
                } else if let Some(p) = d.path.borrow().clone() {
                    session.tabs.push((pane, TabRef::File(p)));
                }
            }
        }
        keep.sort_unstable();
        (session, keep)
    }

    /// Periodic autosave: snapshots changed buffers on the main loop and
    /// writes all cache and session files on a worker thread.
    fn persist_async(self: &Rc<Self>) {
        if self.shut_down.get() {
            return;
        }
        if self.autosave_writing.replace(true) {
            self.autosave_again.set(true);
            return;
        }
        let (session, keep) = self.collect_session();
        let session_text = session.to_text();
        let mut doc_refs: Vec<(Weak<Doc>, String, u64)> = Vec::new();
        let mut io_jobs: Vec<(String, String, config::CacheMeta)> = Vec::new();
        for nb in &self.notebooks {
            for i in 0..nb.n_pages() {
                let Some(d) = nb.nth_page(Some(i)).and_then(|w| self.doc_for_widget(&w)) else { continue };
                if d.is_unsaved() && d.cache_is_stale() {
                    let id = d.id.borrow().clone();
                    doc_refs.push((d.weak(), id.clone(), d.edit_gen()));
                    io_jobs.push((id, d.text(), d.cache_meta()));
                }
            }
        }
        let (write_session, clean_unused) = {
            let last = self.persisted.borrow();
            (session_text != last.0, keep != last.1)
        };
        if io_jobs.is_empty() && !write_session && !clean_unused {
            self.autosave_writing.set(false);
            if self.autosave_again.replace(false) {
                self.schedule_autosave();
            }
            return;
        }
        let ed = Rc::downgrade(self);
        let session_written = session_text.clone();
        let keep_written = keep.clone();
        glib::spawn_future_local(async move {
            let results = gio::spawn_blocking(move || {
                let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let res: Vec<Result<(), String>> = io_jobs
                    .into_iter()
                    .map(|(id, text, meta)| config::write_cache(&id, &text, &meta).map_err(|e| e.to_string()))
                    .collect();
                if write_session {
                    let _ = Session::write_text(&session_written);
                }
                if clean_unused {
                    for id in config::cached_ids() {
                        if keep_written.binary_search(&id).is_err() {
                            config::delete_cache(&id);
                        }
                    }
                }
                res
            })
            .await;
            let Some(ed) = ed.upgrade() else { return };
            ed.autosave_writing.set(false);
            if ed.shut_down.get() {
                return;
            }
            let mut orphan_written = false;
            if let Ok(results) = results {
                for ((wd, id, written_gen), res) in doc_refs.into_iter().zip(results) {
                    match res {
                        Ok(()) => {
                            if let Some(d) = wd.upgrade().filter(|d| d.is_unsaved() && *d.id.borrow() == id) {
                                d.set_cache_gen(written_gen);
                            } else {
                                orphan_written = true;
                            }
                        }
                        Err(e) => ed.message(&format!("Impossibile scrivere la cache: {e}")),
                    }
                }
                let mut last = ed.persisted.borrow_mut();
                if write_session {
                    last.0 = session_text;
                }
                if orphan_written {
                    last.1.clear();
                } else if clean_unused {
                    last.1 = keep;
                }
            }
            if ed.autosave_again.replace(false) || orphan_written {
                ed.schedule_autosave();
            }
        });
    }

    /// Write stale caches and the session file synchronously (at shutdown);
    /// remove unused cache files.
    pub(super) fn persist(&self) {
        if self.shut_down.get() {
            return;
        }
        let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for nb in &self.notebooks {
            for i in 0..nb.n_pages() {
                let Some(d) = nb.nth_page(Some(i)).and_then(|w| self.doc_for_widget(&w)) else { continue };
                if d.is_unsaved()
                    && d.cache_is_stale()
                    && let Err(e) = d.write_cache()
                {
                    self.message(&format!("Impossibile scrivere la cache: {e}"));
                }
            }
        }
        let (session, keep) = self.collect_session();
        let mut last = self.persisted.borrow_mut();
        session.save_if_changed(&mut last.0);
        // Caches of closed documents: looked for only when the set of
        // cached documents changed (closing a tab removes its own cache),
        // or when a background autosave was in flight.
        if keep != last.1 || self.autosave_writing.get() {
            for id in config::cached_ids() {
                if keep.binary_search(&id).is_err() {
                    config::delete_cache(&id);
                }
            }
            last.1 = keep;
        }
    }

    pub(super) fn restore_session(self: &Rc<Self>) {
        self.restoring.set(true);
        let session = Session::load();
        *self.workdir.borrow_mut() = session.workdir.clone().filter(|p| p.is_dir());
        let mut used: Vec<String> = Vec::new();
        for (pane, tab) in &session.tabs {
            match tab {
                TabRef::File(p) => {
                    if !p.exists() || self.docs.borrow().iter().any(|d| d.path.borrow().as_ref() == Some(p)) {
                        continue;
                    }
                    let d = self.new_doc();
                    d.set_unloaded_file(p.clone(), lang::detect(Some(p), ""));
                    self.append_restored(&d, *pane, session.split);
                }
                TabRef::Cache(id) => {
                    if self.restore_cached(id, *pane, session.split) {
                        used.push(id.clone());
                    }
                }
            }
        }
        // Unsaved documents not referenced by the session (e.g. after a
        // crash) are recovered as well.
        for id in config::cached_ids() {
            if !used.contains(&id) {
                self.restore_cached(&id, 0, false);
            }
        }
        if session.split && self.notebooks[1].n_pages() > 0 && self.notebooks[0].n_pages() > 0 {
            self.notebooks[1].set_visible(true);
            self.paned.set_position(self.shared.settings.borrow().width / 2);
        } else {
            self.collapse_split();
        }
        for (pane, nb) in self.notebooks.iter().enumerate() {
            if nb.n_pages() > 0 {
                let i = (session.active[pane] as u32).min(nb.n_pages() - 1);
                nb.set_current_page(Some(i));
                if let Some(d) = self.current_doc(pane)
                    && let Err(e) = d.ensure_loaded() {
                        self.message(&format!("Errore: {e}"));
                    }
            }
        }
        if self.docs.borrow().is_empty() {
            let d = self.new_doc();
            self.add_doc(&d, 0);
        }
        self.restoring.set(false);
        self.active_pane.set(0);
        self.update_status();
        self.schedule_layout();
    }

    /// The unsaved document `id`, from the cache (not loaded until shown).
    pub(super) fn restore_cached(self: &Rc<Self>, id: &str, pane: usize, split: bool) -> bool {
        let Some(meta) = config::read_cache_meta(id) else { return false };
        let d = self.new_doc_with_id(id.to_string());
        d.set_unloaded_from_cache(&meta);
        self.append_restored(&d, pane, split);
        true
    }

    pub(super) fn append_restored(&self, d: &Rc<Doc>, pane: usize, split: bool) {
        let nb = &self.notebooks[if split { pane } else { 0 }];
        nb.append_page(&d.scroller, Some(&d.tab));
    }

    /// Called when the window closes: unsaved content stays in the cache
    /// and is restored at the next start.
    pub(super) fn shutdown(&self) {
        if self.shut_down.get() {
            return;
        }
        self.persist();
        self.shut_down.set(true);
        // Nor any agent.
        self.shutdown_assistant();
        // No language server must outlive the editor.
        for c in self.lsp_clients.borrow_mut().drain(..) {
            c.kill();
        }
        // Removes the socket of the password questions.
        drop(self.askpass.borrow_mut().take());
        let mut s = self.shared.settings.borrow_mut();
        if self.sidebar_visible() {
            s.sidebar_width = self.hpaned.position();
        }
        if !self.win.is_maximized() {
            s.width = self.win.default_width();
            s.height = self.win.default_height();
        }
        self.settings_save_pending.set(false);
        s.save();
    }
}
