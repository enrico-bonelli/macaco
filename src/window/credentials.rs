//! Authentication of push, pull and fetch: the questions of git and ssh
//! (user name, password or token, key passphrase, new host key) in a
//! dialog, HTTPS passwords remembered in the system keyring (KWallet,
//! GNOME Keyring… through `git-credential-libsecret`) and, per repository,
//! the SSH key to use (Git → Credenziali e chiavi SSH).

use super::Editor;
use crate::askpass::{self, Request};
use crate::config::{expand_home, tilde as short_home};
use crate::git::{self, RemoteKind};
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

/// git subcommands that talk to a remote.
fn is_network(args: &[OsString]) -> bool {
    args.first().and_then(|a| a.to_str()).is_some_and(|a| matches!(a, "push" | "pull" | "fetch" | "ls-remote" | "clone"))
}

/// Whether git has the Secret Service credential helper (KWallet, GNOME
/// Keyring, KeePassXC…).
fn keyring_helper_installed() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--exec-path")
            .output()
            .ok()
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("git-credential-libsecret"))
            .is_some_and(|p| p.exists())
    })
}

/// The existing keys ssh would try for `target` (from `~/.ssh/config`),
/// read in the background: `ssh -G` can run `Match exec` commands.
fn ssh_identity_files(target: &str, port: Option<u16>, done: impl FnOnce(Vec<PathBuf>) + 'static) {
    let port = port.map(|p| p.to_string());
    let mut argv: Vec<&OsStr> = vec![OsStr::new("ssh"), OsStr::new("-G")];
    if let Some(p) = &port {
        argv.extend([OsStr::new("-p"), OsStr::new(p)]);
    }
    argv.push(OsStr::new(target));
    let opts = crate::proc::Options { timeout: Some(std::time::Duration::from_secs(5)), detach: true, ..Default::default() };
    crate::proc::run(&argv, opts, move |out| {
        let keys = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("identityfile "))
            .map(expand_home)
            .filter(|p| p.exists())
            .collect();
        done(keys);
    });
}


/// What a prompt of git or ssh asks.
enum Question {
    User { server: String },
    Password { server: String },
    Passphrase { key: String },
    /// A server never seen before: its fingerprint is shown.
    HostKey,
    Other { secret: bool },
}

/// Text between the first pair of quotes (`Password for 'https://x':`).
fn quoted(s: &str) -> String {
    let mut it = s.split('\'');
    it.next();
    it.next().unwrap_or("").to_string()
}

fn classify(prompt: &str) -> Question {
    let low = prompt.to_lowercase();
    if low.contains("(yes/no") || low.contains("continue connecting") {
        Question::HostKey
    } else if low.starts_with("username") {
        Question::User { server: quoted(prompt) }
    } else if low.starts_with("password") {
        Question::Password { server: quoted(prompt) }
    } else if low.contains("passphrase") {
        Question::Passphrase { key: quoted(prompt) }
    } else {
        // Whole words: "pin" is not in "mapping" or "ping".
        let secret = low.split(|c: char| !c.is_alphanumeric()).any(|w| matches!(w, "password" | "pin" | "passcode" | "otp"));
        Question::Other { secret }
    }
}

impl Editor {
    /// Runs `git args` in `dir`; network commands get the askpass helper.
    /// `done` also tells whether a question was cancelled.
    pub(super) fn run_git(
        self: &Rc<Self>,
        dir: &Path,
        args: Vec<OsString>,
        input: Option<Vec<u8>>,
        done: impl FnOnce(&Rc<Editor>, git::Output, bool) + 'static,
    ) {
        let (ed, dir2) = (self.clone(), dir.to_path_buf());
        self.prepare_git(dir, args, move |args, env| {
            let argv: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
            git::run_with(&dir2, &argv, &env, input, move |out| {
                let cancelled = ed.askpass_was_cancelled();
                done(&ed, out, cancelled);
            });
        });
    }

    /// Arguments and environment of a git command: network commands get
    /// the askpass helper and, if wanted, the keyring helper. Calls `then`
    /// when they are ready.
    pub(super) fn prepare_git(self: &Rc<Self>, dir: &Path, args: Vec<OsString>, then: impl FnOnce(Vec<OsString>, Vec<(String, String)>) + 'static) {
        if !is_network(&args) {
            then(args, Vec::new());
            return;
        }
        self.askpass_cancelled.set(false);
        let env = self.askpass_env();
        if !self.shared.settings.borrow().git_keyring || !keyring_helper_installed() {
            then(args, env);
            return;
        }
        // Only when the user has no credential helper of their own.
        git::run_str(dir, &["config", "--get-all", "credential.helper"], move |out| {
            let own = !String::from_utf8_lossy(&out.stdout).trim().is_empty();
            let mut full = Vec::new();
            if !own {
                full.push(OsString::from("-c"));
                full.push(OsString::from("credential.helper=libsecret"));
            }
            full.extend(args);
            then(full, env);
        });
    }

    /// Whether the user cancelled a question of the last network command.
    pub(super) fn askpass_was_cancelled(&self) -> bool {
        self.askpass_cancelled.get()
    }

    fn askpass_env(self: &Rc<Self>) -> Vec<(String, String)> {
        if self.askpass.borrow().is_none() {
            let ed = Rc::downgrade(self);
            let handler: askpass::Handler = Rc::new(move |req, reply| match ed.upgrade() {
                Some(ed) => ed.askpass_dialog(req, reply),
                None => reply(None),
            });
            match askpass::Server::start(handler) {
                Ok(s) => *self.askpass.borrow_mut() = Some(s),
                Err(e) => {
                    self.message(&format!("Richiesta delle password non disponibile: {e}"));
                    return Vec::new();
                }
            }
        }
        self.askpass.borrow().as_ref().map(|s| s.env()).unwrap_or_default()
    }

    /// Ask the user what git or ssh wants to know.
    fn askpass_dialog(self: &Rc<Self>, req: Request, reply: Box<dyn FnOnce(Option<String>)>) {
        if req.kind == "none" {
            // Only a notice (e.g. "touch your security key").
            self.message(req.prompt.trim());
            reply(Some(String::new()));
            return;
        }
        let keyring = self.shared.settings.borrow().git_keyring && keyring_helper_installed();
        let q = if req.kind == "confirm" { Question::HostKey } else { classify(&req.prompt) };
        let (title, detail, secret, confirm) = match &q {
            Question::User { server } => ("Nome utente".to_string(), format!("Nome utente per {server}"), false, false),
            Question::Password { server } => {
                let github = server.contains("github.com");
                let mut d = format!("Password o token di accesso per {server}");
                if github {
                    d.push_str(
                        "\n\nGitHub non accetta la password dell'account: usa un token di accesso personale \
                         (GitHub → Settings → Developer settings → Personal access tokens).",
                    );
                }
                if keyring {
                    d.push_str("\n\nSe l'accesso riesce, git la ricorda nel portachiavi di sistema.");
                }
                ("Password".to_string(), d, true, false)
            }
            Question::Passphrase { key } => {
                ("Passphrase della chiave SSH".to_string(), format!("Passphrase della chiave {}", short_home(Path::new(key))), true, false)
            }
            Question::HostKey => (
                "Server SSH sconosciuto".to_string(),
                "È la prima connessione a questo server: controlla che l'impronta (fingerprint) sia quella giusta \
                 prima di fidarti. Il server viene poi ricordato in ~/.ssh/known_hosts."
                    .to_string(),
                false,
                true,
            ),
            Question::Other { secret } => ("Richiesta di git".to_string(), String::new(), *secret, false),
        };

        // A child of the window in front (the changes window, if open).
        let parent = crate::ui::active_window().unwrap_or_else(|| self.win.clone().upcast());
        // Escape closes it, which answers "cancel".
        let win = crate::ui::dialog_window(&parent, &title, 480);
        win.set_resizable(false);
        let b = crate::ui::dialog_box(10);
        let head = gtk::Label::new(Some(&title));
        head.add_css_class("title-4");
        head.set_xalign(0.0);
        b.append(&head);
        if !detail.is_empty() {
            let l = gtk::Label::new(Some(&detail));
            l.set_xalign(0.0);
            l.set_wrap(true);
            l.set_wrap_mode(pango::WrapMode::WordChar);
            l.set_max_width_chars(60);
            b.append(&l);
        }
        // The question as git or ssh wrote it (fingerprints included).
        let original = gtk::Label::new(Some(req.prompt.trim()));
        original.set_xalign(0.0);
        original.set_wrap(true);
        original.set_wrap_mode(pango::WrapMode::WordChar);
        original.set_max_width_chars(60);
        original.set_selectable(true);
        original.add_css_class("dim-label");
        original.add_css_class("monospace");
        b.append(&original);
        let entry: Option<gtk::Widget> = (!confirm).then(|| {
            if secret {
                gtk::PasswordEntry::builder().show_peek_icon(true).activates_default(true).build().upcast()
            } else {
                gtk::Entry::builder().activates_default(true).build().upcast()
            }
        });
        if let Some(e) = &entry {
            b.append(e);
        }
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Annulla");
        let ok = gtk::Button::with_label(if confirm { "Mi fido del server" } else { "OK" });
        ok.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&ok);
        b.append(&buttons);
        win.set_child(Some(&b));
        win.set_default_widget(Some(&ok));

        let reply = Rc::new(RefCell::new(Some(reply)));
        let answer = {
            let (reply, ed, win) = (reply.clone(), Rc::downgrade(self), win.downgrade());
            move |a: Option<String>| {
                // Closing the window after an answer comes back here: only
                // the first call counts.
                let Some(r) = reply.borrow_mut().take() else { return };
                if a.is_none()
                    && let Some(ed) = ed.upgrade()
                {
                    ed.askpass_cancelled.set(true);
                }
                r(a);
                if let Some(w) = win.upgrade() {
                    w.close();
                }
            }
        };
        let a = answer.clone();
        let e = entry.clone();
        ok.connect_clicked(move |_| {
            let text = match &e {
                Some(w) => w.downcast_ref::<gtk::Editable>().map(|x| x.text().to_string()).unwrap_or_default(),
                None => "yes".to_string(),
            };
            a(Some(text));
        });
        let a = answer.clone();
        cancel.connect_clicked(move |_| a(None));
        let a = answer;
        win.connect_close_request(move |_| {
            a(None);
            glib::Propagation::Proceed
        });
        win.present();
        match &entry {
            Some(e) => {
                e.grab_focus();
            }
            None => {
                cancel.grab_focus();
            }
        }
    }

    // ------------------------------------------------- credentials window

    pub(super) fn credentials_dialog(self: &Rc<Self>) {
        let Some(st) = self.git_status() else {
            self.message("Nessun repository git");
            return;
        };
        let root = st.root.clone();
        let ed = self.clone();
        let cmds: [&[&str]; 3] = [&["remote", "-v"], &["config", "--get-all", "credential.helper"], &["config", "--local", "--get", "core.sshCommand"]];
        git::run_many(&root.clone(), &cmds, move |outs| {
            let text = |i: usize| String::from_utf8_lossy(&outs[i].stdout).into_owned();
            let remotes = git::parse_remotes(&text(0));
            ed.show_credentials(root, remotes, text(1).trim().to_string(), text(2).trim().to_string());
        });
    }

    fn show_credentials(self: &Rc<Self>, root: PathBuf, remotes: Vec<(String, String)>, own_helper: String, ssh_cmd: String) {
        let win = crate::ui::dialog_window(&self.win, "Credenziali e chiavi SSH", 720);
        win.set_default_height(520);
        let b = crate::ui::dialog_box(12);
        let head = gtk::Label::new(Some(&format!("Repository {}", short_home(&root))));
        head.add_css_class("title-4");
        head.set_xalign(0.0);
        head.set_ellipsize(pango::EllipsizeMode::Start);
        b.append(&head);

        // HTTPS passwords.
        let keyring = gtk::CheckButton::with_label("Ricorda nomi utente e password HTTPS nel portachiavi di sistema (KWallet, GNOME Portachiavi…)");
        keyring.set_active(self.shared.settings.borrow().git_keyring);
        let note = gtk::Label::new(None);
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("dim-label");
        if !own_helper.is_empty() {
            keyring.set_sensitive(false);
            note.set_text(&format!("Usi già un tuo credential.helper ({own_helper}): macaco lascia fare a lui."));
        } else if !keyring_helper_installed() {
            keyring.set_sensitive(false);
            note.set_text("git-credential-libsecret non è installato: le password vengono chieste ogni volta.");
        } else {
            note.set_text("Le password non vengono salvate da macaco: le conserva il portachiavi, tramite il credential helper di git.");
        }
        let ed = Rc::downgrade(self);
        keyring.connect_toggled(move |c| {
            if let Some(ed) = ed.upgrade() {
                ed.shared.settings.borrow_mut().git_keyring = c.is_active();
                ed.shared.settings.borrow().save();
            }
        });
        b.append(&keyring);
        b.append(&note);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 10);
        if remotes.is_empty() {
            let l = gtk::Label::new(Some("Nessun repository remoto (git remote add …)."));
            l.set_xalign(0.0);
            list.append(&l);
        }
        let ssh_cmd = Rc::new(RefCell::new(ssh_cmd));
        for (name, url) in &remotes {
            list.append(&self.remote_box(&root, name, url, &ssh_cmd));
        }
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        b.append(&scroll);
        let close = gtk::Button::with_label("Chiudi");
        close.set_halign(gtk::Align::End);
        let w = win.downgrade();
        close.connect_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.close();
            }
        });
        b.append(&close);
        win.set_child(Some(&b));
        win.present();
    }

    /// One remote: how it authenticates and what can be done about it.
    fn remote_box(self: &Rc<Self>, root: &Path, name: &str, url: &str, ssh_cmd: &Rc<RefCell<String>>) -> gtk::Frame {
        let kind = git::remote_kind(url);
        let frame = gtk::Frame::new(None);
        let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(8).margin_bottom(8).margin_start(10).margin_end(10).build();
        let title = gtk::Label::new(None);
        let what = match &kind {
            RemoteKind::Https { .. } => "HTTPS · nome utente e password o token",
            RemoteKind::Ssh { .. } => "SSH · chiave",
            RemoteKind::Local => "locale · nessuna autenticazione",
            RemoteKind::Other => "protocollo git · nessuna autenticazione",
        };
        title.set_markup(&format!("<b>{}</b>  {}  <span alpha=\"60%\">({what})</span>", glib::markup_escape_text(name), glib::markup_escape_text(url)));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_wrap_mode(pango::WrapMode::WordChar);
        b.append(&title);
        let info = gtk::Label::new(None);
        info.set_xalign(0.0);
        info.set_wrap(true);
        info.set_wrap_mode(pango::WrapMode::WordChar);
        info.set_selectable(true);
        b.append(&info);
        let result = gtk::Label::new(None);
        result.set_xalign(0.0);
        result.set_wrap(true);
        result.set_visible(false);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let button = |label: &str| {
            let x = gtk::Button::with_label(label);
            buttons.append(&x);
            x
        };

        match &kind {
            RemoteKind::Ssh { user, host, port } => {
                let target = match user {
                    Some(u) => format!("{u}@{host}"),
                    None => host.clone(),
                };
                let port = *port;
                let key_label = info.downgrade();
                let show_key = {
                    let (target, ssh_cmd) = (target.clone(), ssh_cmd.clone());
                    move || {
                        let Some(label) = key_label.upgrade() else { return };
                        let cmd = ssh_cmd.borrow().clone();
                        if let Some(k) = git::ssh_command_key(&cmd) {
                            label.set_text(&format!(
                                "Chiave scelta per questo repository: {} (provata per prima; ssh può provare anche quelle di ~/.ssh/config per lo stesso server)",
                                short_home(&expand_home(&k))
                            ));
                            return;
                        }
                        let key_label = key_label.clone();
                        ssh_identity_files(&target, port, move |keys| {
                            let Some(label) = key_label.upgrade() else { return };
                            label.set_text(&match keys.as_slice() {
                                [] => "Nessuna chiave trovata per questo server: creane una (ssh-keygen) o scegline una.".to_string(),
                                [k] => format!("Chiave: {} (dalla configurazione di ssh)", short_home(k)),
                                ks => format!("Chiavi provate: {}", ks.iter().map(|k| short_home(k)).collect::<Vec<_>>().join(", ")),
                            });
                        });
                    }
                };
                show_key();
                let show_key = Rc::new(show_key);
                let choose = button("Usa un'altra chiave per questo repository…");
                let (ed, root2, ssh_cmd2, sk) = (Rc::downgrade(self), root.to_path_buf(), ssh_cmd.clone(), show_key.clone());
                choose.connect_clicked(move |btn| {
                    let Some(ed) = ed.upgrade() else { return };
                    let dialog = gtk::FileDialog::builder().title("Chiave privata SSH").modal(true).build();
                    dialog.set_initial_folder(Some(&gio::File::for_path(expand_home("~/.ssh/"))));
                    let parent = btn.root().and_downcast::<gtk::Window>();
                    let (root, ssh_cmd, sk, ed2) = (root2.clone(), ssh_cmd2.clone(), sk.clone(), ed.clone());
                    dialog.open(parent.as_ref(), None::<&gio::Cancellable>, move |res| {
                        let Some(path) = res.ok().and_then(|f| f.path()) else { return };
                        let path = path.strip_suffix_pub();
                        let cmd = git::ssh_command_for(&path.to_string_lossy());
                        let (ssh_cmd, sk, ed3) = (ssh_cmd.clone(), sk.clone(), ed2.clone());
                        let value = cmd.clone();
                        git::run_str(&root, &["config", "--local", "core.sshCommand", &value], move |out| {
                            if out.ok {
                                *ssh_cmd.borrow_mut() = cmd;
                                sk();
                                ed3.message("Chiave SSH impostata per questo repository (core.sshCommand)");
                            } else {
                                ed3.message(&format!("git config: {}", out.summary()));
                            }
                        });
                    });
                });
                let reset = button("Usa la configurazione di ssh");
                reset.set_tooltip_text(Some("Toglie la chiave scelta per questo repository: vale ~/.ssh/config"));
                let (ed, root2, ssh_cmd2, sk) = (Rc::downgrade(self), root.to_path_buf(), ssh_cmd.clone(), show_key.clone());
                reset.connect_clicked(move |_| {
                    let (ed, ssh_cmd, sk) = (ed.clone(), ssh_cmd2.clone(), sk.clone());
                    git::run_str(&root2, &["config", "--local", "--unset", "core.sshCommand"], move |_| {
                        ssh_cmd.borrow_mut().clear();
                        sk();
                        if let Some(ed) = ed.upgrade() {
                            ed.message("Per questo repository vale di nuovo la configurazione di ssh");
                        }
                    });
                });
                let copy = button("Copia chiave pubblica");
                let (ed, ssh_cmd2, target2) = (Rc::downgrade(self), ssh_cmd.clone(), target.clone());
                copy.connect_clicked(move |btn| {
                    let copy_key = {
                        let (ed, btn) = (ed.clone(), btn.clone());
                        move |key: Option<PathBuf>| {
                            let Some(ed) = ed.upgrade() else { return };
                            let Some(key) = key else {
                                ed.message("Nessuna chiave da copiare");
                                return;
                            };
                            let mut public = key.into_os_string();
                            public.push(".pub");
                            match std::fs::read_to_string(&public) {
                                Ok(text) => {
                                    btn.clipboard().set_text(text.trim());
                                    ed.message(&format!("Copiata {}: incollala tra le chiavi SSH del server (GitHub → Settings → SSH keys)", short_home(Path::new(&public))));
                                }
                                Err(e) => ed.message(&format!("{}: {e}", short_home(Path::new(&public)))),
                            }
                        }
                    };
                    match git::ssh_command_key(&ssh_cmd2.borrow()) {
                        Some(k) => copy_key(Some(expand_home(&k))),
                        None => ssh_identity_files(&target2, port, move |keys| copy_key(keys.into_iter().next())),
                    }
                });
            }
            RemoteKind::Https { host, scheme } => {
                info.set_text("Al primo push o pull macaco chiede nome utente e password (o token); con il portachiavi attivo git li ricorda.");
                let forget = button("Dimentica la password salvata");
                let (ed, root2, host, scheme) = (Rc::downgrade(self), root.to_path_buf(), host.clone(), *scheme);
                forget.connect_clicked(move |_| {
                    let ed = ed.clone();
                    let input = format!("protocol={scheme}\nhost={host}\n\n").into_bytes();
                    let args: Vec<&OsStr> = ["-c", "credential.helper=", "-c", "credential.helper=libsecret", "credential", "reject"].iter().map(OsStr::new).collect();
                    let host2 = host.clone();
                    git::run_with(&root2, &args, &[], Some(input), move |out| {
                        if let Some(ed) = ed.upgrade() {
                            ed.message(&if out.ok {
                                format!("Credenziali di {host2} tolte dal portachiavi")
                            } else {
                                format!("Portachiavi: {}", out.summary())
                            });
                        }
                    });
                });
                if !keyring_helper_installed() {
                    forget.set_sensitive(false);
                }
            }
            RemoteKind::Local | RemoteKind::Other => {}
        }
        if !matches!(kind, RemoteKind::Local) {
            let test = button("Prova la connessione");
            let (ed, root2, name2, result2) = (Rc::downgrade(self), root.to_path_buf(), name.to_string(), result.clone());
            test.connect_clicked(move |btn| {
                let Some(ed) = ed.upgrade() else { return };
                // Its password questions would mix with those of a push or pull.
                if ed.git_busy.get() {
                    ed.message("Un comando git è già in esecuzione");
                    return;
                }
                btn.set_sensitive(false);
                result2.set_visible(true);
                result2.remove_css_class("error");
                result2.set_text("Connessione in corso…");
                let args = vec![OsString::from("ls-remote"), OsString::from("--heads"), OsString::from(&name2)];
                let (result, btn) = (result2.clone(), btn.downgrade());
                ed.run_git(&root2, args, None, move |_, out, cancelled| {
                    if let Some(b) = btn.upgrade() {
                        b.set_sensitive(true);
                    }
                    if out.ok {
                        let n = String::from_utf8_lossy(&out.stdout).lines().count();
                        result.set_text(&format!("✓ Connessione riuscita: {n} rami sul server"));
                    } else if cancelled {
                        result.set_text("Annullato");
                    } else {
                        result.add_css_class("error");
                        result.set_text(&format!("Non riuscita: {}", out.summary()));
                    }
                });
            });
        }
        if buttons.first_child().is_some() {
            b.append(&buttons);
        }
        b.append(&result);
        frame.set_child(Some(&b));
        frame
    }
}

trait StripPub {
    /// The private key when the public one (`.pub`) was chosen.
    fn strip_suffix_pub(&self) -> PathBuf;
}

impl StripPub for PathBuf {
    fn strip_suffix_pub(&self) -> PathBuf {
        match self.to_str().and_then(|s| s.strip_suffix(".pub")) {
            Some(p) => PathBuf::from(p),
            None => self.clone(),
        }
    }
}
