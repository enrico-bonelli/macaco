//! Git integration of the window: status bar, menu actions, commit and
//! branch dialogs. The repository is the one containing the working folder
//! or, without one, the active document.

use super::Editor;
use crate::doc;
use crate::git::{self, Mark, Status};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Actions that need a repository.
const REPO_ACTIONS: &[&str] = &[
    "git-add-file",
    "git-add-all",
    "git-unstage-file",
    "git-restore-file",
    "git-commit",
    "git-pull",
    "git-push",
    "git-fetch",
    "git-branches",
    "git-merge-abort",
    "git-changes",
    "git-history",
    "git-credentials",
    "git-resolve",
];

type AfterSave = Box<dyn FnOnce(&Rc<Editor>)>;

/// Last `n` lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Git asked for a password or a passphrase it could not read.
fn needs_credentials(err: &str) -> bool {
    [
        "terminal prompts disabled",
        "could not read Username",
        "could not read Password",
        "Permission denied (publickey",
        "Host key verification failed",
        "read_passphrase",
    ]
    .iter()
    .any(|p| err.contains(p))
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Folder in which to run a command about `path`.
fn dir_of(path: &Path) -> PathBuf {
    if path.is_dir() { path.to_path_buf() } else { path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf) }
}

fn dialog_window(parent: &impl IsA<gtk::Window>, title: &str) -> gtk::Window {
    let w = gtk::Window::builder().title(title).transient_for(parent).modal(true).default_width(520).build();
    let esc = gtk::EventControllerKey::new();
    let wk = w.downgrade();
    esc.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = wk.upgrade() {
                w.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    w.add_controller(esc);
    w
}

fn vbox() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .build()
}

impl Editor {
    pub(super) fn setup_git_actions(self: &Rc<Self>) {
        let add = |name: &str, accels: &[&str], f: fn(&Rc<Editor>)| {
            let a = gio::SimpleAction::new(name, None);
            let ed = self.clone();
            a.connect_activate(move |_, _| f(&ed));
            self.win.add_action(&a);
            if !accels.is_empty() {
                self.app.set_accels_for_action(&format!("win.{name}"), accels);
            }
        };
        add("git-refresh", &[], |ed| {
            ed.git_refresh();
            ed.sidebar_refresh();
        });
        add("git-init", &[], |ed| {
            let Some(dir) = ed.workdir.borrow().clone().or_else(|| ed.git_base()) else {
                ed.message("Apri una cartella di lavoro o salva il documento");
                return;
            };
            ed.git_op(&dir, os(&["init"]), "init", false);
        });
        add("git-add-file", &[], |ed| ed.with_active_file(|ed, p| ed.git_path_action("add", &p)));
        add("git-add-all", &[], |ed| {
            if let Some(root) = ed.git_root() {
                ed.save_all_then(move |ed| ed.git_op(&root, os(&["add", "-A"]), "add -A", false));
            }
        });
        add("git-unstage-file", &[], |ed| ed.with_active_file(|ed, p| ed.git_path_action("unstage", &p)));
        add("git-restore-file", &[], |ed| ed.with_active_file(|ed, p| ed.git_path_action("restore", &p)));
        add("git-commit", &["<Control><Alt>c"], |ed| ed.save_all_then(|ed| ed.commit_dialog()));
        add("git-pull", &[], |ed| ed.git_pull());
        add("git-push", &[], |ed| ed.git_push());
        add("git-fetch", &[], |ed| {
            if let Some(root) = ed.git_root() {
                ed.git_op(&root, os(&["fetch", "--all", "--prune"]), "fetch", false);
            }
        });
        add("git-branches", &[], |ed| ed.branch_dialog());
        add("git-changes", &["<Control><Alt>g"], |ed| {
            let Some(st) = ed.git_status() else { return };
            // The working folder, if it is inside the repository.
            let folder = ed.workdir.borrow().clone().filter(|w| w.starts_with(&st.root)).unwrap_or_else(|| st.root.clone());
            ed.changes_dialog(folder);
        });
        add("git-history", &["<Control><Alt>h"], |ed| ed.active_file_history());
        add("git-credentials", &[], |ed| ed.credentials_dialog());
        add("git-resolve", &[], |ed| ed.resolve_active_conflict());
        add("git-merge-abort", &[], |ed| {
            if let Some(root) = ed.git_root() {
                ed.git_op(&root, os(&["merge", "--abort"]), "merge --abort", true);
            }
        });
        self.enable_git_actions(false);
    }

    pub(super) fn build_git_menu() -> gio::Menu {
        let m = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Aggiungi il file corrente (add)"), Some("win.git-add-file"));
        s1.append(Some("Aggiungi tutte le modifiche (add -A)"), Some("win.git-add-all"));
        s1.append(Some("Rimuovi il file corrente dallo stage"), Some("win.git-unstage-file"));
        s1.append(Some("Annulla le modifiche al file corrente…"), Some("win.git-restore-file"));
        m.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Modifiche, commit e push della cartella di lavoro…"), Some("win.git-changes"));
        s2.append(Some("Commit…"), Some("win.git-commit"));
        s2.append(Some("Cronologia del file corrente…"), Some("win.git-history"));
        m.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Pull"), Some("win.git-pull"));
        s3.append(Some("Push"), Some("win.git-push"));
        s3.append(Some("Fetch"), Some("win.git-fetch"));
        s3.append(Some("Credenziali e chiavi SSH…"), Some("win.git-credentials"));
        m.append_section(None, &s3);
        let s4 = gio::Menu::new();
        s4.append(Some("Rami e merge…"), Some("win.git-branches"));
        s4.append(Some("Risolvi il conflitto del file corrente…"), Some("win.git-resolve"));
        s4.append(Some("Interrompi il merge"), Some("win.git-merge-abort"));
        m.append_section(None, &s4);
        let s5 = gio::Menu::new();
        s5.append(Some("Aggiorna lo stato"), Some("win.git-refresh"));
        s5.append(Some("Inizializza repository (init)"), Some("win.git-init"));
        m.append_section(None, &s5);
        m
    }

    fn enable_git_actions(&self, in_repo: bool) {
        for name in REPO_ACTIONS {
            if let Some(a) = self.win.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(in_repo);
            }
        }
        if let Some(a) = self.win.lookup_action("git-init").and_downcast::<gio::SimpleAction>() {
            a.set_enabled(!in_repo);
        }
    }

    // ------------------------------------------------------------- status

    /// Folder whose repository is shown.
    fn git_base(&self) -> Option<PathBuf> {
        self.workdir
            .borrow()
            .clone()
            .or_else(|| self.active_doc().and_then(|d| d.path.borrow().as_deref().and_then(Path::parent).map(Path::to_path_buf)))
            .filter(|p| p.is_dir())
    }

    fn git_root(&self) -> Option<PathBuf> {
        let root = self.git.borrow().as_ref().map(|s| s.root.clone());
        if root.is_none() {
            self.message("Nessun repository git");
        }
        root
    }

    pub(super) fn git_status(&self) -> Option<Rc<Status>> {
        self.git.borrow().clone()
    }

    /// Reads the status again shortly (several requests are merged).
    pub(super) fn git_refresh(self: &Rc<Self>) {
        if self.git_refresh_pending.replace(true) {
            return;
        }
        let ed = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(250), move || {
            ed.git_refresh_pending.set(false);
            ed.git_refresh_now();
        });
    }

    fn git_refresh_now(self: &Rc<Self>) {
        if self.git_refreshing.get() {
            self.git_refresh_again.set(true);
            return;
        }
        let Some(base) = self.git_base() else {
            self.set_git_status(None);
            return;
        };
        self.git_refreshing.set(true);
        let ed = self.clone();
        git::status(&base, move |st| {
            ed.git_refreshing.set(false);
            ed.set_git_status(st.map(Rc::new));
            if ed.git_refresh_again.replace(false) {
                ed.git_refresh();
            }
        });
    }

    fn set_git_status(self: &Rc<Self>, st: Option<Rc<Status>>) {
        // Watch the .git folder: commits, checkouts and staging done
        // elsewhere (e.g. in the terminal) update the status.
        let git_dir = st.as_ref().map(|s| s.git_dir.clone());
        let watched = self.git_monitor.borrow().as_ref().map(|(p, _)| p.clone());
        if git_dir != watched {
            let monitor = git_dir.and_then(|d| {
                let m = gio::File::for_path(&d)
                    .monitor_directory(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>)
                    .ok()?;
                let w = Rc::downgrade(self);
                m.connect_changed(move |_, f, _, _| {
                    // Lock files come and go during every command.
                    if f.basename().is_some_and(|n| n.extension().is_some_and(|e| e == "lock")) {
                        return;
                    }
                    if let Some(ed) = w.upgrade() {
                        ed.git_refresh();
                    }
                });
                Some((d, m))
            });
            *self.git_monitor.borrow_mut() = monitor;
        }

        let b = &self.status.branch;
        match &st {
            Some(s) => {
                b.set_label(&format!("⎇ {}", s.branch_label()));
                let mut tip = format!("Repository: {}\n", s.root.display());
                if let Some(u) = &s.upstream {
                    tip.push_str(&format!("Remoto: {u}\n"));
                }
                tip.push_str(&format!(
                    "{} in stage · {} modificati · {} non tracciati",
                    s.staged(),
                    s.count(Mark::Modified),
                    s.count(Mark::Untracked)
                ));
                let conflicts = s.count(Mark::Conflict);
                if conflicts > 0 {
                    tip.push_str(&format!(" · {conflicts} in conflitto"));
                    b.add_css_class("git-conflict");
                } else {
                    b.remove_css_class("git-conflict");
                }
                tip.push_str("\nClic: rami e merge");
                b.set_tooltip_text(Some(&tip));
                b.set_visible(true);
            }
            None => b.set_visible(false),
        }
        self.enable_git_actions(st.is_some());
        if let Some(sb) = self.sidebar.borrow().as_ref() {
            sb.set_git(st.clone());
        }
        *self.git.borrow_mut() = st;
        self.reload_changes();
    }

    // ----------------------------------------------------------- commands

    /// Runs a git command that changes the repository. With `reload`,
    /// open documents changed on disk are read again (pull, merge, …).
    fn git_op(self: &Rc<Self>, dir: &Path, args: Vec<OsString>, what: &str, reload: bool) {
        self.git_op_then(dir, args, what, reload, |_, _| {});
    }

    fn git_op_then(
        self: &Rc<Self>,
        dir: &Path,
        args: Vec<OsString>,
        what: &str,
        reload: bool,
        then: impl FnOnce(&Rc<Editor>, bool) + 'static,
    ) {
        if self.git_busy.replace(true) {
            self.message("Un comando git è già in esecuzione");
            return;
        }
        self.message(&format!("git {what}…"));
        let ed = self.clone();
        let what = what.to_string();
        let dir2 = dir.to_path_buf();
        // Network commands: questions of git and ssh go to a dialog.
        self.prepare_git(dir, args, move |args, env| {
        let argv: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        git::run_with(&dir2, &argv, &env, None, move |out| {
            ed.git_busy.set(false);
            if out.ok {
                let s = out.summary();
                ed.message(&if s.is_empty() { format!("git {what}: fatto") } else { format!("git {what}: {s}") });
            } else if ed.askpass_was_cancelled() {
                ed.message(&format!("git {what}: annullato"));
            } else {
                let mut detail = tail(&out.text(), 25);
                if needs_credentials(&out.stderr) {
                    detail.push_str(
                        "\n\nAccesso non riuscito: controlla nome utente, password o token, oppure la chiave \
                         SSH del repository in Git → Credenziali e chiavi SSH.",
                    );
                }
                ed.message(&format!("git {what}: errore"));
                ed.error_dialog(&format!("git {what} non riuscito"), &detail);
            }
            if reload {
                ed.reload_changed_docs();
            }
            ed.git_refresh();
            then(&ed, out.ok);
        });
        });
    }

    fn with_active_file(self: &Rc<Self>, f: fn(&Rc<Editor>, PathBuf)) {
        match self.active_doc().and_then(|d| d.path.borrow().clone()) {
            Some(p) => f(self, p),
            None => self.message("Il documento non è ancora stato salvato su disco"),
        }
    }

    /// Saves the modified documents that have a file, then calls `f`.
    fn save_all_then(self: &Rc<Self>, f: impl FnOnce(&Rc<Editor>) + 'static) {
        let pending: Vec<Rc<doc::Doc>> =
            self.docs.borrow().iter().filter(|d| d.is_modified() && d.path.borrow().is_some()).cloned().collect();
        self.save_chain(pending, Box::new(f));
    }

    fn save_chain(self: &Rc<Self>, mut docs: Vec<Rc<doc::Doc>>, f: AfterSave) {
        let Some(d) = docs.pop() else {
            f(self);
            return;
        };
        let ed = self.clone();
        self.save_doc(&d, false, Some(Box::new(move |ok| if ok { ed.save_chain(docs, f) })));
    }

    /// Action of the menu or of the file panel on one file or folder.
    pub(super) fn git_path_action(self: &Rc<Self>, action: &str, path: &Path) {
        let dir = dir_of(path);
        let p = path.as_os_str().to_owned();
        match action {
            "add" => {
                let ed = self.clone();
                self.save_all_then(move |_| ed.git_op(&dir, vec!["add".into(), "--".into(), p], "add", false));
            }
            "unstage" => self.git_op(&dir, vec!["restore".into(), "--staged".into(), "--".into(), p], "restore --staged", false),
            "restore" => {
                let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                let dialog = gtk::AlertDialog::builder()
                    .modal(true)
                    .message(format!("Annullare le modifiche a «{name}»?"))
                    .detail("Il contenuto tornerà quello dell'ultimo commit (o dell'area di stage). Le modifiche andranno perse.")
                    .buttons(["Annulla", "Scarta le modifiche"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                let ed = self.clone();
                dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| {
                    if res == Ok(1) {
                        ed.git_op(&dir, vec!["restore".into(), "--".into(), p], "restore", true);
                    }
                });
            }
            "refresh" => self.git_refresh(),
            "changes" => self.changes_dialog(dir),
            "resolve" => self.resolve_conflict(path),
            _ => {}
        }
    }

    /// Pull. When the current branch is linked to no remote branch, the
    /// branches of the server are fetched and the user chooses one.
    fn git_pull(self: &Rc<Self>) {
        let Some(st) = self.git_status() else {
            self.message("Nessun repository git");
            return;
        };
        if st.upstream.is_some() {
            self.git_op(&st.root, os(&["pull"]), "pull", true);
            return;
        }
        let Some(branch) = st.branch.clone() else {
            self.message("Non sei su un ramo (HEAD staccato): passa a un ramo per fare pull");
            return;
        };
        let root = st.root.clone();
        let ed = self.clone();
        git::run_str(&root.clone(), &["remote"], move |out| {
            if String::from_utf8_lossy(&out.stdout).trim().is_empty() {
                ed.error_dialog("git pull non riuscito", "Nessun repository remoto configurato (git remote add …).");
                return;
            }
            // The branches on the server are needed: fetch them first.
            ed.git_op_then(&root.clone(), os(&["fetch", "--all", "--prune"]), "fetch", false, move |ed, ok| {
                if !ok {
                    return;
                }
                let (ed, root2) = (ed.clone(), root.clone());
                git::run_str(&root, &["for-each-ref", "--format=%(refname:short)", "refs/remotes"], move |out| {
                    let refs: Vec<String> = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter(|l| l.contains('/') && !l.ends_with("/HEAD"))
                        .map(str::to_string)
                        .collect();
                    if refs.is_empty() {
                        ed.error_dialog("git pull", "Sul server non c'è ancora nessun ramo: pubblica prima il tuo con Push.");
                        return;
                    }
                    ed.upstream_dialog(true, root2, branch, refs);
                });
            });
        });
    }

    /// Push. The first time for a branch, the user chooses the remote and
    /// the name of the branch there.
    fn git_push(self: &Rc<Self>) {
        let Some(st) = self.git_status() else {
            self.message("Nessun repository git");
            return;
        };
        if st.upstream.is_some() || st.branch.is_none() {
            self.git_op(&st.root, os(&["push"]), "push", false);
            return;
        }
        let branch = st.branch.clone().unwrap_or_default();
        let ed = self.clone();
        let root = st.root.clone();
        git::run_str(&root.clone(), &["remote"], move |out| {
            let remotes: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
            if remotes.is_empty() {
                ed.error_dialog("git push non riuscito", "Nessun repository remoto configurato (git remote add …).");
                return;
            }
            ed.upstream_dialog(false, root, branch, remotes);
        });
    }

    /// Which remote branch to pull from (`choices`: "remote/branch") or
    /// which remote to push to (`choices`: remotes), and whether to link
    /// the current branch to it for the next times.
    fn upstream_dialog(self: &Rc<Self>, pull: bool, root: PathBuf, branch: String, choices: Vec<String>) {
        // The same name on the server, preferably on "origin".
        let default = if pull {
            let same = |r: &str| r.split_once('/').is_some_and(|(_, b)| b == branch);
            choices
                .iter()
                .position(|r| same(r) && r.starts_with("origin/"))
                .or_else(|| choices.iter().position(|r| same(r)))
                .or_else(|| choices.iter().position(|r| r == "origin/main" || r == "origin/master"))
                .unwrap_or(0)
        } else {
            choices.iter().position(|r| r == "origin").unwrap_or(0)
        };
        let parent = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Window>().ok())
            .find(|w| w.is_active())
            .unwrap_or_else(|| self.win.clone().upcast());
        let title = if pull { "Pull: da quale ramo remoto?" } else { "Push: su quale ramo remoto?" };
        let win = dialog_window(&parent, title);
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(14).margin_bottom(14).margin_start(14).margin_end(14).build();
        let text = if pull {
            format!("Il ramo locale «{branch}» non è collegato a nessun ramo del server. Scegli quello da cui scaricare e unire le modifiche.")
        } else {
            format!("Il ramo locale «{branch}» non è ancora sul server. Scegli dove pubblicarlo.")
        };
        let l = gtk::Label::new(Some(&text));
        l.set_wrap(true);
        l.set_xalign(0.0);
        l.set_max_width_chars(60);
        b.append(&l);
        let items: Vec<&str> = choices.iter().map(String::as_str).collect();
        let drop = gtk::DropDown::from_strings(&items);
        drop.set_selected(default as u32);
        let grid = gtk::Grid::builder().row_spacing(6).column_spacing(10).build();
        let label = |t: &str| {
            let l = gtk::Label::new(Some(t));
            l.set_xalign(0.0);
            l
        };
        grid.attach(&label(if pull { "Ramo remoto" } else { "Repository remoto" }), 0, 0, 1, 1);
        grid.attach(&drop, 1, 0, 1, 1);
        let name = gtk::Entry::builder().text(&branch).hexpand(true).activates_default(true).build();
        if !pull {
            grid.attach(&label("Nome del ramo sul server"), 0, 1, 1, 1);
            grid.attach(&name, 1, 1, 1, 1);
        }
        b.append(&grid);
        let link = gtk::CheckButton::with_label(&format!("Collega «{branch}» a questo ramo per i prossimi pull e push"));
        link.set_active(true);
        b.append(&link);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Annulla");
        let ok = gtk::Button::with_label(if pull { "Pull" } else { "Push" });
        ok.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&ok);
        b.append(&buttons);
        win.set_child(Some(&b));
        win.set_default_widget(Some(&ok));
        let w = win.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.close();
            }
        });
        let (ed, w) = (self.clone(), win.downgrade());
        ok.connect_clicked(move |_| {
            let Some(choice) = choices.get(drop.selected() as usize).cloned() else { return };
            let linked = link.is_active();
            if let Some(w) = w.upgrade() {
                w.close();
            }
            if pull {
                if linked {
                    let r = root.clone();
                    ed.git_op_then(&root, os(&["branch", "--set-upstream-to", &choice]), "branch --set-upstream-to", false, move |ed, ok| {
                        if ok {
                            ed.git_op(&r, os(&["pull"]), "pull", true);
                        }
                    });
                } else {
                    let (remote, rb) = choice.split_once('/').unwrap_or((&choice, ""));
                    ed.git_op(&root, os(&["pull", remote, rb]), "pull", true);
                }
            } else {
                let target = name.text().trim().to_string();
                let target = if target.is_empty() { branch.clone() } else { target };
                let refspec = format!("HEAD:{target}");
                let mut args = vec!["push"];
                if linked {
                    args.push("-u");
                }
                args.push(&choice);
                args.push(&refspec);
                ed.git_op(&root, os(&args), "push", false);
            }
        });
        win.present();
    }

    /// Reads again the open documents changed on disk; documents with
    /// unsaved changes are left untouched.
    fn reload_changed_docs(self: &Rc<Self>) {
        let docs: Vec<Rc<doc::Doc>> = self.docs.borrow().clone();
        let mut kept = 0;
        for d in docs {
            if !d.is_loaded() {
                continue;
            }
            let Some(path) = d.path.borrow().clone() else { continue };
            let Ok((text, enc, eol)) = doc::read_file(&path, Some(d.encoding.get())) else { continue };
            if text == d.text() {
                continue;
            }
            if d.is_modified() {
                kept += 1;
                continue;
            }
            let cursor = d.cursor_offset();
            d.encoding.set(enc);
            d.eol.set(eol);
            d.set_content(&text, false, cursor);
            if self.is_active(&d) {
                self.update_status_for(&d);
            }
        }
        if kept > 0 {
            self.message(&format!("{kept} documenti con modifiche non salvate sono cambiati su disco: non sono stati riletti"));
        }
        self.schedule_diff();
    }

    // ------------------------------------------------------------ dialogs

    fn commit_dialog(self: &Rc<Self>) {
        let Some(st) = self.git_status() else { return };
        let win = dialog_window(&self.win, &format!("Commit su {}", st.branch.as_deref().unwrap_or("HEAD")));
        let b = vbox();

        // Changed files, staged first.
        let mut files: Vec<(&PathBuf, &git::Entry)> = st.files.iter().collect();
        files.sort_by_key(|(p, e)| (e.mark() != Mark::Staged, (*p).clone()));
        let mut list = String::new();
        for (p, e) in files.iter().take(200) {
            let rel = p.strip_prefix(&st.root).unwrap_or(p);
            list.push_str(&format!("{}{}  {}\n", e.x as char, e.y as char, rel.display()));
        }
        for d in &st.untracked_dirs {
            let rel = d.strip_prefix(&st.root).unwrap_or(d);
            list.push_str(&format!("??  {}/\n", rel.display()));
        }
        let staged = st.staged();
        let modified = st.count(Mark::Modified);
        let summary = gtk::Label::new(Some(&format!(
            "{staged} file nell'area di stage · {modified} modifiche non in stage · {} non tracciati",
            st.count(Mark::Untracked)
        )));
        summary.set_xalign(0.0);
        let files_label = gtk::Label::new(Some(list.trim_end()));
        files_label.set_xalign(0.0);
        files_label.set_yalign(0.0);
        files_label.set_selectable(true);
        files_label.add_css_class("monospace");
        let files_scroll = gtk::ScrolledWindow::builder().child(&files_label).min_content_height(110).max_content_height(220).propagate_natural_height(true).build();

        let msg = gtk::TextView::new();
        msg.set_monospace(true);
        msg.set_wrap_mode(gtk::WrapMode::WordChar);
        msg.set_top_margin(6);
        msg.set_left_margin(6);
        msg.set_right_margin(6);
        let msg_scroll = gtk::ScrolledWindow::builder().child(&msg).min_content_height(110).build();
        msg_scroll.add_css_class("frame");

        let all = gtk::CheckButton::with_label("Includi tutte le modifiche ai file tracciati (commit -a)");
        all.set_active(staged == 0 && modified > 0);
        let amend = gtk::CheckButton::with_label("Modifica l'ultimo commit (--amend)");
        let msg_label = gtk::Label::new(Some("Messaggio (Ctrl+Invio per confermare):"));
        msg_label.set_xalign(0.0);

        let cancel = gtk::Button::with_label("Annulla");
        let ok = gtk::Button::with_label("Commit");
        ok.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&ok);

        b.append(&summary);
        b.append(&files_scroll);
        b.append(&msg_label);
        b.append(&msg_scroll);
        b.append(&all);
        b.append(&amend);
        b.append(&buttons);
        win.set_child(Some(&b));

        // --amend: start from the message of the last commit.
        let mw = msg.downgrade();
        let root = st.root.clone();
        amend.connect_toggled(move |c| {
            let Some(msg) = mw.upgrade() else { return };
            let buf = msg.buffer();
            if !c.is_active() || buf.char_count() > 0 {
                return;
            }
            let mw = msg.downgrade();
            git::run_str(&root, &["log", "-1", "--format=%B"], move |out| {
                if let (true, Some(msg)) = (out.ok, mw.upgrade()) {
                    msg.buffer().set_text(String::from_utf8_lossy(&out.stdout).trim_end());
                }
            });
        });

        let w = win.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.close();
            }
        });
        let ed = self.clone();
        let (w, mw, aw, amw) = (win.downgrade(), msg.downgrade(), all.downgrade(), amend.downgrade());
        let root = st.root.clone();
        let do_commit = Rc::new(move || {
            let (Some(win), Some(msg), Some(all), Some(amend)) = (w.upgrade(), mw.upgrade(), aw.upgrade(), amw.upgrade()) else {
                return;
            };
            let buf = msg.buffer();
            let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
            let mut args = os(&["commit"]);
            if all.is_active() {
                args.push("-a".into());
            }
            if amend.is_active() {
                args.push("--amend".into());
            }
            if text.is_empty() {
                if !amend.is_active() {
                    msg.error_bell();
                    msg.grab_focus();
                    return;
                }
                args.push("--no-edit".into());
            } else {
                args.push("-m".into());
                args.push(text.into());
            }
            win.close();
            ed.git_op(&root, args, "commit", false);
        });
        let f = do_commit.clone();
        ok.connect_clicked(move |_| f());
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, state| {
            if state.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                do_commit();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        msg.add_controller(keys);
        win.present();
        msg.grab_focus();
    }

    fn branch_dialog(self: &Rc<Self>) {
        let Some(st) = self.git_status() else { return };
        let ed = self.clone();
        let root = st.root.clone();
        git::run_str(
            &root,
            &["for-each-ref", "--format=%(HEAD)%09%(refname)%09%(refname:short)", "refs/heads", "refs/remotes"],
            move |out| {
                if !out.ok {
                    ed.error_dialog("Impossibile leggere i rami", &out.text());
                    return;
                }
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                // (name, is remote, is current)
                let branches: Vec<(String, bool, bool)> = text
                    .lines()
                    .filter_map(|l| {
                        let mut f = l.splitn(3, '\t');
                        let (head, full, short) = (f.next()?, f.next()?, f.next()?);
                        (!full.ends_with("/HEAD")).then(|| (short.to_string(), full.starts_with("refs/remotes/"), head == "*"))
                    })
                    .collect();
                ed.show_branch_dialog(&st, branches);
            },
        );
    }

    fn show_branch_dialog(self: &Rc<Self>, st: &Status, branches: Vec<(String, bool, bool)>) {
        let win = dialog_window(&self.win, "Rami e merge");
        win.set_default_height(460);
        let b = vbox();
        let current = gtk::Label::new(Some(&format!("Ramo corrente: {}", st.branch_label())));
        current.set_xalign(0.0);
        current.add_css_class("heading");

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("boxed-list");
        for (name, remote, head) in &branches {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(4);
            row.set_margin_bottom(4);
            row.set_margin_start(8);
            row.set_margin_end(8);
            let l = gtk::Label::new(Some(name));
            l.set_xalign(0.0);
            l.set_hexpand(true);
            if *head {
                l.add_css_class("heading");
            }
            row.append(&l);
            let kind = gtk::Label::new(Some(if *head { "corrente" } else if *remote { "remoto" } else { "" }));
            kind.add_css_class("dim-label");
            row.append(&kind);
            list.append(&row);
        }
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).min_content_height(200).build();

        let switch = gtk::Button::with_label("Passa al ramo");
        let merge = gtk::Button::with_label("Unisci nel ramo corrente (merge)");
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.append(&switch);
        actions.append(&merge);
        switch.set_sensitive(false);
        merge.set_sensitive(false);

        let new_name = gtk::Entry::builder().placeholder_text("nome del nuovo ramo").hexpand(true).build();
        let create = gtk::Button::with_label("Crea e passa al ramo");
        let new_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        new_row.append(&new_name);
        new_row.append(&create);

        b.append(&current);
        b.append(&scroll);
        b.append(&actions);
        b.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        b.append(&new_row);
        win.set_child(Some(&b));

        let branches = Rc::new(branches);
        let selected = {
            let branches = branches.clone();
            let lw = list.downgrade();
            move || -> Option<(String, bool, bool)> {
                let i = lw.upgrade()?.selected_row()?.index();
                branches.get(usize::try_from(i).ok()?).cloned()
            }
        };
        let selected = Rc::new(selected);
        {
            let (sw, mw, sel) = (switch.downgrade(), merge.downgrade(), selected.clone());
            list.connect_selected_rows_changed(move |_| {
                let s = sel();
                let other = s.as_ref().is_some_and(|(_, _, head)| !head);
                if let (Some(sw), Some(mw)) = (sw.upgrade(), mw.upgrade()) {
                    sw.set_sensitive(other);
                    mw.set_sensitive(other);
                }
            });
        }
        let root = st.root.clone();
        let do_switch = {
            let (ed, w, sel, root) = (self.clone(), win.downgrade(), selected.clone(), root.clone());
            Rc::new(move || {
                let Some((name, remote, head)) = sel() else { return };
                if head {
                    return;
                }
                // A remote branch "origin/x" becomes the local branch "x"
                // tracking it.
                let target = if remote { name.split_once('/').map_or(name.clone(), |(_, n)| n.to_string()) } else { name };
                if let Some(w) = w.upgrade() {
                    w.close();
                }
                ed.save_all_then({
                    let root = root.clone();
                    move |ed| ed.git_op(&root, vec!["switch".into(), target.into()], "switch", true)
                });
            })
        };
        let f = do_switch.clone();
        switch.connect_clicked(move |_| f());
        list.connect_row_activated(move |l, row| {
            l.select_row(Some(row));
            do_switch();
        });
        {
            let (ed, w, sel, root) = (self.clone(), win.downgrade(), selected.clone(), root.clone());
            merge.connect_clicked(move |_| {
                let Some((name, _, _)) = sel() else { return };
                if let Some(w) = w.upgrade() {
                    w.close();
                }
                let root = root.clone();
                ed.save_all_then(move |ed| ed.git_op(&root, vec!["merge".into(), "--no-edit".into(), name.into()], "merge", true));
            });
        }
        {
            let (ed, w, ew) = (self.clone(), win.downgrade(), new_name.downgrade());
            let create_branch = Rc::new(move || {
                let (Some(w), Some(e)) = (w.upgrade(), ew.upgrade()) else { return };
                let name = e.text().trim().to_string();
                if name.is_empty() || name.contains(char::is_whitespace) {
                    e.error_bell();
                    return;
                }
                w.close();
                ed.git_op(&root, vec!["switch".into(), "-c".into(), name.into()], "switch -c", true);
            });
            let f = create_branch.clone();
            create.connect_clicked(move |_| f());
            new_name.connect_activate(move |_| create_branch());
        }
        win.present();
    }
}

// ------------------------------------------------------- changes dialog

/// A changed file of the changes dialog.
struct ChangeItem {
    path: PathBuf,
    entry: git::Entry,
    excluded: bool,
}

/// Window listing the changed files of a folder, to add and commit them.
/// Files and folders can be excluded from the list; the exclusions are
/// remembered per repository and can be restored.
///
/// The list is virtual (`GtkListView`): only the visible rows exist, so
/// thousands of changed files cost little memory.
pub(super) struct Changes {
    ed: std::rc::Weak<Editor>,
    pub(super) win: gtk::Window,
    folder: PathBuf,
    summary: gtk::Label,
    /// Paths (relative to the repository) of the rows shown.
    store: gtk::StringList,
    view: gtk::ListView,
    empty: gtk::Label,
    filter: gtk::SearchEntry,
    /// Number of selected files.
    count: gtk::Label,
    show_excluded: gtk::CheckButton,
    msg: gtk::TextView,
    /// Menu of a row (one for all the rows).
    menu: gtk::PopoverMenu,
    actions: Vec<gtk::Button>,
    status: std::cell::RefCell<Option<Status>>,
    /// Changed files by relative path.
    items: std::cell::RefCell<std::collections::HashMap<String, ChangeItem>>,
    /// Files the user unchecked (kept across reloads).
    unchecked: std::cell::RefCell<std::collections::HashSet<String>>,
    excluded: std::cell::RefCell<git::Excluded>,
    /// Set while a row is being bound: its check box changes are not
    /// user choices.
    binding: std::cell::Cell<bool>,
    loading: std::cell::Cell<bool>,
    reload_again: std::cell::Cell<bool>,
}

impl Editor {
    /// Opens the changes window for `folder` (or brings it to the front).
    pub(super) fn changes_dialog(self: &Rc<Self>, folder: PathBuf) {
        let existing = self.changes.borrow().iter().find(|c| c.folder == folder).cloned();
        if let Some(c) = existing {
            c.win.present();
            c.reload();
            return;
        }
        let c = Changes::new(self, folder);
        self.changes.borrow_mut().push(c.clone());
        c.reload();
        c.win.present();
    }

    /// Updates the open changes windows.
    fn reload_changes(&self) {
        let open: Vec<Rc<Changes>> = self.changes.borrow().clone();
        for c in open {
            c.reload();
        }
    }
}

/// Relative path shown by a row (the text of its name label).
fn row_rel(row: &gtk::Widget) -> Option<String> {
    let name = row.first_child()?.next_sibling()?.next_sibling()?;
    Some(name.downcast_ref::<gtk::Label>()?.text().to_string())
}

impl Changes {
    pub(super) fn folder(&self) -> &Path {
        &self.folder
    }

    fn new(ed: &Rc<Editor>, folder: PathBuf) -> Rc<Changes> {
        let name = folder.file_name().map_or_else(|| folder.display().to_string(), |n| n.to_string_lossy().into_owned());
        let win = gtk::Window::builder()
            .title(format!("Modifiche git — {name}"))
            .transient_for(&ed.win)
            .default_width(640)
            .default_height(640)
            .build();
        let b = vbox();
        let summary = gtk::Label::new(Some("Lettura dello stato…"));
        summary.set_xalign(0.0);
        summary.set_wrap(true);

        let all = gtk::Button::with_label("Seleziona tutto");
        all.set_tooltip_text(Some("Seleziona i file mostrati (con il filtro: solo quelli che corrispondono)"));
        let none = gtk::Button::with_label("Nessuno");
        none.set_tooltip_text(Some("Deseleziona i file mostrati (con il filtro: solo quelli che corrispondono)"));
        let hide = gtk::Button::with_label("Nascondi selezionati");
        hide.set_tooltip_text(Some("Esclude dall'elenco i file selezionati tra quelli mostrati (si ripristinano da «Mostra esclusi»)"));
        let show_excluded = gtk::CheckButton::with_label("Mostra esclusi");
        show_excluded.set_hexpand(true);
        show_excluded.set_halign(gtk::Align::End);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Aggiorna"));
        let tools = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for w in [all.upcast_ref::<gtk::Widget>(), none.upcast_ref(), hide.upcast_ref(), show_excluded.upcast_ref(), refresh.upcast_ref()] {
            tools.append(w);
        }
        let filter = gtk::SearchEntry::builder()
            .placeholder_text("Filtra: .pdf  *.log  src/")
            .tooltip_text("Parte del percorso (.pdf, src/) o modello con * e ? (*.log); più termini separati da spazio: basta che ne corrisponda uno")
            .hexpand(true)
            .build();
        filter.set_width_chars(12);
        let count = gtk::Label::new(None);
        count.add_css_class("dim-label");
        let filter_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        filter_row.append(&filter);
        filter_row.append(&count);

        let store = gtk::StringList::new(&[]);
        let view = gtk::ListView::new(Some(gtk::NoSelection::new(Some(store.clone()))), None::<gtk::ListItemFactory>);
        view.add_css_class("changes-list");
        let empty = gtk::Label::new(Some("Nessuna modifica da aggiungere"));
        empty.add_css_class("dim-label");
        empty.set_valign(gtk::Align::Center);
        empty.set_visible(false);
        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).min_content_height(200).build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroll));
        overlay.add_overlay(&empty);
        overlay.add_css_class("frame");

        let msg_label = gtk::Label::new(Some("Messaggio di commit (Ctrl+Invio: commit):"));
        msg_label.set_xalign(0.0);
        let msg = gtk::TextView::new();
        msg.set_monospace(true);
        msg.set_wrap_mode(gtk::WrapMode::WordChar);
        msg.set_top_margin(6);
        msg.set_left_margin(6);
        msg.set_right_margin(6);
        let msg_scroll = gtk::ScrolledWindow::builder().child(&msg).min_content_height(80).build();
        msg_scroll.add_css_class("frame");

        let add = gtk::Button::with_label("Aggiungi (add)");
        add.set_tooltip_text(Some("Mette nell'area di stage i file selezionati"));
        let commit = gtk::Button::with_label("Commit");
        commit.set_tooltip_text(Some("Commit dei soli file selezionati"));
        commit.add_css_class("suggested-action");
        let commit_push = gtk::Button::with_label("Commit e push");
        let pull = gtk::Button::with_label("Pull");
        pull.set_hexpand(true);
        pull.set_halign(gtk::Align::End);
        let push = gtk::Button::with_label("Push");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        for w in [&add, &commit, &commit_push, &pull, &push] {
            buttons.append(w);
        }

        b.append(&summary);
        b.append(&tools);
        b.append(&filter_row);
        b.append(&overlay);
        b.append(&msg_label);
        b.append(&msg_scroll);
        b.append(&buttons);
        win.set_child(Some(&b));

        let menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_parent(&b);

        let c = Rc::new(Changes {
            ed: Rc::downgrade(ed),
            win,
            folder,
            summary,
            store,
            view,
            empty,
            filter,
            count,
            show_excluded,
            msg,
            menu,
            actions: vec![
                add.clone(),
                commit.clone(),
                commit_push.clone(),
                pull.clone(),
                push.clone(),
                all.clone(),
                none.clone(),
                hide.clone(),
            ],
            status: Default::default(),
            items: Default::default(),
            unchecked: Default::default(),
            excluded: Default::default(),
            binding: Default::default(),
            loading: Default::default(),
            reload_again: Default::default(),
        });
        c.view.set_factory(Some(&c.factory()));

        let group = gio::SimpleActionGroup::new();
        for name in ["exclude", "restore", "open", "resolve"] {
            let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let w = Rc::downgrade(&c);
            a.connect_activate(move |a, v| {
                let (Some(c), Some(arg)) = (w.upgrade(), v.and_then(|v| v.get::<String>())) else { return };
                match a.name().as_str() {
                    "exclude" => c.set_excluded(&arg, true),
                    "restore" => c.set_excluded(&arg, false),
                    "resolve" => {
                        if let Some(ed) = c.ed.upgrade() {
                            ed.resolve_conflict(Path::new(&arg));
                        }
                    }
                    _ => {
                        if let Some(ed) = c.ed.upgrade() {
                            ed.open_path(Path::new(&arg));
                        }
                    }
                }
            });
            group.add_action(&a);
        }
        c.win.insert_action_group("chg", Some(&group));

        let w = Rc::downgrade(&c);
        c.win.connect_close_request(move |_| {
            if let Some(c) = w.upgrade()
                && let Some(ed) = c.ed.upgrade()
            {
                ed.changes.borrow_mut().retain(|x| !Rc::ptr_eq(x, &c));
            }
            glib::Propagation::Proceed
        });
        let esc = gtk::EventControllerKey::new();
        let w = c.win.downgrade();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(w) = w.upgrade() {
                    w.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        c.win.add_controller(esc);

        let w = Rc::downgrade(&c);
        c.view.connect_activate(move |_, pos| {
            let Some(c) = w.upgrade() else { return };
            let Some(rel) = c.store.string(pos) else { return };
            let path = c.item_path(&rel);
            if let (Some(p), Some(ed)) = (path, c.ed.upgrade())
                && p.is_file()
            {
                ed.open_path(&p);
            }
        });
        let w = Rc::downgrade(&c);
        c.show_excluded.connect_toggled(move |_| {
            if let Some(c) = w.upgrade() {
                c.render();
            }
        });
        let w = Rc::downgrade(&c);
        refresh.connect_clicked(move |_| {
            if let Some(c) = w.upgrade() {
                c.reload();
            }
        });
        for (btn, on) in [(&all, true), (&none, false)] {
            let w = Rc::downgrade(&c);
            btn.connect_clicked(move |_| {
                let Some(c) = w.upgrade() else { return };
                {
                    let mut unchecked = c.unchecked.borrow_mut();
                    for rel in c.shown_rows().into_iter().filter(|r| c.items.borrow().get(r).is_some_and(|it| !it.excluded)) {
                        if on {
                            unchecked.remove(&rel);
                        } else {
                            unchecked.insert(rel);
                        }
                    }
                }
                c.render();
            });
        }
        let w = Rc::downgrade(&c);
        hide.connect_clicked(move |_| {
            if let Some(c) = w.upgrade() {
                c.hide_selected();
            }
        });
        let w = Rc::downgrade(&c);
        c.filter.connect_search_changed(move |_| {
            if let Some(c) = w.upgrade() {
                c.render();
            }
        });
        let w = Rc::downgrade(&c);
        add.connect_clicked(move |_| {
            if let Some(c) = w.upgrade() {
                c.add_selected(|_| {});
            }
        });
        for (btn, then_push) in [(&commit, false), (&commit_push, true)] {
            let w = Rc::downgrade(&c);
            btn.connect_clicked(move |_| {
                if let Some(c) = w.upgrade() {
                    c.commit(then_push);
                }
            });
        }
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&c);
        keys.connect_key_pressed(move |_, key, _, state| {
            if state.contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
                && let Some(c) = w.upgrade()
            {
                c.commit(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        c.msg.add_controller(keys);
        let w = Rc::downgrade(&c);
        pull.connect_clicked(move |_| {
            if let Some(ed) = w.upgrade().and_then(|c| c.ed.upgrade()) {
                ed.git_pull();
            }
        });
        let w = Rc::downgrade(&c);
        push.connect_clicked(move |_| {
            if let Some(ed) = w.upgrade().and_then(|c| c.ed.upgrade()) {
                ed.git_push();
            }
        });
        c
    }

    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let f = gtk::SignalListItemFactory::new();
        let w = Rc::downgrade(self);
        f.connect_setup(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let check = gtk::CheckButton::new();
            let letter = gtk::Label::new(None);
            letter.add_css_class("git-mark");
            letter.set_width_chars(2);
            let name = gtk::Label::new(None);
            name.set_xalign(0.0);
            name.set_hexpand(true);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            let state = gtk::Label::new(None);
            state.add_css_class("dim-label");
            let more = gtk::Button::from_icon_name("view-more-symbolic");
            more.set_has_frame(false);
            more.set_tooltip_text(Some("Escludi, ripristina, apri"));
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_start(6);
            row.set_margin_end(6);
            for x in [check.upcast_ref::<gtk::Widget>(), letter.upcast_ref(), name.upcast_ref(), state.upcast_ref(), more.upcast_ref()] {
                row.append(x);
            }
            li.set_child(Some(&row));

            let w1 = w.clone();
            check.connect_toggled(move |cb| {
                let Some(c) = w1.upgrade() else { return };
                if c.binding.get() {
                    return;
                }
                let Some(rel) = cb.parent().and_then(|r| row_rel(&r)) else { return };
                if cb.is_active() {
                    c.unchecked.borrow_mut().remove(&rel);
                } else {
                    c.unchecked.borrow_mut().insert(rel);
                }
                c.update_count();
            });
            let w2 = w.clone();
            more.connect_clicked(move |b| {
                if let (Some(c), Some(rel)) = (w2.upgrade(), b.parent().and_then(|r| row_rel(&r))) {
                    let (x, y) = (b.width() as f64 / 2.0, b.height() as f64);
                    c.popup_menu(&rel, b.upcast_ref(), x, y);
                }
            });
            let g = gtk::GestureClick::new();
            g.set_button(gdk::BUTTON_SECONDARY);
            let w3 = w.clone();
            g.connect_pressed(move |g, _, x, y| {
                if let (Some(c), Some(row)) = (w3.upgrade(), g.widget())
                    && let Some(rel) = row_rel(&row)
                {
                    c.popup_menu(&rel, &row, x, y);
                }
            });
            row.add_controller(g);
        });
        let w = Rc::downgrade(self);
        f.connect_bind(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let (Some(c), Some(row)) = (w.upgrade(), li.child()) else { return };
            let Some(rel) = li.item().and_downcast::<gtk::StringObject>().map(|s| s.string().to_string()) else { return };
            let items = c.items.borrow();
            let Some(it) = items.get(&rel) else { return };
            let mut w = row.first_child();
            let mut next = || {
                let x = w.clone();
                w = x.as_ref().and_then(|x| x.next_sibling());
                x
            };
            let (check, letter, name, state) = (next(), next(), next(), next());
            c.binding.set(true);
            if let Some(check) = check.and_downcast::<gtk::CheckButton>() {
                check.set_active(!it.excluded && !c.unchecked.borrow().contains(&rel));
                check.set_sensitive(!it.excluded);
            }
            c.binding.set(false);
            if let Some(l) = letter.and_downcast::<gtk::Label>() {
                l.set_text(&it.entry.letter().to_string());
            }
            if let Some(l) = name.and_downcast::<gtk::Label>() {
                l.set_text(&rel);
                l.set_tooltip_text(Some(&rel));
            }
            if let Some(l) = state.and_downcast::<gtk::Label>() {
                l.set_text(if it.excluded { "escluso" } else { it.entry.mark().describe() });
            }
            for cls in git::MARK_CLASSES {
                row.remove_css_class(cls);
            }
            row.add_css_class(it.entry.mark().css_class());
            if it.excluded {
                row.add_css_class("dim-label");
            } else {
                row.remove_css_class("dim-label");
            }
        });
        f
    }

    fn item_path(&self, rel: &str) -> Option<PathBuf> {
        self.items.borrow().get(rel).map(|it| it.path.clone())
    }

    /// Reads the status of the folder again.
    pub(super) fn reload(self: &Rc<Self>) {
        if self.loading.replace(true) {
            self.reload_again.set(true);
            return;
        }
        let w = Rc::downgrade(self);
        git::status_of(&self.folder, Some(&self.folder), move |st| {
            let Some(c) = w.upgrade() else { return };
            c.loading.set(false);
            let new_root = st.as_ref().map(|s| s.root.clone());
            let old_root = c.status.borrow().as_ref().map(|s| s.root.clone());
            if new_root != old_root {
                *c.excluded.borrow_mut() = new_root.as_deref().map(crate::config::load_git_excluded).unwrap_or_default();
            }
            *c.status.borrow_mut() = st;
            c.render();
            if c.reload_again.replace(false) {
                c.reload();
            }
        });
    }

    /// Rebuilds the list from the last status.
    fn render(self: &Rc<Self>) {
        let st = self.status.borrow();
        let Some(st) = st.as_ref() else {
            self.items.borrow_mut().clear();
            self.store.splice(0, self.store.n_items(), &[]);
            self.summary.set_text(&format!("{} non è in un repository git", self.folder.display()));
            for b in &self.actions {
                b.set_sensitive(false);
            }
            return;
        };
        for b in &self.actions {
            b.set_sensitive(true);
        }
        let excluded = self.excluded.borrow();
        let items: std::collections::HashMap<String, ChangeItem> = st
            .files
            .iter()
            .map(|(path, e)| {
                let rel = path.strip_prefix(&st.root).unwrap_or(path).to_string_lossy().into_owned();
                let excluded = excluded.rule_for(&rel).is_some();
                (rel, ChangeItem { path: path.clone(), entry: *e, excluded })
            })
            .collect();
        let show_excluded = self.show_excluded.is_active();
        let filter = self.filter.text();
        let mut shown: Vec<String> = items
            .iter()
            .filter(|(r, it)| (show_excluded || !it.excluded) && git::filter_matches(r, &filter))
            .map(|(r, _)| r.clone())
            .collect();
        shown.sort();
        let changed = items.values().filter(|it| !it.excluded).count();
        let hidden = items.len() - changed;
        // Items first: binding the new rows reads them.
        *self.items.borrow_mut() = items;
        let refs: Vec<&str> = shown.iter().map(String::as_str).collect();
        self.store.splice(0, self.store.n_items(), &refs);
        self.empty.set_label(if filter.trim().is_empty() { "Nessuna modifica da aggiungere" } else { "Nessun file corrisponde al filtro" });
        self.empty.set_visible(shown.is_empty());
        self.update_count();

        let folder = self.folder.strip_prefix(&st.root).ok().filter(|p| !p.as_os_str().is_empty());
        let place = match folder {
            Some(f) => format!("{} / {}", st.root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), f.display()),
            None => st.root.display().to_string(),
        };
        self.summary.set_text(&format!(
            "{place}\nRamo {} · {changed} file con modifiche{}",
            st.branch_label(),
            if hidden > 0 { format!(" · {hidden} esclusi") } else { String::new() }
        ));
        self.show_excluded.set_label(Some(&format!("Mostra esclusi ({hidden})")));
    }

    /// Relative paths of the rows currently shown.
    fn shown_rows(&self) -> Vec<String> {
        (0..self.store.n_items()).filter_map(|i| self.store.string(i)).map(|s| s.to_string()).collect()
    }

    fn update_count(&self) {
        let selected = self.selected().len();
        let shown = self.store.n_items();
        let text = if self.filter.text().trim().is_empty() {
            format!("{selected} selezionati")
        } else {
            format!("{shown} mostrati · {selected} selezionati in tutto")
        };
        self.count.set_text(&text);
    }

    /// Excludes the selected files among those shown.
    fn hide_selected(self: &Rc<Self>) {
        let rels: Vec<String> = {
            let unchecked = self.unchecked.borrow();
            let items = self.items.borrow();
            self.shown_rows()
                .into_iter()
                .filter(|r| items.get(r).is_some_and(|it| !it.excluded) && !unchecked.contains(r))
                .collect()
        };
        let Some(ed) = self.ed.upgrade() else { return };
        if rels.is_empty() {
            self.view.error_bell();
            ed.message("Nessun file selezionato tra quelli mostrati");
            return;
        }
        {
            let mut ex = self.excluded.borrow_mut();
            for r in &rels {
                ex.add(r.clone());
            }
            if let Some(st) = self.status.borrow().as_ref() {
                crate::config::save_git_excluded(&st.root, &ex);
            }
        }
        ed.message(&format!("{} file nascosti dall'elenco (ripristinabili da «Mostra esclusi»)", rels.len()));
        self.render();
    }

    fn popup_menu(&self, rel: &str, widget: &gtk::Widget, x: f64, y: f64) {
        let Some(root) = self.status.borrow().as_ref().map(|s| s.root.clone()) else { return };
        let Some((path, excluded)) = self.items.borrow().get(rel).map(|it| (it.path.clone(), it.excluded)) else { return };
        self.menu.set_menu_model(Some(&self.row_menu(rel, &path, excluded, &root)));
        let Some(parent) = self.menu.parent() else { return };
        let Some(p) = widget.compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32)) else { return };
        self.menu.set_pointing_to(Some(&gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1)));
        self.menu.popup();
    }

    /// Menu of a row: exclude the file or one of its folders, restore it, open it.
    fn row_menu(&self, rel: &str, path: &Path, excluded: bool, root: &Path) -> gio::Menu {
        let m = gio::Menu::new();
        let item = |label: &str, action: &str, arg: &str| {
            let i = gio::MenuItem::new(Some(label), None);
            i.set_action_and_target_value(Some(action), Some(&arg.to_variant()));
            i
        };
        if path.is_file() {
            m.append_item(&item("Apri", "chg.open", &path.to_string_lossy()));
        }
        if self.items.borrow().get(rel).is_some_and(|i| i.entry.mark() == Mark::Conflict) {
            m.append_item(&item("Risolvi il conflitto…", "chg.resolve", &path.to_string_lossy()));
        }
        let s = gio::Menu::new();
        if excluded {
            s.append_item(&item("Ripristina nell'elenco", "chg.restore", rel));
        } else {
            s.append_item(&item("Escludi questo file dall'elenco", "chg.exclude", rel));
            // Folders between the file and the folder of the window.
            let base = self.folder.strip_prefix(root).unwrap_or(Path::new(""));
            let mut dir = Path::new(rel).parent();
            while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty() && d.starts_with(base) && *d != base) {
                let rule = format!("{}/", d.to_string_lossy());
                s.append_item(&item(&format!("Escludi la cartella {rule}"), "chg.exclude", &rule));
                dir = d.parent();
            }
        }
        m.append_section(None, &s);
        m
    }

    fn set_excluded(self: &Rc<Self>, arg: &str, exclude: bool) {
        {
            let mut ex = self.excluded.borrow_mut();
            if exclude {
                ex.add(arg.to_string());
            } else {
                ex.restore(arg);
            }
            if let Some(st) = self.status.borrow().as_ref() {
                crate::config::save_git_excluded(&st.root, &ex);
            }
        }
        // Rebuilding the list from the menu of one of its rows: wait for
        // the menu to close.
        let w = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(c) = w.upgrade() {
                c.render();
            }
        });
    }

    fn selected(&self) -> Vec<OsString> {
        let unchecked = self.unchecked.borrow();
        self.items
            .borrow()
            .iter()
            .filter(|(rel, it)| !it.excluded && !unchecked.contains(*rel))
            .map(|(rel, _)| OsString::from(rel))
            .collect()
    }

    fn root(&self) -> Option<PathBuf> {
        self.status.borrow().as_ref().map(|s| s.root.clone())
    }

    /// `git add` of the selected files, then `then(ok)`.
    fn add_selected(self: &Rc<Self>, then: impl FnOnce(bool) + 'static) {
        let (Some(ed), Some(root)) = (self.ed.upgrade(), self.root()) else { return };
        let files = self.selected();
        if files.is_empty() {
            self.view.error_bell();
            ed.message("Nessun file selezionato");
            return;
        }
        let mut args = os(&["add", "-A", "--"]);
        args.extend(files);
        ed.save_all_then(move |ed| ed.git_op_then(&root, args, "add", false, move |_, ok| then(ok)));
    }

    /// Commit of the selected files only (and push).
    fn commit(self: &Rc<Self>, push: bool) {
        let buf = self.msg.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
        if text.is_empty() {
            self.msg.error_bell();
            self.msg.grab_focus();
            return;
        }
        let Some(st) = self.status.borrow().clone() else { return };
        let files = self.selected();
        // A merge must be committed as a whole.
        let merging = st.git_dir.join("MERGE_HEAD").exists();
        let w = Rc::downgrade(self);
        self.add_selected(move |ok| {
            let (true, Some(c)) = (ok, w.upgrade()) else { return };
            let Some(ed) = c.ed.upgrade() else { return };
            let mut args = os(&["commit", "-m"]);
            args.push(text.into());
            if !merging {
                args.push("--".into());
                args.extend(files);
            }
            let w = Rc::downgrade(&c);
            ed.git_op_then(&st.root, args, "commit", false, move |ed, ok| {
                if !ok {
                    return;
                }
                if let Some(c) = w.upgrade() {
                    c.msg.buffer().set_text("");
                }
                if push {
                    ed.git_push();
                }
            });
        });
    }
}
