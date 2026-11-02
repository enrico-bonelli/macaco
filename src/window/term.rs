//! The terminal panel.

use super::*;

/// Shortcuts that keep working while the terminal has the focus; all the
/// others are passed to the shell (Ctrl+C, Ctrl+R, Ctrl+W, F-keys, …).
pub(super) const TERMINAL_ACCELS: &[&str] = &[
    "win.terminal",
    "win.terminal-here",
    "win.next-tab",
    "win.prev-tab",
    "win.zoom-in",
    "win.zoom-out",
    "win.zoom-reset",
];

impl Editor {
    /// Folder of the active document, or the working folder (then the home
    /// folder) if it was never saved.
    pub(super) fn doc_dir(&self) -> PathBuf {
        self.active_doc()
            .and_then(|d| d.path.borrow().as_deref().and_then(Path::parent).map(Path::to_path_buf))
            .filter(|p| p.is_dir())
            .or_else(|| self.workdir.borrow().clone())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    pub(super) fn terminal_visible(&self) -> bool {
        self.terminal.borrow().as_ref().is_some_and(|t| t.widget.get_visible())
    }

    pub(super) fn set_terminal_state(&self, on: bool) {
        self.set_action_state("terminal", on);
    }

    /// Shows or hides the terminal panel. The first time, the shell is
    /// started in the folder of the active document; hiding the panel keeps
    /// the shell running.
    pub(super) fn set_terminal(self: &Rc<Self>, on: bool) {
        self.show_terminal(on, &self.doc_dir());
    }

    /// Like `set_terminal`; a new shell is started in `dir`.
    pub(super) fn show_terminal(self: &Rc<Self>, on: bool, dir: &Path) {
        let created = on && self.terminal.borrow().is_none();
        if created {
            let weak = Rc::downgrade(self);
            let on_exit = move || {
                if let Some(ed) = weak.upgrade() {
                    // Leave the signal handler before destroying the widget.
                    glib::idle_add_local_once(move || ed.drop_terminal());
                }
            };
            let weak = Rc::downgrade(self);
            let on_error = move |e: String| {
                if let Some(ed) = weak.upgrade() {
                    ed.error_dialog("Impossibile avviare la shell", &e);
                    glib::idle_add_local_once(move || ed.drop_terminal());
                }
            };
            let t = match Terminal::new(dir, on_exit, on_error) {
                Ok(t) => t,
                Err(e) => {
                    self.set_terminal_state(false);
                    self.error_dialog(
                        "Terminale non disponibile",
                        &format!("Serve la libreria VTE per GTK 4 (pacchetto «vte4» su Arch, «libvte-2.91-gtk4-0» su Debian/Ubuntu).\n\n{e}"),
                    );
                    return;
                }
            };
            let s = self.shared.settings.borrow();
            t.apply_style(&self.shared.font.borrow(), s.dark, s.bell);
            drop(s);
            let weak = Rc::downgrade(self);
            t.connect_focus(move |focus| {
                if let Some(ed) = weak.upgrade() {
                    ed.suspend_accels(focus);
                }
            });
            t.widget.set_visible(false);
            self.vpaned.set_end_child(Some(&t.widget));
            *self.terminal.borrow_mut() = Some(t);
        }
        let Some(widget) = self.terminal.borrow().as_ref().map(|t| t.widget.clone()) else { return };
        if on {
            widget.set_visible(true);
            if created || self.vpaned.height() - self.vpaned.position() < 60 {
                let h = self.vpaned.height();
                self.vpaned.set_position(h - (h / 3).clamp(120, 400));
            }
            if let Some(t) = self.terminal.borrow().as_ref() {
                t.grab_focus();
            }
        } else {
            widget.set_visible(false);
            self.suspend_accels(false);
            if let Some(d) = self.active_doc() {
                d.view.grab_focus();
            }
        }
        self.set_terminal_state(on);
    }

    /// Opens the terminal and moves the shell to `dir`.
    /// Runs `cmd` in the terminal, in `dir` (the terminal is opened if
    /// needed; its shell may take a moment to start).
    pub(super) fn run_in_terminal(self: &Rc<Self>, dir: &Path, cmd: &'static str) {
        self.terminal_at(dir.to_path_buf());
        let ed = Rc::downgrade(self);
        let tries = Cell::new(0);
        glib::timeout_add_local(Duration::from_millis(150), move || {
            let Some(ed) = ed.upgrade() else { return glib::ControlFlow::Break };
            tries.set(tries.get() + 1);
            let done = ed.terminal.borrow().as_ref().is_some_and(|t| t.run(cmd).is_ok());
            if done || tries.get() > 30 {
                if !done {
                    ed.message(&format!("Il terminale è occupato: esegui «{cmd}»"));
                }
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    pub(super) fn terminal_at(self: &Rc<Self>, dir: PathBuf) {
        let res = self.terminal.borrow().as_ref().map(|t| t.cd(&dir));
        if let Some(Err(e)) = res {
            self.message(&e);
        }
        self.show_terminal(true, &dir);
    }

    /// Removes the terminal after its shell has exited.
    pub(super) fn drop_terminal(self: &Rc<Self>) {
        let Some(t) = self.terminal.borrow_mut().take() else { return };
        let had_focus = t.has_focus();
        self.vpaned.set_end_child(None::<&gtk::Widget>);
        self.suspend_accels(false);
        self.set_terminal_state(false);
        self.release_memory();
        if had_focus && let Some(d) = self.active_doc() {
            d.view.grab_focus();
        }
    }

    /// Window shortcuts are handled before the focused widget sees the key:
    /// while the terminal has the focus they are removed (except a few), so
    /// that keys like Ctrl+C or Ctrl+R reach the shell.
    pub(super) fn suspend_accels(&self, suspend: bool) {
        let mut saved = self.term_accels.borrow_mut();
        if suspend {
            if !saved.is_empty() {
                return;
            }
            for name in self.app.list_action_descriptions() {
                if TERMINAL_ACCELS.contains(&name.as_str()) {
                    continue;
                }
                let accels = self.app.accels_for_action(&name);
                if !accels.is_empty() {
                    self.app.set_accels_for_action(&name, &[]);
                    saved.push((name.to_string(), accels));
                }
            }
        } else {
            for (name, accels) in saved.drain(..) {
                let accels: Vec<&str> = accels.iter().map(|a| a.as_str()).collect();
                self.app.set_accels_for_action(&name, &accels);
            }
        }
    }
}
