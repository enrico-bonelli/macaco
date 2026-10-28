//! Compare and merge (Visualizza → Confronta e unisci, Git → Risolvi il
//! conflitto): file 1 on the left, the result in the middle (editable),
//! file 2 on the right. Bands between the columns link the blocks that
//! differ; the arrow of a block copies it into the result (Ctrl: after the
//! block of the result, Shift: before it). For a git conflict the result
//! starts as the automatic three-way merge, with the conflicts in red.

use super::Editor;
use crate::config;
use crate::diff::{self, DiffResult, HunkKind};
use crate::doc::Doc;
use crate::git;
use crate::lang;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, graphene, pango};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

const LINK_WIDTH: i32 = 46;
const ARROW: f64 = 7.0;
/// Pause in typing before the differences are computed again.
const RECOMPUTE: Duration = Duration::from_millis(250);

/// Where the result goes.
pub(super) enum Target {
    /// Two files (or tabs): saved into one of them or elsewhere.
    Files { left: Option<PathBuf>, right: Option<PathBuf> },
    /// A file in conflict in a git repository.
    Git { root: PathBuf, rel: String, file: PathBuf },
}

/// One side of the merge as shown: its text, title and language.
pub(super) struct Side {
    pub(super) text: String,
    pub(super) title: String,
    pub(super) path: Option<PathBuf>,
}

pub(super) struct Merge {
    ed: Weak<Editor>,
    pub(super) win: gtk::Window,
    /// Left, result, right.
    panes: [Rc<Doc>; 3],
    /// Between left and result, between result and right.
    links: [gtk::DrawingArea; 2],
    /// Differences of the left and of the right side from the result
    /// (old = side, new = result).
    diffs: RefCell<[DiffResult; 2]>,
    /// Arrows drawn: (link, x, y, hunk index).
    arrows: RefCell<Vec<(usize, f64, f64, usize)>>,
    summary: gtk::Label,
    target: Target,
    pending: Cell<bool>,
    syncing: Cell<bool>,
}

/// Whether two line ranges of the result overlap (an empty range is the
/// point between two lines).
fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() || b.is_empty() {
        (a.start >= b.start && a.start <= b.end) || (b.start >= a.start && b.start <= a.end)
    } else {
        a.start < b.end && b.start < a.end
    }
}

fn pane_box(title: &str, doc: &Rc<Doc>) -> gtk::Box {
    let head = gtk::Label::new(Some(title));
    head.set_xalign(0.0);
    head.set_ellipsize(pango::EllipsizeMode::Start);
    head.add_css_class("heading");
    head.set_margin_start(6);
    head.set_margin_bottom(4);
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.set_hexpand(true);
    b.append(&head);
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&doc.scroller));
    frame.set_vexpand(true);
    b.append(&frame);
    b
}

impl Editor {
    /// Opens the merge window.
    pub(super) fn open_merge(self: &Rc<Self>, left: Side, center: String, right: Side, target: Target, title: &str) {
        let m = Merge::new(self, left, center, right, target, title);
        self.merges.borrow_mut().push(m.clone());
        m.win.present();
    }

    /// Visualizza → Confronta e unisci: the two tabs side by side, or the
    /// active tab and a file to choose.
    pub(super) fn merge_tabs(self: &Rc<Self>) {
        let (Some(l), Some(r)) = (self.current_doc(0), self.notebooks[1].is_visible().then(|| self.current_doc(1)).flatten()) else {
            self.merge_with_file();
            return;
        };
        for d in [&l, &r] {
            if let Err(e) = d.ensure_loaded() {
                self.message(&format!("Errore: {e}"));
                return;
            }
        }
        let side = |d: &Rc<Doc>| Side { text: d.text(), title: d.path.borrow().as_deref().map_or_else(|| d.title(), short), path: d.path.borrow().clone() };
        let (left, right) = (side(&l), side(&r));
        let target = Target::Files { left: left.path.clone(), right: right.path.clone() };
        let center = left.text.clone();
        self.open_merge(left, center, right, target, "Confronto e unione");
    }

    /// Compare the active tab with a file chosen from disk.
    pub(super) fn merge_with_file(self: &Rc<Self>) {
        let Some(d) = self.active_doc() else { return };
        if d.ensure_loaded().is_err() {
            return;
        }
        let dialog = gtk::FileDialog::builder().title(format!("Confronta «{}» con…", d.title())).modal(true).build();
        if let Some(dir) = d.path.borrow().as_deref().and_then(Path::parent) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let ed = self.clone();
        dialog.open(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            let Some(path) = res.ok().and_then(|f| f.path()) else { return };
            let text = match crate::doc::read_file(&path, None) {
                Ok((t, _, _)) => t,
                Err(e) => {
                    ed.error_dialog("Impossibile leggere il file", &format!("{}: {e}", path.display()));
                    return;
                }
            };
            let left = Side { text: d.text(), title: d.path.borrow().as_deref().map_or_else(|| d.title(), short), path: d.path.borrow().clone() };
            let right = Side { text, title: short(&path), path: Some(path.clone()) };
            let target = Target::Files { left: left.path.clone(), right: Some(path) };
            let center = left.text.clone();
            ed.open_merge(left, center, right, target, "Confronto e unione");
        });
    }

    /// Git → Risolvi il conflitto: the versions of the file in the index
    /// (common ancestor, ours, theirs) merged automatically.
    pub(super) fn resolve_conflict(self: &Rc<Self>, file: &Path) {
        let Some(st) = self.git_status() else {
            self.message("Nessun repository git");
            return;
        };
        let Ok(rel) = file.strip_prefix(&st.root) else { return };
        let rel = rel.to_string_lossy().into_owned();
        let (root, file) = (st.root.clone(), file.to_path_buf());
        let ed = self.clone();
        let stage = |n: u8| format!(":{n}:{rel}");
        let (s1, s2, s3) = (stage(1), stage(2), stage(3));
        git::run_str(&root.clone(), &["show", &s2], move |ours| {
            if !ours.ok {
                ed.message(&format!("{rel}: nell'indice manca la tua versione (non è in conflitto, o è stato eliminato nel tuo ramo: in quel caso tienilo con git add o toglilo con git rm)"));
                return;
            }
            let ed2 = ed.clone();
            git::run_str(&root.clone(), &["show", &s3], move |theirs| {
                if !theirs.ok {
                    ed2.message(&format!("{rel}: nell'indice manca la versione in arrivo (il file è stato eliminato nell'altro ramo: tienilo con git add o toglilo con git rm)"));
                    return;
                }
                let ed3 = ed2.clone();
                // The common ancestor is missing when both sides added the file.
                git::run_str(&root.clone(), &["show", &s1], move |base| {
                    let text = |o: &git::Output| String::from_utf8_lossy(&o.stdout).into_owned();
                    let (b, o, t) = (if base.ok { text(&base) } else { String::new() }, text(&ours), text(&theirs));
                    let merged = diff::merge3(&b, &o, &t);
                    let left = Side { text: o, title: format!("La tua versione (HEAD) — {rel}"), path: Some(file.clone()) };
                    let right = Side { text: t, title: format!("Versione in arrivo — {rel}"), path: Some(file.clone()) };
                    ed3.message(&match merged.conflicts {
                        0 => "Unione automatica senza conflitti: controlla il risultato e salva".to_string(),
                        1 => "1 conflitto da risolvere (in rosso)".to_string(),
                        n => format!("{n} conflitti da risolvere (in rosso)"),
                    });
                    ed3.open_merge(left, merged.text, right, Target::Git { root, rel: rel.clone(), file }, &format!("Risolvi il conflitto — {rel}"));
                });
            });
        });
    }

    pub(super) fn resolve_active_conflict(self: &Rc<Self>) {
        match self.active_doc().and_then(|d| d.path.borrow().clone()) {
            Some(p) => self.resolve_conflict(&p),
            None => self.message("Apri il file in conflitto"),
        }
    }
}

fn short(p: &Path) -> String {
    let s = p.display().to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && s.starts_with(&home) { format!("~{}", &s[home.len()..]) } else { s }
}

impl Merge {
    fn new(ed: &Rc<Editor>, left: Side, center: String, right: Side, target: Target, title: &str) -> Rc<Merge> {
        let win = gtk::Window::builder().title(title).transient_for(&ed.win).default_width(1400).default_height(820).build();
        let lang_of = |s: &Side| lang::detect(s.path.as_deref(), &s.text.chars().take(256).collect::<String>());
        let doc = |text: &str, l: lang::Lang, editable: bool| {
            let d = Doc::new(&ed.shared, config::new_id());
            d.view.set_editable(editable);
            d.apply_settings();
            d.set_lang(l);
            d.set_content(text, false, 0);
            d
        };
        let l = lang_of(&left);
        let panes = [doc(&left.text, l, false), doc(&center, l, true), doc(&right.text, lang_of(&right), false)];
        let links = [gtk::DrawingArea::new(), gtk::DrawingArea::new()];
        for a in &links {
            a.set_content_width(LINK_WIDTH);
            a.set_vexpand(true);
        }
        let center_title = match &target {
            Target::Git { .. } => "Risultato (modificabile)".to_string(),
            Target::Files { .. } => "Risultato (modificabile, all'inizio uguale al file 1)".to_string(),
        };
        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        columns.set_vexpand(true);
        columns.append(&pane_box(&format!("1 · {}", left.title), &panes[0]));
        columns.append(&links[0]);
        columns.append(&pane_box(&center_title, &panes[1]));
        columns.append(&links[1]);
        columns.append(&pane_box(&format!("2 · {}", right.title), &panes[2]));
        // The panes share the width equally.
        let group = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        let mut c = columns.first_child();
        while let Some(w) = c {
            if w.is::<gtk::Box>() {
                group.add_widget(&w);
            }
            c = w.next_sibling();
        }

        // Toolbar.
        let prev = gtk::Button::from_icon_name("go-up-symbolic");
        prev.set_tooltip_text(Some("Differenza precedente (Alt+↑)"));
        let next = gtk::Button::from_icon_name("go-down-symbolic");
        next.set_tooltip_text(Some("Differenza successiva (Alt+↓)"));
        let summary = gtk::Label::new(None);
        summary.set_margin_start(8);
        let hint = gtk::Label::new(Some("Freccia: sostituisce il blocco · Ctrl+clic: aggiunge dopo · Maiusc+clic: aggiunge prima"));
        hint.add_css_class("dim-label");
        hint.set_hexpand(true);
        hint.set_ellipsize(pango::EllipsizeMode::End);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.append(&prev);
        bar.append(&next);
        bar.append(&summary);
        bar.append(&hint);
        let mut save_buttons = Vec::new();
        match &target {
            Target::Git { .. } => {
                let b = gtk::Button::with_label("Salva e segna come risolto");
                b.add_css_class("suggested-action");
                b.set_tooltip_text(Some("Scrive il risultato nel file e lo aggiunge all'indice (git add) — Ctrl+S"));
                save_buttons.push((b, None));
            }
            Target::Files { left, right } => {
                for (n, p) in [(1, left), (2, right)] {
                    if let Some(p) = p {
                        let name = p.file_name().map_or_else(|| p.display().to_string(), |x| x.to_string_lossy().into_owned());
                        let b = gtk::Button::with_label(&format!("Salva in {n} ({name})"));
                        b.set_tooltip_text(Some(&format!("Scrive il risultato in {}{}", short(p), if n == 1 { " — Ctrl+S" } else { "" })));
                        save_buttons.push((b, Some(p.clone())));
                    }
                }
                save_buttons.push((gtk::Button::with_label("Salva come…"), None));
            }
        }
        for (b, _) in &save_buttons {
            bar.append(b);
        }

        let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(8).margin_bottom(8).margin_start(8).margin_end(8).build();
        body.append(&bar);
        body.append(&columns);
        win.set_child(Some(&body));

        let m = Rc::new(Merge {
            ed: Rc::downgrade(ed),
            win,
            panes,
            links,
            diffs: RefCell::new([DiffResult::default(), DiffResult::default()]),
            arrows: RefCell::new(Vec::new()),
            summary,
            target,
            pending: Cell::new(false),
            syncing: Cell::new(false),
        });
        m.connect(&prev, &next, save_buttons);
        m.recompute();
        m
    }

    fn connect(self: &Rc<Self>, prev: &gtk::Button, next: &gtk::Button, saves: Vec<(gtk::Button, Option<PathBuf>)>) {
        for (k, link) in self.links.iter().enumerate() {
            let w = Rc::downgrade(self);
            link.set_draw_func(move |_, cr, width, height| {
                if let Some(m) = w.upgrade() {
                    m.draw_link(k, cr, width as f64, height as f64);
                }
            });
            let click = gtk::GestureClick::new();
            click.set_button(1);
            let w = Rc::downgrade(self);
            click.connect_pressed(move |g, _, x, y| {
                let Some(m) = w.upgrade() else { return };
                let state = g.current_event_state();
                let mode = if state.contains(gdk::ModifierType::CONTROL_MASK) {
                    Apply::After
                } else if state.contains(gdk::ModifierType::SHIFT_MASK) {
                    Apply::Before
                } else {
                    Apply::Replace
                };
                m.click(k, x, y, mode);
            });
            link.add_controller(click);
            let motion = gtk::EventControllerMotion::new();
            let w = Rc::downgrade(self);
            motion.connect_motion(move |c, x, y| {
                if let Some(m) = w.upgrade()
                    && let Some(link) = c.widget()
                {
                    let over = m.arrow_at(k, x, y).is_some();
                    link.set_cursor_from_name(Some(if over { "pointer" } else { "default" }));
                }
            });
            link.add_controller(motion);
        }
        // Redraw while scrolling, keep the columns aligned.
        for (p, d) in self.panes.iter().enumerate() {
            let w = Rc::downgrade(self);
            d.scroller.vadjustment().connect_value_changed(move |_| {
                if let Some(m) = w.upgrade() {
                    m.sync_from(p);
                    m.redraw();
                }
            });
        }
        // The result changed: compute the differences again shortly.
        let w = Rc::downgrade(self);
        self.panes[1].buffer.connect_changed(move |_| {
            if let Some(m) = w.upgrade() {
                m.schedule_recompute();
            }
        });
        let w = Rc::downgrade(self);
        prev.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.jump(false);
            }
        });
        let w = Rc::downgrade(self);
        next.connect_clicked(move |_| {
            if let Some(m) = w.upgrade() {
                m.jump(true);
            }
        });
        let first_save = saves.first().map(|(b, _)| b.clone());
        for (b, path) in saves {
            let w = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(m) = w.upgrade() {
                    m.save(path.clone());
                }
            });
        }
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(m) = w.upgrade() else { return glib::Propagation::Proceed };
            if state.contains(gdk::ModifierType::ALT_MASK) && matches!(key, gdk::Key::Down | gdk::Key::Up) {
                m.jump(key == gdk::Key::Down);
                return glib::Propagation::Stop;
            }
            if state.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::s | gdk::Key::S) {
                if let Some(b) = &first_save {
                    b.emit_clicked();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.win.add_controller(keys);
        let w = Rc::downgrade(self);
        self.win.connect_close_request(move |win| {
            let Some(m) = w.upgrade() else { return glib::Propagation::Proceed };
            if !m.panes[1].buffer.is_modified() {
                m.forget();
                return glib::Propagation::Proceed;
            }
            let dialog = gtk::AlertDialog::builder()
                .modal(true)
                .message("Chiudere senza salvare il risultato?")
                .detail("Le scelte fatte in questa finestra andranno perse.")
                .buttons(["Annulla", "Chiudi senza salvare"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let w2 = Rc::downgrade(&m);
            dialog.choose(Some(win), None::<&gio::Cancellable>, move |res| {
                if res == Ok(1)
                    && let Some(m) = w2.upgrade()
                {
                    m.panes[1].buffer.set_modified(false);
                    m.win.close();
                }
            });
            glib::Propagation::Stop
        });
    }

    /// The result has changes not saved anywhere.
    pub(super) fn is_modified(&self) -> bool {
        self.panes[1].buffer.is_modified()
    }

    pub(super) fn discard(&self) {
        self.panes[1].buffer.set_modified(false);
    }

    /// The window is closed: the editor drops it.
    fn forget(self: &Rc<Self>) {
        if let Some(ed) = self.ed.upgrade() {
            ed.merges.borrow_mut().retain(|x| !Rc::ptr_eq(x, self));
        }
    }

    fn redraw(&self) {
        for l in &self.links {
            l.queue_draw();
        }
    }

    fn schedule_recompute(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::timeout_add_local_once(RECOMPUTE, move || {
            if let Some(m) = w.upgrade() {
                m.pending.set(false);
                m.recompute();
            }
        });
    }

    /// Compare both sides with the result and mark the blocks.
    fn recompute(&self) {
        let center = self.panes[1].text();
        let diffs = [diff::compute(&self.panes[0].text(), &center), diff::compute(&self.panes[2].text(), &center)];
        let tags = &self.panes[1].shared.tags;
        for d in &self.panes {
            let (a, b) = d.buffer.bounds();
            for t in tags.diff_tags() {
                d.buffer.remove_tag(t, &a, &b);
            }
        }
        let lines = |d: &Doc, r: &Range<usize>, tag: &gtk::TextTag| {
            let buf = &d.buffer;
            let Some(a) = buf.iter_at_line(r.start as i32) else { return };
            let b = buf.iter_at_line(r.end as i32).unwrap_or_else(|| buf.end_iter());
            buf.apply_tag(tag, &a, &b);
        };
        let words = |d: &Doc, spans: &[diff::Span], tag: &gtk::TextTag| {
            let buf = &d.buffer;
            for s in spans {
                if let (Some(mut a), Some(mut b)) = (buf.iter_at_line(s.line as i32), buf.iter_at_line(s.line as i32)) {
                    a.set_line_index(s.start as i32);
                    b.set_line_index(s.end as i32);
                    buf.apply_tag(tag, &a, &b);
                }
            }
        };
        let mut blocks = 0;
        for (k, side) in [(0, 0), (1, 2)] {
            let other = &diffs[1 - k];
            for h in &diffs[k].hunks {
                let conflict = other.hunks.iter().any(|o| touches(&o.new, &h.new));
                let side_tag = match (conflict, &h.kind) {
                    (true, _) => &tags.diff_conflict,
                    (false, HunkKind::Removed) => &tags.diff_add,
                    _ => &tags.diff_chg,
                };
                lines(&self.panes[side], &h.old, side_tag);
                lines(&self.panes[1], &h.new, if conflict { &tags.diff_conflict } else { &tags.diff_chg });
                blocks += 1;
            }
            words(&self.panes[side], &diffs[k].old_words, &tags.diff_word_add);
            words(&self.panes[1], &diffs[k].new_words, &tags.diff_word_del);
        }
        // A conflict is counted once, from the left side.
        let conflicts_left = diffs[0].hunks.iter().filter(|h| diffs[1].hunks.iter().any(|o| touches(&o.new, &h.new))).count();
        let diff_count = blocks - conflicts_left;
        self.summary.set_text(&match (diff_count, conflicts_left) {
            (0, _) => "Nessuna differenza".to_string(),
            (n, 0) => format!("{n} {}", if n == 1 { "differenza" } else { "differenze" }),
            (n, c) => format!("{n} differenze · {c} {}", if c == 1 { "conflitto" } else { "conflitti" }),
        });
        if conflicts_left > 0 {
            self.summary.add_css_class("error");
        } else {
            self.summary.remove_css_class("error");
        }
        *self.diffs.borrow_mut() = diffs;
        self.redraw();
    }

    /// Top and bottom of lines `r` of pane `p`, in the coordinates of `link`.
    fn range_y(&self, p: usize, r: &Range<usize>, link: &gtk::DrawingArea) -> (f64, f64) {
        let view = &self.panes[p].view;
        let buf = &self.panes[p].buffer;
        let y_of = |line: usize| -> i32 {
            match buf.iter_at_line(line as i32) {
                Some(it) if (line as i32) < buf.line_count() => view.line_yrange(&it).0,
                _ => {
                    let (y, h) = view.line_yrange(&buf.end_iter());
                    y + h
                }
            }
        };
        let top = y_of(r.start);
        let bottom = if r.is_empty() { top } else { y_of(r.end) };
        let conv = |y: i32| {
            let (_, wy) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, 0, y);
            view.compute_point(link, &graphene::Point::new(0.0, wy as f32)).map_or(wy as f64, |p| p.y() as f64)
        };
        (conv(top), conv(bottom))
    }

    fn draw_link(&self, k: usize, cr: &gtk::cairo::Context, w: f64, h: f64) {
        let dark = self.panes[1].shared.settings.borrow().dark;
        let (chg, add, conflict) = if dark {
            ((0.30, 0.45, 0.80), (0.30, 0.65, 0.40), (0.90, 0.45, 0.25))
        } else {
            ((0.35, 0.55, 0.95), (0.25, 0.70, 0.40), (0.95, 0.45, 0.20))
        };
        let link = &self.links[k];
        let diffs = self.diffs.borrow();
        let (side, d, other) = if k == 0 { (0, &diffs[0], &diffs[1]) } else { (2, &diffs[1], &diffs[0]) };
        let mut arrows = self.arrows.borrow_mut();
        arrows.retain(|a| a.0 != k);
        for (i, hk) in d.hunks.iter().enumerate() {
            let (s1, s2) = self.range_y(side, &hk.old, link);
            let (c1, c2) = self.range_y(1, &hk.new, link);
            if s2.max(c2) < 0.0 || s1.min(c1) > h {
                continue;
            }
            let color = if other.hunks.iter().any(|o| touches(&o.new, &hk.new)) {
                conflict
            } else if hk.kind == HunkKind::Removed {
                add
            } else {
                chg
            };
            // Left edge: the side (link 0) or the result (link 1).
            let ((ya1, ya2), (yb1, yb2)) = if k == 0 { ((s1, s2), (c1, c2)) } else { ((c1, c2), (s1, s2)) };
            let mid = w / 2.0;
            cr.move_to(0.0, ya1);
            cr.curve_to(mid, ya1, mid, yb1, w, yb1);
            cr.line_to(w, yb2.max(yb1 + 1.0));
            cr.curve_to(mid, yb2.max(yb1 + 1.0), mid, ya2.max(ya1 + 1.0), 0.0, ya2.max(ya1 + 1.0));
            cr.close_path();
            cr.set_source_rgba(color.0, color.1, color.2, 0.28);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(color.0, color.1, color.2, 0.75);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
            // The arrow, on the side of the block to copy, towards the result.
            let (x, ys) = if k == 0 { (ARROW + 3.0, (s1, s2)) } else { (w - ARROW - 3.0, (s1, s2)) };
            let y = ((ys.0 + ys.1) / 2.0).clamp(ARROW + 2.0, h - ARROW - 2.0);
            let dir = if k == 0 { 1.0 } else { -1.0 };
            cr.set_source_rgba(color.0 * 0.8, color.1 * 0.8, color.2 * 0.8, 1.0);
            cr.move_to(x - dir * ARROW * 0.7, y - ARROW);
            cr.line_to(x + dir * ARROW * 0.9, y);
            cr.line_to(x - dir * ARROW * 0.7, y + ARROW);
            cr.close_path();
            let _ = cr.fill();
            arrows.push((k, x, y, i));
        }
    }

    fn arrow_at(&self, k: usize, x: f64, y: f64) -> Option<usize> {
        self.arrows
            .borrow()
            .iter()
            .find(|a| a.0 == k && (a.1 - x).abs() <= ARROW + 4.0 && (a.2 - y).abs() <= ARROW + 4.0)
            .map(|a| a.3)
    }

    fn click(self: &Rc<Self>, k: usize, x: f64, y: f64, mode: Apply) {
        let Some(i) = self.arrow_at(k, x, y) else { return };
        let Some(h) = self.diffs.borrow()[k].hunks.get(i).cloned() else { return };
        let side = if k == 0 { 0 } else { 2 };
        self.copy_block(side, &h.old, &h.new, mode);
    }

    /// Copy lines `from` of pane `side` into the result at lines `to`.
    fn copy_block(self: &Rc<Self>, side: usize, from: &Range<usize>, to: &Range<usize>, mode: Apply) {
        let src = &self.panes[side].buffer;
        let a = src.iter_at_line(from.start as i32).unwrap_or_else(|| src.end_iter());
        let b = src.iter_at_line(from.end as i32).filter(|_| (from.end as i32) < src.line_count()).unwrap_or_else(|| src.end_iter());
        let mut text = src.text(&a, &b, true).to_string();
        let buf = &self.panes[1].buffer;
        let line_iter = |l: usize| buf.iter_at_line(l as i32).filter(|_| (l as i32) < buf.line_count()).unwrap_or_else(|| buf.end_iter());
        let (mut start, mut end) = (line_iter(to.start), line_iter(to.end));
        match mode {
            Apply::Replace => {}
            Apply::After => start = end,
            Apply::Before => end = start,
        }
        // Whole lines: a block taken from the end of a file without a final
        // newline still ends its line, a block added after the last line
        // starts a new one.
        if !text.is_empty() && !text.ends_with('\n') && !end.is_end() {
            text.push('\n');
        }
        if start.is_end() && !start.starts_line() && !text.is_empty() {
            text.insert(0, '\n');
        }
        let offset = start.offset();
        buf.begin_user_action();
        buf.delete(&mut start, &mut end);
        buf.insert(&mut start, &text);
        buf.end_user_action();
        buf.place_cursor(&buf.iter_at_offset(offset));
        self.recompute();
    }

    /// Keep the other columns aligned with pane `p` that scrolled.
    fn sync_from(&self, p: usize) {
        if self.syncing.replace(true) {
            return;
        }
        let diffs = self.diffs.borrow();
        let top_line = |i: usize| {
            let d = &self.panes[i];
            let y = d.scroller.vadjustment().value() as i32;
            let (it, ly) = d.view.line_at_y(y.max(0));
            let lh = d.view.line_yrange(&it).1.max(1);
            (it.line() as usize, (y - ly) as f64 / lh as f64)
        };
        let set = |i: usize, line: usize, frac: f64| {
            let d = &self.panes[i];
            let max = (d.buffer.line_count() - 1).max(0) as usize;
            if let Some(it) = d.buffer.iter_at_line(line.min(max) as i32) {
                let (y, h) = d.view.line_yrange(&it);
                d.scroller.vadjustment().set_value(y as f64 + frac * h as f64);
            }
        };
        let (line, frac) = top_line(p);
        // To the result, then from the result to the other side.
        let center = match p {
            0 => diffs[0].map_line(line, true),
            2 => diffs[1].map_line(line, true),
            _ => line,
        };
        if p != 1 {
            set(1, center, frac);
        }
        if p != 0 {
            set(0, diffs[0].map_line(center, false), frac);
        }
        if p != 2 {
            set(2, diffs[1].map_line(center, false), frac);
        }
        drop(diffs);
        self.syncing.set(false);
    }

    /// Move the cursor of the result to the next/previous block.
    fn jump(&self, forward: bool) {
        let diffs = self.diffs.borrow();
        let mut starts: Vec<usize> = diffs.iter().flat_map(|d| d.hunks.iter().map(|h| h.new.start)).collect();
        starts.sort_unstable();
        starts.dedup();
        let buf = &self.panes[1].buffer;
        let cur = buf.iter_at_mark(&buf.get_insert()).line() as usize;
        let target = if forward { starts.iter().find(|&&s| s > cur) } else { starts.iter().rev().find(|&&s| s < cur) };
        let Some(&line) = target.or(if forward { starts.first() } else { starts.last() }) else {
            if let Some(ed) = self.ed.upgrade() {
                ed.message("Nessuna differenza");
            }
            return;
        };
        drop(diffs);
        if let Some(it) = buf.iter_at_line(line as i32) {
            buf.place_cursor(&it);
            self.panes[1].view.scroll_to_mark(&buf.get_insert(), 0.1, true, 0.0, 0.3);
            self.panes[1].view.grab_focus();
        }
    }

    fn conflicts(&self) -> usize {
        let d = self.diffs.borrow();
        d[0].hunks.iter().filter(|h| d[1].hunks.iter().any(|o| touches(&o.new, &h.new))).count()
    }

    /// Write the result: into `path`, or as git resolution, or "save as".
    fn save(self: &Rc<Self>, path: Option<PathBuf>) {
        self.recompute();
        let Some(ed) = self.ed.upgrade() else { return };
        let text = self.panes[1].text();
        match (&self.target, path) {
            (Target::Git { root, rel, file }, _) => {
                let go = {
                    let (m, ed, root, rel, file, text) = (self.clone(), ed.clone(), root.clone(), rel.clone(), file.clone(), text.clone());
                    move || {
                        if !ed.write_text_to(&file, &text) {
                            return;
                        }
                        let (m2, ed2, rel2) = (m.clone(), ed.clone(), rel.clone());
                        git::run_str(&root, &["add", "--", &rel], move |out| {
                            if out.ok {
                                ed2.message(&format!("{rel2}: conflitto risolto e aggiunto all'indice (git add)"));
                                m2.panes[1].buffer.set_modified(false);
                                m2.win.close();
                            } else {
                                ed2.error_dialog("git add non riuscito", &out.text());
                            }
                            ed2.git_refresh();
                        });
                    }
                };
                let n = self.conflicts();
                if n == 0 {
                    go();
                    return;
                }
                let dialog = gtk::AlertDialog::builder()
                    .modal(true)
                    .message(format!("Restano {n} conflitti"))
                    .detail("Le parti in rosso non sono state scelte: nel file resterà il testo della versione comune. Segnare comunque il file come risolto?")
                    .buttons(["Annulla", "Salva comunque"])
                    .cancel_button(0)
                    .default_button(0)
                    .build();
                dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| {
                    if res == Ok(1) {
                        go();
                    }
                });
            }
            (Target::Files { .. }, Some(p)) => {
                if ed.write_text_to(&p, &text) {
                    self.panes[1].buffer.set_modified(false);
                    ed.message(&format!("Risultato salvato in {}", short(&p)));
                }
            }
            (Target::Files { left, .. }, None) => {
                let dialog = gtk::FileDialog::builder().title("Salva il risultato come").modal(true).build();
                if let Some(p) = left {
                    dialog.set_initial_file(Some(&gio::File::for_path(p)));
                }
                let (m, ed2) = (self.clone(), ed.clone());
                dialog.save(Some(&self.win), None::<&gio::Cancellable>, move |res| {
                    if let Some(p) = res.ok().and_then(|f| f.path())
                        && ed2.write_text_to(&p, &text)
                    {
                        m.panes[1].buffer.set_modified(false);
                        ed2.message(&format!("Risultato salvato in {}", short(&p)));
                    }
                });
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Apply {
    Replace,
    After,
    Before,
}

impl Editor {
    /// Write `text` into `path` through its tab (opened if needed), so that
    /// the editor, its undo history and the change monitor stay in step.
    pub(super) fn write_text_to(self: &Rc<Self>, path: &Path, text: &str) -> bool {
        self.open_path(path);
        let Some(d) = self.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path)).cloned() else { return false };
        if let Err(e) = d.ensure_loaded() {
            self.error_dialog("Impossibile salvare", &e);
            return false;
        }
        if d.text() != text {
            d.replace_all_text(text);
        }
        self.write_doc(&d, path)
    }
}

#[cfg(debug_assertions)]
impl Merge {
    /// Click the arrow of block `i` of link `k`.
    pub(super) fn debug_take(self: &Rc<Self>, k: usize, i: usize, after: bool) {
        let Some(h) = self.diffs.borrow()[k].hunks.get(i).cloned() else { return };
        self.copy_block(if k == 0 { 0 } else { 2 }, &h.old, &h.new, if after { Apply::After } else { Apply::Replace });
    }

    pub(super) fn debug_dump(&self) {
        let d = self.diffs.borrow();
        eprintln!("MERGE summary={:?} left={} right={} conflicts={}", self.summary.text(), d[0].hunks.len(), d[1].hunks.len(), self.conflicts());
        eprintln!("RESULT {:?}", self.panes[1].text());
    }

    pub(super) fn debug_save(self: &Rc<Self>, n: usize) {
        let path = match &self.target {
            Target::Files { left, right } => if n == 2 { right.clone() } else { left.clone() },
            Target::Git { .. } => None,
        };
        self.save(path);
    }
}
