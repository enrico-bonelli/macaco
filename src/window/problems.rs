//! "Verifica il file" (F5): the language servers of the working folder
//! check the current document and its problems are listed in a panel below
//! the editor, with the faulty text underlined.
//!
//! Servers push their diagnostics (`textDocument/publishDiagnostics`) by
//! themselves after every change. The check sends the current text and
//! waits until every server of the document has answered and gone quiet; a
//! server with nothing new to say keeps its previous diagnostics.

use super::Editor;
use crate::doc::Doc;
use crate::lsp::{self, Diagnostic, Severity};
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(150);
/// After diagnostics arrive, wait this long for more (several passes).
const SETTLE: Duration = Duration::from_millis(700);
/// A ready server silent this long after the text was sent has nothing
/// new: its previous diagnostics are still valid.
const QUIET: Duration = Duration::from_millis(2500);
/// Give up waiting (indexing of a big project).
const TIMEOUT: Duration = Duration::from_secs(45);

/// Latest diagnostics pushed by the servers: (server, URI) → (time, problems).
pub(super) type DiagStore = HashMap<(&'static str, String), (Instant, Rc<Vec<Diagnostic>>)>;

pub(super) struct ProblemsUi {
    pub(super) revealer: gtk::Revealer,
    title: gtk::Label,
    spinner: gtk::Spinner,
    list: gtk::ListBox,
}

/// A check waiting for the servers.
pub(super) struct Check {
    doc: Weak<Doc>,
    uri: String,
    servers: Vec<&'static str>,
    started: Instant,
    /// Last time each server was seen starting or busy (indexing).
    busy: HashMap<&'static str, Instant>,
    /// Servers that stopped before answering.
    lost: Vec<&'static str>,
}

/// Problems listed in the panel.
#[derive(Default)]
pub(super) struct Shown {
    doc: Weak<Doc>,
    /// Loads of the document when the problems were found (its marks are
    /// lost when it is unloaded to save memory).
    loads: u32,
    /// Position of each listed problem: line and column (in characters) at
    /// check time and a mark that follows the edits.
    items: Vec<(u32, u32, gtk::TextMark)>,
}

pub(super) fn build_panel() -> ProblemsUi {
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(pango::EllipsizeMode::End);
    title.add_css_class("problems-title");
    let spinner = gtk::Spinner::new();
    let recheck = gtk::Button::with_label("Verifica di nuovo");
    recheck.add_css_class("flat");
    recheck.set_action_name(Some("win.check-file"));
    recheck.set_tooltip_text(Some("Verifica di nuovo il file (F5)"));
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.set_action_name(Some("win.problems-close"));
    close.set_tooltip_text(Some("Chiudi il pannello e togli le sottolineature"));
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.append(&title);
    head.append(&spinner);
    head.append(&recheck);
    head.append(&close);

    let list = gtk::ListBox::new();
    list.set_activate_on_single_click(true);
    list.add_css_class("problems-list");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(200)
        .build();

    let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
    b.add_css_class("problems-panel");
    b.append(&head);
    b.append(&scroll);
    let revealer = gtk::Revealer::builder()
        .child(&b)
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(120)
        .build();
    ProblemsUi { revealer, title, spinner, list }
}

impl Editor {
    pub(super) fn setup_problems_actions(self: &Rc<Self>) {
        let a = gio::SimpleAction::new("check-file", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| ed.check_file());
        self.win.add_action(&a);
        self.app.set_accels_for_action("win.check-file", &["F5"]);
        let a = gio::SimpleAction::new("problems-close", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| ed.close_problems());
        self.win.add_action(&a);
        let ed = Rc::downgrade(self);
        self.problems.list.connect_row_activated(move |_, row| {
            if let Some(ed) = ed.upgrade() {
                ed.jump_to_problem(row.index().max(0) as usize);
            }
        });
    }

    /// A server sent the diagnostics of a document.
    pub(super) fn lsp_diagnostics(&self, server: &'static str, params: &serde_json::Value) {
        if let Some((uri, diags)) = lsp::parse_diagnostics(params) {
            self.lsp_diags.borrow_mut().insert((server, uri), (Instant::now(), Rc::new(diags)));
        }
    }

    /// A server stopped: its diagnostics are gone.
    pub(super) fn forget_diagnostics(&self, server: &str) {
        self.lsp_diags.borrow_mut().retain(|(s, _), _| *s != server);
    }

    /// Why the language servers cannot check `d`, if they cannot.
    fn check_unavailable(&self, d: &Doc) -> Option<String> {
        let Some(path) = d.path.borrow().clone() else {
            return Some("Salva il file nella cartella di lavoro per poterlo verificare".into());
        };
        let Some(root) = self.workdir.borrow().clone() else {
            return Some("La verifica usa i language server: apri una cartella di lavoro e attiva File → Analisi del codice con language server".into());
        };
        if !path.starts_with(&root) {
            return Some("Il file non è nella cartella di lavoro: la verifica funziona solo per i suoi file".into());
        }
        if !self.lsp_enabled() {
            return Some("Attiva File → Analisi del codice con language server per verificare i file di questa cartella".into());
        }
        if self.lsp_specs_for(d).is_empty() {
            return Some(format!("Nessun language server per {}", d.lang.get().label()));
        }
        None
    }

    fn check_file(self: &Rc<Self>) {
        let Some(d) = self.active_doc() else { return };
        if let Some(why) = self.check_unavailable(&d) {
            self.message(&why);
            return;
        }
        let specs = self.lsp_specs_for(&d);
        let clients = self.lsp_clients_for(&d);
        if clients.is_empty() {
            let s = specs[0];
            self.message(&format!("{} non è disponibile: installa {}", s.name, s.package));
            return;
        }
        let path = d.path.borrow().clone().unwrap_or_default();
        let uri = lsp::path_to_uri(&path);
        // Send the current text (a new version makes the servers check it
        // again); servers still starting get it when they are ready.
        self.lsp_dirty.borrow_mut().remove(&*d.id.borrow());
        let text = d.text();
        for c in &clients {
            if c.docs.borrow().contains_key(&uri) {
                c.did_change(&uri, &text);
            } else if c.ready.get() {
                c.did_open(&uri, lsp::language_id(d.lang.get(), &path), &text);
            }
        }
        let servers: Vec<&'static str> = clients.iter().map(|c| c.spec.name).collect();
        let started = Instant::now();
        *self.lsp_check.borrow_mut() =
            Some(Check { doc: Rc::downgrade(&d), uri, servers: servers.clone(), started, busy: HashMap::new(), lost: Vec::new() });
        self.clear_shown();
        let p = &self.problems;
        p.title.set_text(&format!("{} — verifica in corso ({})…", d.title(), servers.join(", ")));
        p.spinner.set_spinning(true);
        p.revealer.set_reveal_child(true);
        let ed = Rc::downgrade(self);
        glib::timeout_add_local(POLL, move || {
            let Some(ed) = ed.upgrade() else { return glib::ControlFlow::Break };
            let still_mine = ed.lsp_check.borrow().as_ref().is_some_and(|c| c.started == started);
            if still_mine && !ed.poll_check() { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
        });
    }

    /// Whether the running check is over (results shown).
    fn poll_check(self: &Rc<Self>) -> bool {
        let now = Instant::now();
        let mut done = true;
        let mut waiting_for = Vec::new();
        {
            let mut check = self.lsp_check.borrow_mut();
            let Some(c) = check.as_mut() else { return true };
            if c.doc.upgrade().is_none() {
                drop(check);
                self.close_problems();
                return true;
            }
            let clients = self.lsp_clients.borrow();
            let status = self.lsp_status.borrow();
            let diags = self.lsp_diags.borrow();
            for &s in &c.servers {
                let client = clients.iter().find(|x| x.spec.name == s && !x.stopping.get());
                let Some(client) = client else {
                    if !c.lost.contains(&s) {
                        c.lost.push(s);
                    }
                    continue;
                };
                let busy = !client.ready.get() || status.get(s).is_some_and(|t| !t.is_empty());
                if busy {
                    c.busy.insert(s, now);
                }
                let since = c.busy.get(s).copied().unwrap_or(c.started).max(c.started);
                let pushed = diags.get(&(s, c.uri.clone())).map(|(t, _)| *t).filter(|t| *t > c.started);
                let ready = !busy
                    && match pushed {
                        Some(t) => now - t >= SETTLE,
                        None => now - since >= QUIET,
                    };
                if !ready {
                    done = false;
                    waiting_for.push(match status.get(s) {
                        Some(t) if !t.is_empty() => format!("{s}: {t}"),
                        _ => s.to_string(),
                    });
                }
            }
            if !done && now - c.started < TIMEOUT {
                if let Some(d) = c.doc.upgrade() {
                    self.problems.title.set_text(&format!("{} — verifica in corso ({})…", d.title(), waiting_for.join(", ")));
                }
                return false;
            }
        }
        let check = self.lsp_check.borrow_mut().take();
        if let Some(c) = check {
            self.show_problems(&c, done);
        }
        true
    }

    fn show_problems(&self, c: &Check, complete: bool) {
        let p = &self.problems;
        p.spinner.set_spinning(false);
        let Some(d) = c.doc.upgrade() else { return };
        let mut all: Vec<Diagnostic> = Vec::new();
        // Problems of the saved file left out because the text changed.
        let mut of_saved = 0;
        {
            let diags = self.lsp_diags.borrow();
            let modified = d.is_modified();
            for &s in &c.servers {
                if let Some((_, v)) = diags.get(&(s, c.uri.clone())) {
                    for x in v.iter().filter(|x| x.severity != Severity::Hint) {
                        if modified && from_saved_file(s, x) {
                            of_saved += 1;
                        } else {
                            all.push(x.clone());
                        }
                    }
                }
            }
        }
        all.sort_by_key(|a| (a.line, a.u16col, a.severity));
        all.dedup_by(|a, b| a.line == b.line && a.u16col == b.u16col && a.message == b.message);
        let count = |sev: Severity| all.iter().filter(|x| x.severity == sev).count();
        let (errors, warnings, infos) = (count(Severity::Error), count(Severity::Warning), count(Severity::Info));
        let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        if errors > 0 {
            parts.push(plural(errors, "errore", "errori"));
        }
        if warnings > 0 {
            parts.push(plural(warnings, "avviso", "avvisi"));
        }
        if infos > 0 {
            parts.push(plural(infos, "nota", "note"));
        }
        let failed = c.lost.len() == c.servers.len();
        let summary = match () {
            _ if !parts.is_empty() => parts.join(", "),
            _ if failed => "verifica non riuscita".to_string(),
            _ => "nessun problema trovato ✓".to_string(),
        };
        let mut late = if !c.lost.is_empty() {
            format!(" — {} si è fermato prima di rispondere", c.lost.join(", "))
        } else if complete {
            String::new()
        } else {
            " — il server non ha finito in tempo: risultato parziale".to_string()
        };
        if of_saved > 0 {
            late.push_str(&format!(
                " — {of_saved} segnalazioni del compilatore riguardano la versione salvata: salva e verifica di nuovo"
            ));
        }
        p.title.set_text(&format!("{} — {summary} ({}){late}", d.title(), c.servers.join(", ")));
        self.message(&format!("Verifica di {}: {summary}", d.title()));

        // List and underlines.
        let buf = &d.buffer;
        let tags = &self.shared.tags;
        let mut items = Vec::new();
        for x in &all {
            let at = |line: u32, u16col: u32| {
                let mut it = buf.iter_at_line(line as i32).unwrap_or_else(|| buf.end_iter());
                let mut end = it;
                if !end.ends_line() {
                    end.forward_to_line_end();
                }
                let col = lsp::char_col(&buf.text(&it, &end, true), u16col) as i32;
                it.set_line_offset(col.min(end.line_offset()));
                it
            };
            let start = at(x.line, x.u16col);
            let mut end = at(x.end_line, x.end_u16col);
            if end <= start {
                // Empty range: underline the character there (or before it
                // at the end of a line).
                end = start;
                if !end.ends_line() {
                    end.forward_char();
                }
            }
            let mut s = start;
            if s == end && !s.starts_line() {
                s.backward_char();
            }
            match x.severity {
                Severity::Error => buf.apply_tag(&tags.diag_error, &s, &end),
                Severity::Warning => buf.apply_tag(&tags.diag_warning, &s, &end),
                _ => {}
            }
            items.push((start.line() as u32, start.line_offset() as u32, buf.create_mark(None, &start, true)));
            p.list.append(&problem_row(x, start.line() + 1, start.line_offset() + 1));
        }
        *self.problems_shown.borrow_mut() = Shown { doc: Rc::downgrade(&d), loads: d.loads(), items };
    }

    /// Remove the listed problems and their underlines.
    fn clear_shown(&self) {
        let shown = std::mem::take(&mut *self.problems_shown.borrow_mut());
        if let Some(d) = shown.doc.upgrade() {
            let (a, b) = d.buffer.bounds();
            for t in self.shared.tags.diag_tags() {
                d.buffer.remove_tag(t, &a, &b);
            }
            for (_, _, m) in &shown.items {
                d.buffer.delete_mark(m);
            }
        }
        let list = &self.problems.list;
        while let Some(row) = list.first_child() {
            list.remove(&row);
        }
    }

    pub(super) fn close_problems(&self) {
        *self.lsp_check.borrow_mut() = None;
        self.clear_shown();
        self.problems.spinner.set_spinning(false);
        self.problems.revealer.set_reveal_child(false);
    }

    pub(super) fn jump_to_problem(self: &Rc<Self>, i: usize) {
        let target = {
            let shown = self.problems_shown.borrow();
            shown.doc.upgrade().map(|d| (d, shown.loads, shown.items.get(i).map(|(l, c, m)| (*l, *c, m.clone()))))
        };
        let Some((d, loads, Some((line, col, mark)))) = target else {
            if target.is_none() {
                self.message("Il file verificato è stato chiuso");
                self.close_problems();
            }
            return;
        };
        self.focus_doc(&d);
        if d.ensure_loaded().is_err() {
            return;
        }
        let buf = &d.buffer;
        let it = if d.loads() == loads {
            buf.iter_at_mark(&mark)
        } else {
            // Reloaded since the check: the marks are lost, use the
            // position found then.
            let mut it = buf.iter_at_line(line as i32).unwrap_or_else(|| buf.end_iter());
            let mut end = it;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            it.set_line_offset((col as i32).min(end.line_offset()));
            it
        };
        buf.place_cursor(&it);
        d.view.scroll_to_mark(&buf.get_insert(), 0.1, true, 0.0, 0.4);
        d.view.grab_focus();
    }
}

/// Diagnostics computed by compiling the file on disk (rust-analyzer runs
/// `cargo check` when it is saved), not valid for unsaved changes.
fn from_saved_file(server: &str, x: &Diagnostic) -> bool {
    server == "rust-analyzer" && (x.source.starts_with("rustc") || x.source.starts_with("clippy"))
}

fn problem_row(x: &Diagnostic, line: i32, col: i32) -> gtk::ListBoxRow {
    let (label, class) = match x.severity {
        Severity::Error => ("errore", "problem-error"),
        Severity::Warning => ("avviso", "problem-warning"),
        _ => ("nota", "problem-info"),
    };
    let sev = gtk::Label::new(Some(label));
    sev.add_css_class(class);
    sev.set_width_chars(7);
    sev.set_xalign(0.0);
    sev.set_valign(gtk::Align::Start);
    let pos = gtk::Label::new(Some(&format!("{line}:{col}")));
    pos.add_css_class("problem-pos");
    pos.set_width_chars(8);
    pos.set_xalign(0.0);
    pos.set_valign(gtk::Align::Start);
    // Long messages (rustc, TypeScript) span several lines: the first
    // one in the row, all of them in the tooltip.
    let first = x.message.lines().next().unwrap_or("");
    let msg = gtk::Label::new(Some(first));
    msg.set_xalign(0.0);
    msg.set_hexpand(true);
    msg.set_wrap(true);
    msg.set_wrap_mode(pango::WrapMode::WordChar);
    if x.message.contains('\n') {
        msg.set_tooltip_text(Some(&x.message));
    }
    let src = gtk::Label::new(Some(&x.source));
    src.add_css_class("dim-label");
    src.set_valign(gtk::Align::Start);
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.append(&sev);
    b.append(&pos);
    b.append(&msg);
    b.append(&src);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&b));
    row
}

#[cfg(debug_assertions)]
impl Editor {
    pub(super) fn debug_problems(&self) {
        let p = &self.problems;
        eprintln!("PROBLEMS visible={} title={:?}", p.revealer.reveals_child(), p.title.text());
        let mut row = p.list.first_child();
        while let Some(r) = row {
            let mut texts = Vec::new();
            let mut c = r.first_child().and_then(|b| b.first_child());
            while let Some(w) = c {
                if let Some(l) = w.downcast_ref::<gtk::Label>() {
                    texts.push(l.text().to_string());
                }
                c = w.next_sibling();
            }
            eprintln!("  {}", texts.join(" | "));
            row = r.next_sibling();
        }
    }
}
