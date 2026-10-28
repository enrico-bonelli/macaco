//! A document: one tab with its buffer, view, line number gutter,
//! incremental syntax highlighter and memory/cache management.

use crate::config::{self, CacheMeta, Settings};
use crate::encoding::{self, Encoding, Eol};
use crate::fold;
use crate::lang::Lang;
use crate::lexer::{self, KINDS, State};
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant, SystemTime};

/// Documents larger than this are not highlighted.
const MAX_HIGHLIGHT_BYTES: i32 = 8 * 1024 * 1024;
/// Time budget of one idle highlighting step.
const HL_BUDGET: Duration = Duration::from_millis(6);
const GUTTER_PAD: i32 = 8;

/// Text tags shared by every buffer (one tag table for all documents).
pub struct Tags {
    pub table: gtk::TextTagTable,
    pub syntax: Vec<gtk::TextTag>,
    pub curline: gtk::TextTag,
    pub bracket: gtk::TextTag,
    pub search: gtk::TextTag,
    /// Folding: the hidden lines of a collapsed block and its first line.
    pub fold: gtk::TextTag,
    pub fold_head: gtk::TextTag,
    /// Problems found by a language server ("Verifica il file").
    pub diag_error: gtk::TextTag,
    pub diag_warning: gtk::TextTag,
    /// Diff view: whole lines added / removed / changed.
    pub diff_add: gtk::TextTag,
    pub diff_del: gtk::TextTag,
    pub diff_chg: gtk::TextTag,
    /// Merge: lines changed differently on both sides.
    pub diff_conflict: gtk::TextTag,
    /// Diff view: changed words in the left / right document.
    pub diff_word_del: gtk::TextTag,
    pub diff_word_add: gtk::TextTag,
}

impl Tags {
    pub fn new(dark: bool) -> Tags {
        let table = gtk::TextTagTable::new();
        // Creation order defines priority: current line < syntax < matches.
        let curline = gtk::TextTag::new(Some("curline"));
        table.add(&curline);
        let mk = |name: &str| {
            let t = gtk::TextTag::new(Some(name));
            table.add(&t);
            t
        };
        let (diff_add, diff_del, diff_chg) = (mk("diff-add"), mk("diff-del"), mk("diff-chg"));
        let diff_conflict = mk("diff-conflict");
        let (diff_word_del, diff_word_add) = (mk("diff-word-del"), mk("diff-word-add"));
        let syntax: Vec<gtk::TextTag> = KINDS
            .iter()
            .map(|k| {
                let t = gtk::TextTag::new(Some(&format!("{k:?}")));
                table.add(&t);
                t
            })
            .collect();
        let bracket = gtk::TextTag::builder().name("bracket").weight(700).build();
        table.add(&bracket);
        let search = gtk::TextTag::new(Some("search"));
        table.add(&search);
        let fold = gtk::TextTag::builder().name("fold").invisible(true).build();
        table.add(&fold);
        let fold_head = gtk::TextTag::new(Some("fold-head"));
        table.add(&fold_head);
        let diag_warning = gtk::TextTag::builder().name("diag-warning").underline(pango::Underline::Error).build();
        table.add(&diag_warning);
        let diag_error = gtk::TextTag::builder().name("diag-error").underline(pango::Underline::Error).build();
        table.add(&diag_error);
        let tags = Tags { table, syntax, curline, bracket, search, fold, fold_head, diag_error, diag_warning, diff_add, diff_del, diff_chg, diff_conflict, diff_word_del, diff_word_add };
        tags.set_palette(dark);
        tags
    }

    pub fn diag_tags(&self) -> [&gtk::TextTag; 2] {
        [&self.diag_error, &self.diag_warning]
    }

    pub fn diff_tags(&self) -> [&gtk::TextTag; 6] {
        [&self.diff_add, &self.diff_del, &self.diff_chg, &self.diff_conflict, &self.diff_word_del, &self.diff_word_add]
    }

    pub fn set_palette(&self, dark: bool) {
        // One Light / One Dark inspired palettes, in `KINDS` order.
        let colors: [&str; 11] = if dark {
            ["#c678dd", "#e5c07b", "#d19a66", "#d19a66", "#98c379", "#7f848e", "#56b6c2", "#61afef", "#e06c75", "#e06c75", "#d19a66"]
        } else {
            ["#a626a4", "#b07a00", "#986801", "#986801", "#50a14f", "#a0a1a7", "#0184bc", "#4078f2", "#e45649", "#e45649", "#986801"]
        };
        for (t, c) in self.syntax.iter().zip(colors) {
            t.set_foreground(Some(c));
        }
        use lexer::Kind;
        let tag = |k: Kind| &self.syntax[k as usize];
        tag(Kind::Comment).set_style(pango::Style::Italic);
        tag(Kind::Keyword).set_weight(600);
        // Markdown styles.
        tag(Kind::Heading).set_foreground(Some(if dark { "#e06c75" } else { "#c0392b" }));
        tag(Kind::Heading).set_weight(700);
        tag(Kind::Strong).set_weight(700);
        tag(Kind::Emph).set_style(pango::Style::Italic);
        tag(Kind::Link).set_foreground(Some(if dark { "#61afef" } else { "#2f6fdd" }));
        tag(Kind::Link).set_underline(pango::Underline::Single);
        self.curline.set_paragraph_background(Some(if dark { "#2c313a" } else { "#f2f4f8" }));
        self.bracket.set_background(Some(if dark { "#3e4451" } else { "#d8e0ec" }));
        self.search.set_background(Some(if dark { "#7a6a1a" } else { "#fff176" }));
        let rgba = |c: &str| gdk::RGBA::parse(c).ok();
        self.diag_error.set_underline_rgba(rgba(if dark { "#ff6b6b" } else { "#d32f2f" }).as_ref());
        self.diag_error.set_background_rgba(rgba(if dark { "rgba(255,107,107,0.18)" } else { "rgba(211,47,47,0.12)" }).as_ref());
        self.diag_warning.set_underline_rgba(rgba(if dark { "#e5c07b" } else { "#c98a00" }).as_ref());
        self.diag_warning.set_background_rgba(rgba(if dark { "rgba(229,192,123,0.16)" } else { "rgba(201,138,0,0.12)" }).as_ref());
        self.fold_head.set_paragraph_background(Some(if dark { "#273349" } else { "#e5ecf9" }));
        let [add, del, chg, wdel, wadd] = if dark {
            ["#1f3a2a", "#45262b", "#2b3550", "#7a2f3a", "#2f6b45"]
        } else {
            ["#e6ffec", "#ffebe9", "#eef3ff", "#ffb3ba", "#a8eebb"]
        };
        self.diff_add.set_paragraph_background(Some(add));
        self.diff_del.set_paragraph_background(Some(del));
        self.diff_chg.set_paragraph_background(Some(chg));
        self.diff_conflict.set_paragraph_background(Some(if dark { "#4a2a1c" } else { "#ffe1cc" }));
        self.diff_word_del.set_background(Some(wdel));
        self.diff_word_add.set_background(Some(wadd));
    }
}

/// State shared by all documents.
pub struct Shared {
    pub settings: RefCell<Settings>,
    pub tags: Tags,
    pub font: RefCell<pango::FontDescription>,
    /// Width in pixels of a digit in the editor font (for the gutter).
    pub digit_width: Cell<i32>,
}

impl Shared {
    pub fn indent_unit(&self, lang: Lang) -> String {
        let s = self.settings.borrow();
        if s.insert_spaces && !lang.prefers_tabs() { " ".repeat(s.tab_width as usize) } else { "\t".into() }
    }
}

const NO_DIRTY: usize = usize::MAX;

/// The file of a document changed on disk (by another program) since it
/// was read or saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskChange {
    None,
    Modified,
    Deleted,
}

/// What identifies a version of a file on disk.
fn disk_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// A collapsed block: the lines from `start` (included) to `end` (excluded)
/// are hidden below the header line starting at `head`.
struct Fold {
    head: gtk::TextMark,
    start: gtk::TextMark,
    end: gtk::TextMark,
}

/// Foldable blocks computed for one edit generation and language.
type FoldCache = Option<(u64, Lang, Rc<Vec<fold::Range>>)>;

/// Whether the cursor at `it` would be inside the hidden lines `start..end`.
fn hides(start: &gtk::TextIter, end: &gtk::TextIter, it: &gtk::TextIter) -> bool {
    it >= start && (it < end || (it == end && it.is_end() && !it.starts_line()))
}

/// Apply `tag` to `a..b` unless it already covers the whole range.
fn ensure_tag(buf: &gtk::TextBuffer, tag: &gtk::TextTag, a: &gtk::TextIter, b: &gtk::TextIter) {
    if a >= b {
        return;
    }
    let mut t = *a;
    if !(a.has_tag(tag) && (!t.forward_to_tag_toggle(Some(tag)) || t >= *b)) {
        buf.apply_tag(tag, a, b);
    }
}

/// Incremental highlighter bookkeeping: lexer state at the end of every
/// line plus the range of lines that must be re-lexed.
struct Highlighter {
    states: Vec<State>,
    lo: usize,
    hi: usize,
    scheduled: bool,
    enabled: bool,
}

impl Highlighter {
    fn mark(&mut self, lo: usize, hi: usize) {
        if self.lo == NO_DIRTY {
            self.lo = lo;
            self.hi = hi;
        } else {
            self.lo = self.lo.min(lo);
            self.hi = self.hi.max(hi);
        }
    }

    fn on_insert(&mut self, line: usize, added: usize) {
        if added > 0 {
            let at = (line + 1).min(self.states.len());
            self.states.splice(at..at, std::iter::repeat_n(State::Normal, added));
            if self.lo != NO_DIRTY && self.hi > line {
                self.hi += added;
            }
        }
        self.mark(line, line + added);
    }

    fn on_delete(&mut self, line: usize, removed: usize) {
        if removed > 0 {
            let a = (line + 1).min(self.states.len());
            let b = (line + 1 + removed).min(self.states.len());
            self.states.drain(a..b);
            if self.lo != NO_DIRTY && self.hi > line {
                self.hi = self.hi.saturating_sub(removed).max(line);
            }
        }
        self.mark(line, line);
    }
}

pub struct Doc {
    pub id: RefCell<String>,
    pub path: RefCell<Option<PathBuf>>,
    pub encoding: Cell<Encoding>,
    encoding_known: Cell<bool>,
    pub eol: Cell<Eol>,
    pub lang: Cell<Lang>,
    pub buffer: gtk::TextBuffer,
    pub view: gtk::TextView,
    pub gutter: gtk::DrawingArea,
    pub scroller: gtk::ScrolledWindow,
    pub tab: gtk::Box,
    pub label: gtk::Label,
    pub close_button: gtk::Button,
    pub shared: Rc<Shared>,
    loaded: Cell<bool>,
    /// Set while the buffer is filled/emptied programmatically.
    loading: Cell<bool>,
    unloaded_modified: Cell<bool>,
    unloaded_nonempty: Cell<bool>,
    saved_cursor: Cell<i32>,
    edit_gen: Cell<u64>,
    cache_gen: Cell<u64>,
    pub hidden_since: Cell<Option<Instant>>,
    /// Called first for every key press (completion popup); returns true
    /// if it handled the key.
    #[allow(clippy::type_complexity)]
    pub key_hook: RefCell<Option<Box<dyn Fn(gdk::Key, gdk::ModifierType) -> bool>>>,
    title: RefCell<String>,
    label_budget: Cell<usize>,
    /// Folders shown after the name when other tabs have the same name.
    label_context: RefCell<String>,
    hl: RefCell<Highlighter>,
    bracket_on: Cell<bool>,
    /// Collapsed blocks.
    folds: RefCell<Vec<Fold>>,
    fold_cache: RefCell<FoldCache>,
    fold_pending: Cell<bool>,
    /// Header lines of the collapsed blocks while the document is unloaded.
    saved_folds: RefCell<Vec<u32>>,
    gutter_hover: Cell<bool>,
    /// Times the content was (re)loaded.
    loads: Cell<u32>,
    /// Modification time and size of the file when it was read or saved.
    disk_stamp: Cell<Option<(SystemTime, u64)>>,
    pub disk_change: Cell<DiskChange>,
    /// Called when the file may have changed on disk; while it is set, a
    /// monitor watches the file of the loaded document.
    #[allow(clippy::type_complexity)]
    disk_handler: RefCell<Option<Rc<dyn Fn()>>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

fn count_digits(mut n: i32) -> i32 {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Debug builds: documents alive and weak references to their buffers, to
/// check that closed documents are freed.
#[cfg(debug_assertions)]
pub mod live {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static DOCS: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        pub static BUFFERS: RefCell<Vec<gtk::glib::WeakRef<gtk::TextBuffer>>> = const { RefCell::new(Vec::new()) };
    }
    /// (documents, buffers) still alive.
    pub fn counts() -> (usize, usize) {
        let buffers = BUFFERS.with(|b| {
            let mut b = b.borrow_mut();
            b.retain(|w| w.upgrade().is_some());
            b.len()
        });
        (DOCS.load(Ordering::Relaxed), buffers)
    }
}

#[cfg(debug_assertions)]
impl Drop for Doc {
    fn drop(&mut self) {
        live::DOCS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Doc {
    pub fn new(shared: &Rc<Shared>, id: String) -> Rc<Doc> {
        let buffer = gtk::TextBuffer::new(Some(&shared.tags.table));
        #[cfg(debug_assertions)]
        {
            use gtk::prelude::ObjectExt;
            live::DOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            live::BUFFERS.with(|b| b.borrow_mut().push(buffer.downgrade()));
        }
        let view = gtk::TextView::with_buffer(&buffer);
        view.add_css_class("macaco");
        view.set_monospace(true);
        view.set_left_margin(6);
        view.set_right_margin(6);
        view.set_top_margin(2);
        view.set_bottom_margin(40);
        view.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::NO_EMOJI);
        let gutter = gtk::DrawingArea::new();
        view.set_gutter(gtk::TextWindowType::Left, Some(&gutter));
        let scroller = gtk::ScrolledWindow::builder().child(&view).hexpand(true).vexpand(true).build();

        let label = gtk::Label::new(None);
        let close_button = gtk::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.add_css_class("tab-close");
        close_button.set_focus_on_click(false);
        let tab = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tab.append(&label);
        tab.append(&close_button);

        let doc = Rc::new(Doc {
            id: RefCell::new(id),
            path: RefCell::new(None),
            encoding: Cell::new(Encoding::Utf8),
            encoding_known: Cell::new(true),
            eol: Cell::new(Eol::Lf),
            lang: Cell::new(Lang::Plain),
            buffer,
            view,
            gutter,
            scroller,
            tab,
            label,
            close_button,
            shared: shared.clone(),
            loaded: Cell::new(true),
            loading: Cell::new(false),
            unloaded_modified: Cell::new(false),
            unloaded_nonempty: Cell::new(false),
            saved_cursor: Cell::new(0),
            edit_gen: Cell::new(0),
            cache_gen: Cell::new(0),
            hidden_since: Cell::new(None),
            key_hook: RefCell::new(None),
            title: RefCell::new(String::new()),
            label_budget: Cell::new(10),
            label_context: RefCell::new(String::new()),
            hl: RefCell::new(Highlighter { states: vec![State::Normal], lo: NO_DIRTY, hi: 0, scheduled: false, enabled: true }),
            bracket_on: Cell::new(false),
            folds: RefCell::new(Vec::new()),
            fold_cache: RefCell::new(None),
            fold_pending: Cell::new(false),
            saved_folds: RefCell::new(Vec::new()),
            gutter_hover: Cell::new(false),
            loads: Cell::new(0),
            disk_stamp: Cell::new(None),
            disk_change: Cell::new(DiskChange::None),
            disk_handler: RefCell::new(None),
            monitor: RefCell::new(None),
        });
        doc.connect_signals();
        doc.apply_settings();
        doc.refresh_title();
        doc
    }

    fn connect_signals(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.buffer.connect_insert_text(move |_, iter, text| {
            if let Some(d) = w.upgrade()
                && !d.loading.get() {
                    let added = text.bytes().filter(|&c| c == b'\n').count();
                    d.hl.borrow_mut().on_insert(iter.line() as usize, added);
                }
        });
        let w = Rc::downgrade(self);
        self.buffer.connect_delete_range(move |_, start, end| {
            if let Some(d) = w.upgrade()
                && !d.loading.get() {
                    let (a, b) = (start.line(), end.line());
                    d.hl.borrow_mut().on_delete(a as usize, (b - a) as usize);
                }
        });
        let w = Rc::downgrade(self);
        self.buffer.connect_changed(move |_| {
            if let Some(d) = w.upgrade()
                && !d.loading.get() {
                    d.edit_gen.set(d.edit_gen.get() + 1);
                    d.check_folds();
                    d.schedule_highlight();
                    d.update_gutter_width();
                    d.gutter.queue_draw();
                    if d.path.borrow().is_none() {
                        d.refresh_title();
                    }
                }
        });
        let w = Rc::downgrade(self);
        self.buffer.connect_modified_changed(move |_| {
            if let Some(d) = w.upgrade()
                && !d.loading.get() {
                    d.update_label();
                }
        });
        let w = Rc::downgrade(self);
        self.buffer.connect_cursor_position_notify(move |_| {
            if let Some(d) = w.upgrade()
                && !d.loading.get() {
                    d.reveal_cursor();
                    d.update_cursor_decorations();
                    d.gutter.queue_draw();
                }
        });

        // Line numbers.
        let w = Rc::downgrade(self);
        self.gutter.set_draw_func(move |da, cr, width, height| {
            if let Some(d) = w.upgrade() {
                d.draw_gutter(da, cr, width, height);
            }
        });
        // Fold handles: click to collapse or expand, shown on hover.
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let w = Rc::downgrade(self);
        click.connect_pressed(move |g, _, _, y| {
            if let Some(d) = w.upgrade()
                && let Some(line) = d.gutter_line_at(y)
                && d.toggle_fold_line(line)
            {
                g.set_state(gtk::EventSequenceState::Claimed);
            }
        });
        self.gutter.add_controller(click);
        let motion = gtk::EventControllerMotion::new();
        let w = Rc::downgrade(self);
        motion.connect_enter(move |_, _, y| {
            if let Some(d) = w.upgrade() {
                d.gutter_hover.set(true);
                d.update_gutter_pointer(y);
                d.gutter.queue_draw();
            }
        });
        let w = Rc::downgrade(self);
        motion.connect_motion(move |_, _, y| {
            if let Some(d) = w.upgrade() {
                d.update_gutter_pointer(y);
            }
        });
        let w = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(d) = w.upgrade() {
                d.gutter_hover.set(false);
                d.gutter.queue_draw();
            }
        });
        self.gutter.add_controller(motion);

        let vadj = self.scroller.vadjustment();
        let g = self.gutter.downgrade();
        vadj.connect_value_changed(move |_| {
            if let Some(g) = g.upgrade() {
                g.queue_draw();
            }
        });
        let g = self.gutter.downgrade();
        vadj.connect_changed(move |_| {
            if let Some(g) = g.upgrade() {
                g.queue_draw();
            }
        });

        // Editing keys: auto-indent, tab/shift-tab, smart backspace.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(d) = w.upgrade() else { return glib::Propagation::Proceed };
            let hooked = d.key_hook.borrow().as_ref().is_some_and(|h| h(key, state));
            if hooked || d.handle_key(key, state) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        self.view.add_controller(keys);
    }

    // ------------------------------------------------------------ settings

    pub fn apply_settings(&self) {
        let s = self.shared.settings.borrow();
        self.view.set_wrap_mode(if s.wrap { gtk::WrapMode::WordChar } else { gtk::WrapMode::None });
        let font = self.shared.font.borrow();
        let ctx = self.view.pango_context();
        let layout = pango::Layout::new(&ctx);
        layout.set_font_description(Some(&font));
        layout.set_text(&" ".repeat(s.tab_width as usize));
        let mut tabs = pango::TabArray::new(1, true);
        tabs.set_tab(0, pango::TabAlign::Left, layout.pixel_size().0);
        self.view.set_tabs(&tabs);
        if !s.highlight_line {
            let (a, b) = self.buffer.bounds();
            self.buffer.remove_tag(&self.shared.tags.curline, &a, &b);
        }
        drop((s, font));
        self.update_gutter_width();
        self.update_cursor_decorations();
        self.gutter.queue_draw();
    }

    /// Width of the column of the fold handles (0 if the language cannot
    /// be folded).
    fn fold_width(&self) -> i32 {
        if fold::supported(self.lang.get()) { self.shared.digit_width.get() + 8 } else { 0 }
    }

    fn update_gutter_width(&self) {
        let numbers = self.shared.settings.borrow().line_numbers;
        let fw = self.fold_width();
        self.gutter.set_visible(numbers || fw > 0);
        let w = if numbers {
            let digits = count_digits(self.buffer.line_count()).max(2);
            digits * self.shared.digit_width.get() + 2 * GUTTER_PAD + fw
        } else {
            fw + GUTTER_PAD / 2
        };
        if self.gutter.content_width() != w {
            self.gutter.set_content_width(w);
        }
    }

    fn draw_gutter(self: &Rc<Self>, da: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let (dark, numbers) = {
            let s = self.shared.settings.borrow();
            (s.dark, s.line_numbers)
        };
        let (bg, fg, cur, accent) = if dark {
            ((0.15, 0.16, 0.18), (0.45, 0.47, 0.5), (0.85, 0.86, 0.88), (0.38, 0.69, 0.94))
        } else {
            ((0.96, 0.96, 0.97), (0.6, 0.62, 0.65), (0.15, 0.15, 0.2), (0.16, 0.42, 0.85))
        };
        cr.set_source_rgb(bg.0, bg.1, bg.2);
        cr.rectangle(0.0, 0.0, width as f64, height as f64);
        let _ = cr.fill();
        if !self.loaded.get() {
            return;
        }
        let layout = da.create_pango_layout(None);
        layout.set_font_description(Some(&self.shared.font.borrow()));
        let rect = self.view.visible_rect();
        // The visible rect starts above line 0 by the top margin.
        let (mut iter, _) = self.view.line_at_y(rect.y().max(0));
        let cur_line = self.buffer.iter_at_mark(&self.buffer.get_insert()).line();
        let bottom = rect.y() + rect.height();
        let fw = self.fold_width();
        // Collapsed blocks: (header line, first visible position after them).
        let folded: Vec<(i32, gtk::TextIter)> = self
            .folds
            .borrow()
            .iter()
            .map(|f| (self.buffer.iter_at_mark(&f.head).line(), self.buffer.iter_at_mark(&f.end)))
            .collect();
        let foldable = if fw > 0 && self.gutter_hover.get() {
            let r = self.cached_fold_ranges();
            if r.is_none() {
                self.schedule_fold_ranges();
            }
            r
        } else {
            None
        };
        let half = (self.shared.digit_width.get() as f64 * 0.42).max(3.0);
        let mut num = String::with_capacity(12);
        loop {
            let (y, h) = self.view.line_yrange(&iter);
            if y > bottom {
                break;
            }
            let (_, wy) = self.view.buffer_to_window_coords(gtk::TextWindowType::Left, 0, y);
            let line = iter.line();
            if numbers {
                num.clear();
                use std::fmt::Write;
                let _ = write!(num, "{}", line + 1);
                layout.set_text(&num);
                let tw = layout.pixel_size().0;
                let c = if line == cur_line { cur } else { fg };
                cr.set_source_rgb(c.0, c.1, c.2);
                cr.move_to((width - GUTTER_PAD - fw - tw) as f64, wy as f64);
                pangocairo::functions::show_layout(cr, &layout);
            }
            let fold_end = folded.iter().filter(|f| f.0 == line).map(|f| f.1).max_by_key(|e| e.offset());
            if fw > 0 {
                let (cx, cy) = ((width - fw / 2 - 2) as f64, wy as f64 + h as f64 / 2.0);
                if fold_end.is_some() {
                    // Collapsed: a triangle pointing right.
                    cr.set_source_rgb(accent.0, accent.1, accent.2);
                    cr.move_to(cx - half * 0.6, cy - half);
                    cr.line_to(cx + half * 0.8, cy);
                    cr.line_to(cx - half * 0.6, cy + half);
                    cr.close_path();
                    let _ = cr.fill();
                } else if foldable.as_ref().is_some_and(|r| r.binary_search_by_key(&(line as u32), |r| r.header).is_ok()) {
                    // Expanded: a triangle pointing down.
                    cr.set_source_rgb(fg.0, fg.1, fg.2);
                    cr.move_to(cx - half, cy - half * 0.6);
                    cr.line_to(cx + half, cy - half * 0.6);
                    cr.line_to(cx, cy + half * 0.8);
                    cr.close_path();
                    let _ = cr.fill();
                }
            }
            // Skip the hidden lines.
            if let Some(end) = fold_end
                && end.line() > line
            {
                iter = end;
                if iter.is_end() && !iter.starts_line() {
                    break;
                }
                continue;
            }
            if !iter.forward_line() {
                break;
            }
        }
    }

    /// Line whose row in the gutter contains the window coordinate `y`.
    fn gutter_line_at(&self, y: f64) -> Option<i32> {
        if !self.loaded.get() {
            return None;
        }
        let (_, by) = self.view.window_to_buffer_coords(gtk::TextWindowType::Left, 0, y as i32);
        let (it, top) = self.view.line_at_y(by);
        let (_, h) = self.view.line_yrange(&it);
        (by >= top && by < top + h).then(|| it.line())
    }

    fn update_gutter_pointer(&self, y: f64) {
        let handle = self.gutter_line_at(y).is_some_and(|line| {
            self.fold_index_at_line(line).is_some()
                || self.cached_fold_ranges().is_some_and(|r| r.binary_search_by_key(&(line as u32), |r| r.header).is_ok())
        });
        self.gutter.set_cursor_from_name(Some(if handle { "pointer" } else { "default" }));
    }

    // ------------------------------------------------------------- folding

    /// Foldable blocks of the current text.
    pub fn fold_ranges(&self) -> Rc<Vec<fold::Range>> {
        if let Some(r) = self.cached_fold_ranges() {
            return r;
        }
        let lang = self.lang.get();
        let r = if fold::supported(lang) && self.loaded.get() && self.buffer.char_count() < MAX_HIGHLIGHT_BYTES {
            fold::ranges(lang, &self.text())
        } else {
            Vec::new()
        };
        let r = Rc::new(r);
        *self.fold_cache.borrow_mut() = Some((self.edit_gen.get(), lang, r.clone()));
        r
    }

    /// The foldable blocks if they are up to date with the text.
    fn cached_fold_ranges(&self) -> Option<Rc<Vec<fold::Range>>> {
        match &*self.fold_cache.borrow() {
            Some((g, l, r)) if *g == self.edit_gen.get() && *l == self.lang.get() => Some(r.clone()),
            _ => None,
        }
    }

    /// Compute the foldable blocks shortly (after a pause in typing) and
    /// redraw the handles.
    fn schedule_fold_ranges(self: &Rc<Self>) {
        if self.fold_pending.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            if let Some(d) = w.upgrade() {
                d.fold_pending.set(false);
                d.fold_ranges();
                d.gutter.queue_draw();
            }
        });
    }

    fn fold_iters(&self, f: &Fold) -> (gtk::TextIter, gtk::TextIter, gtk::TextIter) {
        let b = &self.buffer;
        (b.iter_at_mark(&f.head), b.iter_at_mark(&f.start), b.iter_at_mark(&f.end))
    }

    /// Index of the collapsed block whose header is `line`.
    fn fold_index_at_line(&self, line: i32) -> Option<usize> {
        self.folds.borrow().iter().position(|f| self.buffer.iter_at_mark(&f.head).line() == line)
    }

    /// Collapse a block (the cursor is moved out of it first).
    fn fold_range(&self, r: fold::Range) {
        let buf = &self.buffer;
        if self.fold_index_at_line(r.header as i32).is_some() {
            return;
        }
        let (Some(head), Some(start)) = (buf.iter_at_line(r.header as i32), buf.iter_at_line(r.header as i32 + 1)) else {
            return;
        };
        let end = buf.iter_at_line(r.last as i32 + 1).unwrap_or_else(|| buf.end_iter());
        if start >= end {
            return;
        }
        let ins = buf.iter_at_mark(&buf.get_insert());
        let sel = buf.iter_at_mark(&buf.selection_bound());
        if hides(&start, &end, &ins) || hides(&start, &end, &sel) {
            let mut it = head;
            if !it.ends_line() {
                it.forward_to_line_end();
            }
            buf.place_cursor(&it);
        }
        let tags = &self.shared.tags;
        buf.apply_tag(&tags.fold, &start, &end);
        buf.apply_tag(&tags.fold_head, &head, &start);
        self.folds.borrow_mut().push(Fold {
            head: buf.create_mark(None, &head, false),
            start: buf.create_mark(None, &start, true),
            end: buf.create_mark(None, &end, true),
        });
        self.gutter.queue_draw();
    }

    /// Remove the tags and marks of a block that is no longer collapsed.
    fn drop_fold(&self, f: Fold) {
        let buf = &self.buffer;
        let tags = &self.shared.tags;
        let (h, s, e) = self.fold_iters(&f);
        buf.remove_tag(&tags.fold, &s.min(e), &s.max(e));
        let mut a = h.min(s);
        a.set_line_offset(0);
        let mut b = h.max(s);
        b.forward_line();
        buf.remove_tag(&tags.fold_head, &a, &b);
        for m in [&f.head, &f.start, &f.end] {
            buf.delete_mark(m);
        }
    }

    /// Make sure every collapsed block is hidden and its header marked
    /// (after an expanded block or an edit removed some of the tags).
    fn retag_folds(&self) {
        let buf = &self.buffer;
        let tags = &self.shared.tags;
        for f in self.folds.borrow().iter() {
            let (mut h, s, e) = self.fold_iters(f);
            h.set_line_offset(0);
            ensure_tag(buf, &tags.fold, &s, &e);
            ensure_tag(buf, &tags.fold_head, &h, &s);
        }
    }

    fn unfold_index(&self, i: usize) {
        let f = self.folds.borrow_mut().remove(i);
        self.drop_fold(f);
        self.retag_folds();
        self.gutter.queue_draw();
    }

    /// Collapse or expand the block starting at `line`; false if there is
    /// none.
    pub fn toggle_fold_line(&self, line: i32) -> bool {
        if let Some(i) = self.fold_index_at_line(line) {
            self.unfold_index(i);
            return true;
        }
        let ranges = self.fold_ranges();
        match ranges.binary_search_by_key(&(line as u32), |r| r.header) {
            Ok(i) => {
                self.fold_range(ranges[i]);
                true
            }
            Err(_) => false,
        }
    }

    /// Collapse the block starting on the cursor line or else the innermost
    /// one containing it; false if there is none.
    pub fn fold_at_cursor(&self) -> bool {
        let line = self.buffer.iter_at_mark(&self.buffer.get_insert()).line() as u32;
        let ranges = self.fold_ranges();
        let open = |r: &&fold::Range| self.fold_index_at_line(r.header as i32).is_none();
        let pick = ranges.iter().filter(open).find(|r| r.header == line).or_else(|| {
            ranges.iter().filter(open).filter(|r| r.header < line && line <= r.last).max_by_key(|r| r.header)
        });
        match pick {
            Some(r) => {
                self.fold_range(*r);
                true
            }
            None => false,
        }
    }

    /// Expand the collapsed block on the cursor line; false if there is none.
    pub fn unfold_at_cursor(&self) -> bool {
        let line = self.buffer.iter_at_mark(&self.buffer.get_insert()).line();
        match self.fold_index_at_line(line) {
            Some(i) => {
                self.unfold_index(i);
                true
            }
            None => false,
        }
    }

    /// Collapse the body of every definition (functions, classes, …) or,
    /// in languages without definitions, every outermost block. Returns
    /// the number of blocks collapsed.
    pub fn fold_all(&self) -> usize {
        let ranges = self.fold_ranges();
        let lang = self.lang.get();
        // A C# or C++ namespace wraps the whole file: its body stays open.
        let wraps_file = |s: &crate::symbols::Symbol| matches!(lang, Lang::CSharp | Lang::Cpp) && s.kind == crate::symbols::SymKind::Module;
        let mut lines: Vec<u32> =
            crate::symbols::extract(lang, &self.text()).iter().filter(|s| !wraps_file(s)).map(|s| s.line).collect();
        lines.sort_unstable();
        lines.dedup();
        let mut targets: Vec<fold::Range> = Vec::new();
        for (k, &d) in lines.iter().enumerate() {
            // The body opens on the definition line or a few lines below
            // (long signatures), never past the next definition.
            let limit = lines.get(k + 1).copied().unwrap_or(u32::MAX).min(d + 6);
            if let Some(r) = ranges.iter().find(|r| r.block && r.header >= d && r.header < limit) {
                targets.push(*r);
            }
        }
        if targets.is_empty() {
            let mut outer_end = None;
            for r in ranges.iter().filter(|r| r.block) {
                if outer_end.is_none_or(|e| r.header > e) {
                    targets.push(*r);
                    outer_end = Some(r.last);
                }
            }
        }
        let before = self.folds.borrow().len();
        for r in targets {
            self.fold_range(r);
        }
        self.folds.borrow().len() - before
    }

    pub fn unfold_all(&self) {
        if self.folds.borrow().is_empty() {
            return;
        }
        let folds = std::mem::take(&mut *self.folds.borrow_mut());
        for f in folds {
            self.drop_fold(f);
        }
        self.gutter.queue_draw();
    }

    /// After an edit: expand the blocks whose header was changed into
    /// something else (lines joined or split, header deleted) and hide
    /// text inserted among hidden lines.
    fn check_folds(&self) {
        if self.folds.borrow().is_empty() {
            return;
        }
        let broken: Vec<usize> = self
            .folds
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let (h, s, e) = self.fold_iters(f);
                !(h.line() + 1 == s.line() && s.starts_line() && s < e && (e.starts_line() || e.is_end()))
            })
            .map(|(i, _)| i)
            .collect();
        for &i in broken.iter().rev() {
            let f = self.folds.borrow_mut().remove(i);
            self.drop_fold(f);
        }
        self.retag_folds();
    }

    /// Expand the blocks hiding the cursor (moved by a search, "go to
    /// line", a jump to a definition, …).
    fn reveal_cursor(&self) {
        loop {
            let ins = self.buffer.iter_at_mark(&self.buffer.get_insert());
            let hit = self.folds.borrow().iter().position(|f| {
                let (_, s, e) = self.fold_iters(f);
                hides(&s, &e, &ins)
            });
            match hit {
                Some(i) => self.unfold_index(i),
                None => break,
            }
        }
    }

    /// Backspace right after a collapsed block or Delete at the end of its
    /// header would edit hidden text: expand the block instead.
    fn unfold_at_edge(&self, backward: bool) -> bool {
        let buf = &self.buffer;
        if buf.has_selection() || self.folds.borrow().is_empty() {
            return false;
        }
        let ins = buf.iter_at_mark(&buf.get_insert());
        let hit = self.folds.borrow().iter().position(|f| {
            let (h, _, e) = self.fold_iters(f);
            if backward { e == ins && ins.starts_line() } else { h.line() == ins.line() && ins.ends_line() }
        });
        match hit {
            Some(i) => {
                self.unfold_index(i);
                true
            }
            None => false,
        }
    }

    #[cfg(debug_assertions)]
    pub fn debug_gutter_hover(&self, on: bool) {
        self.gutter_hover.set(on);
        self.gutter.queue_draw();
    }

    // ------------------------------------------------------------- content

    // ------------------------------------------------------- changes on disk

    /// Watch the file for changes made by other programs: `handler` is
    /// called (on the main loop) when it may have changed.
    pub fn set_disk_handler(&self, handler: Rc<dyn Fn()>) {
        *self.disk_handler.borrow_mut() = Some(handler);
        self.watch_file();
    }

    /// (Re)create the monitor of the file, or drop it when the document is
    /// unloaded or has no file.
    fn watch_file(&self) {
        let path = self.path.borrow().clone().filter(|_| self.loaded.get());
        let handler = self.disk_handler.borrow().clone();
        if let Some(m) = self.monitor.take() {
            m.cancel();
        }
        let (Some(path), Some(handler)) = (path, handler) else { return };
        // WATCH_MOVES also catches files replaced by a rename (atomic saves
        // of other editors, git checkout).
        if let Ok(m) = gio::File::for_path(&path).monitor_file(gio::FileMonitorFlags::WATCH_MOVES, None::<&gio::Cancellable>) {
            m.connect_changed(move |_, _, _, ev| {
                if !matches!(ev, gio::FileMonitorEvent::Unmounted | gio::FileMonitorEvent::AttributeChanged) {
                    handler();
                }
            });
            *self.monitor.borrow_mut() = Some(m);
        }
    }

    /// The content now matches the file on disk (just read or saved).
    pub fn record_disk_stamp(&self) {
        let stamp = self.path.borrow().as_deref().and_then(disk_stamp);
        self.disk_stamp.set(stamp);
        self.disk_change.set(DiskChange::None);
        // Saving under a new name (or reloading) changes the watched file.
        self.watch_file();
    }

    /// Compare the file on disk with the version that was read or saved.
    pub fn check_disk(&self) -> DiskChange {
        let Some(path) = self.path.borrow().clone() else { return DiskChange::None };
        let Some(known) = self.disk_stamp.get() else { return DiskChange::None };
        if !self.loaded.get() {
            return DiskChange::None;
        }
        match disk_stamp(&path) {
            None if !path.exists() => DiskChange::Deleted,
            None => DiskChange::None,
            Some(now) if now != known => DiskChange::Modified,
            Some(_) => DiskChange::None,
        }
    }

    /// Times the content was (re)loaded: marks set before a reload are lost.
    pub fn loads(&self) -> u32 {
        self.loads.get()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.get()
    }

    pub fn is_modified(&self) -> bool {
        if self.loaded.get() { self.buffer.is_modified() } else { self.unloaded_modified.get() }
    }

    fn is_nonempty(&self) -> bool {
        if self.loaded.get() { self.buffer.char_count() > 0 } else { self.unloaded_nonempty.get() }
    }

    /// Content that exists only in memory/cache: modified, or never saved.
    pub fn is_unsaved(&self) -> bool {
        self.is_modified() || (self.path.borrow().is_none() && self.is_nonempty())
    }

    pub fn is_pristine(&self) -> bool {
        self.path.borrow().is_none() && !self.is_nonempty()
    }

    pub fn text(&self) -> String {
        let (a, b) = self.buffer.bounds();
        self.buffer.text(&a, &b, true).into()
    }

    pub fn cursor_offset(&self) -> i32 {
        if self.loaded.get() {
            self.buffer.iter_at_mark(&self.buffer.get_insert()).offset()
        } else {
            self.saved_cursor.get()
        }
    }

    /// Replace the buffer content without recording undo history.
    pub fn set_content(self: &Rc<Self>, text: &str, modified: bool, cursor: i32) {
        self.unfold_all();
        self.loads.set(self.loads.get().wrapping_add(1));
        self.loading.set(true);
        self.buffer.begin_irreversible_action();
        self.buffer.set_text(text);
        self.buffer.end_irreversible_action();
        self.buffer.set_modified(modified);
        let it = self.buffer.iter_at_offset(cursor);
        self.buffer.place_cursor(&it);
        self.loading.set(false);
        self.loaded.set(true);
        self.record_disk_stamp();
        self.edit_gen.set(self.edit_gen.get() + 1);
        self.reset_highlight();
        self.update_gutter_width();
        self.update_cursor_decorations();
        self.refresh_title();
        self.gutter.queue_draw();
    }

    /// Load the content of a file-backed or cached document that was
    /// unloaded to save memory. Returns an error message on failure.
    pub fn ensure_loaded(self: &Rc<Self>) -> Result<(), String> {
        if self.loaded.get() {
            return Ok(());
        }
        let cursor = self.saved_cursor.get();
        let path = self.path.borrow().clone();
        if self.unloaded_modified.get() || path.is_none() {
            let id = self.id.borrow().clone();
            let text = config::read_cache_text(&id).map_err(|e| format!("cache {id}: {e}"))?;
            self.set_content(&text, self.unloaded_modified.get() || path.is_some(), cursor);
            self.cache_gen.set(self.edit_gen.get());
        } else if let Some(p) = path {
            let enc = self.encoding_known.get().then(|| self.encoding.get());
            let (text, enc, eol) = read_file(&p, enc).map_err(|e| format!("{}: {e}", p.display()))?;
            self.encoding.set(enc);
            self.encoding_known.set(true);
            self.eol.set(eol);
            if self.lang.get() == Lang::Plain {
                let head: String = text.chars().take(256).collect();
                self.lang.set(crate::lang::detect(Some(&p), &head));
            }
            self.set_content(&text, false, cursor);
        }
        let saved = std::mem::take(&mut *self.saved_folds.borrow_mut());
        if !saved.is_empty() {
            let ranges = self.fold_ranges();
            for h in saved {
                if let Ok(i) = ranges.binary_search_by_key(&h, |r| r.header) {
                    self.fold_range(ranges[i]);
                }
            }
        }
        Ok(())
    }

    /// Free the buffer memory. Unsaved content is written to the cache
    /// first; nothing is dropped if that fails.
    pub fn unload(self: &Rc<Self>) -> bool {
        if !self.loaded.get() {
            return true;
        }
        if self.is_unsaved() && self.write_cache().is_err() {
            return false;
        }
        self.saved_cursor.set(self.cursor_offset());
        self.unloaded_modified.set(self.buffer.is_modified());
        self.unloaded_nonempty.set(self.buffer.char_count() > 0);
        *self.saved_folds.borrow_mut() = self.folds.borrow().iter().map(|f| self.buffer.iter_at_mark(&f.head).line() as u32).collect();
        self.unfold_all();
        *self.fold_cache.borrow_mut() = None;
        self.loading.set(true);
        self.buffer.begin_irreversible_action();
        self.buffer.set_text("");
        self.buffer.end_irreversible_action();
        self.loading.set(false);
        self.loaded.set(false);
        self.watch_file();
        let mut hl = self.hl.borrow_mut();
        hl.states = vec![State::Normal];
        hl.states.shrink_to_fit();
        hl.lo = NO_DIRTY;
        true
    }

    /// Mark an unloaded document as restored from the cache.
    pub fn set_unloaded_from_cache(&self, meta: &CacheMeta) {
        self.loaded.set(false);
        self.unloaded_modified.set(meta.path.is_some());
        self.unloaded_nonempty.set(true);
        self.saved_cursor.set(meta.cursor);
        *self.path.borrow_mut() = meta.path.clone();
        self.encoding.set(meta.encoding);
        self.eol.set(meta.eol);
        self.lang.set(meta.lang);
        let id = self.id.borrow().clone();
        let head: String = std::fs::File::open(config::cache_dir().join(format!("{id}.txt")))
            .ok()
            .map(|f| {
                use std::io::Read;
                let mut buf = Vec::new();
                let _ = f.take(256).read_to_end(&mut buf);
                String::from_utf8_lossy(&buf).into_owned()
            })
            .unwrap_or_default();
        *self.title.borrow_mut() = self.compute_title(&head);
        self.update_label();
    }

    /// Mark a file-backed document as not yet loaded (lazy session restore);
    /// its encoding is detected when it is first shown.
    pub fn set_unloaded_file(&self, path: PathBuf, lang: Lang) {
        self.loaded.set(false);
        self.unloaded_modified.set(false);
        self.unloaded_nonempty.set(true);
        self.encoding_known.set(false);
        *self.path.borrow_mut() = Some(path);
        self.lang.set(lang);
        self.refresh_title();
    }

    pub fn cache_meta(&self) -> CacheMeta {
        CacheMeta {
            path: self.path.borrow().clone(),
            encoding: self.encoding.get(),
            eol: self.eol.get(),
            lang: self.lang.get(),
            cursor: self.cursor_offset(),
        }
    }

    pub fn cache_is_stale(&self) -> bool {
        self.loaded.get() && self.is_unsaved() && self.cache_gen.get() != self.edit_gen.get()
    }

    /// Write the current content to `~/.macaco/cache/`.
    pub fn write_cache(&self) -> std::io::Result<()> {
        if !self.loaded.get() {
            return Ok(());
        }
        let id = self.id.borrow().clone();
        config::write_cache(&id, &self.text(), &self.cache_meta())?;
        self.cache_gen.set(self.edit_gen.get());
        Ok(())
    }

    pub fn drop_cache(&self) {
        config::delete_cache(&self.id.borrow());
        self.cache_gen.set(u64::MAX);
    }

    /// Encode and write the document to `path`.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        self.ensure_loaded_noinit()?;
        let text = self.text();
        let text = encoding::apply_eol(&text, self.eol.get());
        let (bytes, _) = encoding::encode(&text, self.encoding.get());
        std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        *self.path.borrow_mut() = Some(path.to_path_buf());
        self.record_disk_stamp();
        self.buffer.set_modified(false);
        self.drop_cache();
        self.refresh_title();
        Ok(())
    }

    fn ensure_loaded_noinit(&self) -> Result<(), String> {
        if self.loaded.get() { Ok(()) } else { Err("documento non caricato".into()) }
    }

    // --------------------------------------------------------------- title

    fn compute_title(&self, head: &str) -> String {
        if let Some(p) = self.path.borrow().as_ref() {
            return p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string());
        }
        let t: String = head
            .trim_start()
            .chars()
            .take(10)
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        let t = t.trim_end().to_string();
        if t.is_empty() { "Senza titolo".into() } else { t }
    }

    /// Recompute the title (file name or first characters of the content).
    pub fn refresh_title(&self) {
        let title = if self.path.borrow().is_none() && self.loaded.get() {
            let start = self.buffer.start_iter();
            let mut end = start;
            // Skip leading blank space, then take a few characters.
            end.forward_chars(256);
            let head = self.buffer.text(&start, &end, true);
            self.compute_title(&head)
        } else {
            self.compute_title("")
        };
        let changed = *self.title.borrow() != title;
        if changed {
            *self.title.borrow_mut() = title;
        }
        self.update_label();
    }

    pub fn title(&self) -> String {
        self.title.borrow().clone()
    }

    /// Maximum number of characters the tab label may show.
    pub fn set_label_budget(&self, chars: usize) {
        if self.label_budget.get() != chars {
            self.label_budget.set(chars);
            self.update_label();
        }
    }

    /// Folders that tell this tab from others with the same file name
    /// (`page.tsx · dashboard`); empty when the name is unique.
    pub fn set_label_context(&self, context: String) {
        if *self.label_context.borrow() != context {
            *self.label_context.borrow_mut() = context;
            self.update_label();
        }
    }

    pub fn update_label(&self) {
        let title = self.title.borrow();
        let budget = self.label_budget.get().max(1);
        let mark = if self.is_unsaved() { "• " } else { "" };
        let context = self.label_context.borrow();
        let text = if self.path.borrow().is_none() {
            let shown: String = title.chars().take(budget).collect();
            format!("{mark}{shown}")
        } else if context.is_empty() {
            format!("{mark}{title}")
        } else {
            format!("{mark}{title} · {context}")
        };
        self.label.set_text(&text);
        if self.path.borrow().is_some() {
            // Fixed width so the notebook does not squeeze the label to "…".
            let w = text.chars().count().min(budget.max(4)) as i32;
            self.label.set_ellipsize(pango::EllipsizeMode::Middle);
            self.label.set_width_chars(w);
            self.label.set_max_width_chars(w);
        } else {
            self.label.set_ellipsize(pango::EllipsizeMode::None);
            self.label.set_width_chars(-1);
            self.label.set_max_width_chars(-1);
        }
        let tip = match self.path.borrow().as_ref() {
            Some(p) => p.display().to_string(),
            None => "Non ancora salvato".into(),
        };
        self.tab.set_tooltip_text(Some(&tip));
    }

    // ---------------------------------------------------------- highlighting

    pub fn set_lang(self: &Rc<Self>, lang: Lang) {
        self.lang.set(lang);
        if !fold::supported(lang) {
            self.unfold_all();
        }
        self.update_gutter_width();
        self.gutter.queue_draw();
        self.reset_highlight();
    }

    fn reset_highlight(self: &Rc<Self>) {
        let (a, b) = self.buffer.bounds();
        for t in &self.shared.tags.syntax {
            self.buffer.remove_tag(t, &a, &b);
        }
        let n = self.buffer.line_count().max(1) as usize;
        let enabled = self.lang.get() != Lang::Plain && self.buffer.char_count() < MAX_HIGHLIGHT_BYTES;
        {
            let mut hl = self.hl.borrow_mut();
            hl.states = vec![State::Normal; n];
            hl.enabled = enabled;
            hl.lo = NO_DIRTY;
            if enabled {
                hl.mark(0, n - 1);
            }
        }
        self.schedule_highlight();
    }

    fn schedule_highlight(self: &Rc<Self>) {
        {
            let mut hl = self.hl.borrow_mut();
            if hl.scheduled || hl.lo == NO_DIRTY || !hl.enabled || !self.loaded.get() {
                return;
            }
            hl.scheduled = true;
        }
        let w = Rc::downgrade(self);
        glib::idle_add_local(move || {
            let Some(d) = w.upgrade() else { return glib::ControlFlow::Break };
            if d.highlight_step() {
                glib::ControlFlow::Continue
            } else {
                d.hl.borrow_mut().scheduled = false;
                glib::ControlFlow::Break
            }
        });
    }

    /// Re-lex dirty lines for a bounded amount of time. Returns true if
    /// more work remains.
    fn highlight_step(&self) -> bool {
        let lang = self.lang.get();
        let buf = &self.buffer;
        let tags = &self.shared.tags;
        let mut hl = self.hl.borrow_mut();
        let n = buf.line_count().max(1) as usize;
        if hl.states.len() != n {
            hl.states.resize(n, State::Normal);
        }
        if hl.lo == NO_DIRTY || !hl.enabled || !self.loaded.get() {
            hl.lo = NO_DIRTY;
            return false;
        }
        if hl.lo >= n {
            hl.lo = NO_DIRTY;
            return false;
        }
        let deadline = Instant::now() + HL_BUDGET;
        let mut line = hl.lo;
        let mut state = if line == 0 { lexer::initial_state(lang) } else { hl.states[line - 1] };
        let mut chunk = 8;
        let mut toks = Vec::with_capacity(32);
        loop {
            let end_line = (line + chunk).min(n);
            let start = buf.iter_at_line(line as i32).unwrap_or_else(|| buf.end_iter());
            let end = if end_line >= n { buf.end_iter() } else { buf.iter_at_line(end_line as i32).unwrap_or_else(|| buf.end_iter()) };
            for t in &tags.syntax {
                buf.remove_tag(t, &start, &end);
            }
            let text = buf.text(&start, &end, true);
            let mut base = start;
            let mut converged = false;
            for (k, l) in text.split('\n').take(end_line - line).enumerate() {
                let ln = line + k;
                let end_state = lexer::lex_line(lang, state, l, &mut toks);
                for t in &toks {
                    let mut s = base;
                    s.set_line_index(t.start as i32);
                    let mut e = base;
                    e.set_line_index(t.end as i32);
                    buf.apply_tag(&tags.syntax[t.kind as usize], &s, &e);
                }
                let old = std::mem::replace(&mut hl.states[ln], end_state);
                if ln >= hl.hi && old == end_state {
                    converged = true;
                }
                state = end_state;
                base.forward_line();
            }
            line = end_line;
            if converged || line >= n {
                hl.lo = NO_DIRTY;
                return false;
            }
            if Instant::now() >= deadline {
                hl.lo = line;
                return true;
            }
            chunk = (chunk * 2).min(2048);
        }
    }

    // ------------------------------------------------- cursor decorations

    fn update_cursor_decorations(&self) {
        if !self.loaded.get() {
            return;
        }
        let tags = &self.shared.tags;
        let buf = &self.buffer;
        let cursor = buf.iter_at_mark(&buf.get_insert());
        if self.shared.settings.borrow().highlight_line {
            let (a, b) = buf.bounds();
            buf.remove_tag(&tags.curline, &a, &b);
            if !buf.has_selection() {
                let mut s = cursor;
                s.set_line_offset(0);
                let mut e = s;
                e.forward_line();
                buf.apply_tag(&tags.curline, &s, &e);
            }
        }
        // Matching bracket.
        if self.bracket_on.replace(false) {
            let (a, b) = buf.bounds();
            buf.remove_tag(&tags.bracket, &a, &b);
        }
        if let Some((p, q)) = self.find_bracket_pair(&cursor) {
            for off in [p, q] {
                let s = buf.iter_at_offset(off);
                let mut e = s;
                e.forward_char();
                buf.apply_tag(&tags.bracket, &s, &e);
            }
            self.bracket_on.set(true);
        }
    }

    fn find_bracket_pair(&self, cursor: &gtk::TextIter) -> Option<(i32, i32)> {
        const LIMIT: i32 = 20_000;
        let str_tag = &self.shared.tags.syntax[lexer::Kind::Str as usize];
        let com_tag = &self.shared.tags.syntax[lexer::Kind::Comment as usize];
        let ignored = |it: &gtk::TextIter| it.has_tag(str_tag) || it.has_tag(com_tag);
        let mut candidates = [None, None];
        candidates[0] = Some(*cursor);
        let mut before = *cursor;
        if before.backward_char() {
            candidates[1] = Some(before);
        }
        for it in candidates.into_iter().flatten() {
            let c = it.char();
            let (open, close, forward) = match c {
                '(' => ('(', ')', true),
                '[' => ('[', ']', true),
                '{' => ('{', '}', true),
                ')' => ('(', ')', false),
                ']' => ('[', ']', false),
                '}' => ('{', '}', false),
                _ => continue,
            };
            if ignored(&it) {
                continue;
            }
            let mut depth = 0;
            let mut j = it;
            for _ in 0..LIMIT {
                let ch = j.char();
                if (ch == open || ch == close) && !ignored(&j) {
                    depth += if (ch == open) == forward { 1 } else { -1 };
                    if depth == 0 {
                        return Some((it.offset(), j.offset()));
                    }
                }
                let moved = if forward { j.forward_char() } else { j.backward_char() };
                if !moved {
                    break;
                }
            }
            return None;
        }
        None
    }

    // ----------------------------------------------------------- editing

    pub fn handle_key(&self, key: gdk::Key, state: gdk::ModifierType) -> bool {
        let mods = state & (gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
        if !mods.is_empty() {
            return false;
        }
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter if !shift => self.newline_with_indent(),
            gdk::Key::Tab if !shift => self.tab_key(),
            gdk::Key::ISO_Left_Tab => {
                self.indent_lines(false);
                true
            }
            gdk::Key::Tab => {
                self.indent_lines(false);
                true
            }
            gdk::Key::BackSpace => self.unfold_at_edge(true) || self.smart_backspace(),
            gdk::Key::Delete | gdk::Key::KP_Delete => self.unfold_at_edge(false),
            gdk::Key::braceright | gdk::Key::bracketright | gdk::Key::parenright => self.closing_bracket(key),
            _ => false,
        }
    }

    fn line_prefix(&self, it: &gtk::TextIter) -> String {
        let mut s = *it;
        s.set_line_offset(0);
        self.buffer.text(&s, it, true).into()
    }

    fn newline_with_indent(&self) -> bool {
        let buf = &self.buffer;
        buf.begin_user_action();
        buf.delete_selection(true, true);
        let it = buf.iter_at_mark(&buf.get_insert());
        let prefix = self.line_prefix(&it);
        let indent: String = prefix.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let before = prefix.trim_end().chars().last();
        let after = it.char();
        let lang = self.lang.get();
        if lang == Lang::Markdown
            && let Some((next, empty)) = crate::format::md_list_continuation(&prefix)
        {
            let mut it = it;
            if empty {
                // Enter on an empty item ends the list.
                let mut start = it;
                start.set_line_offset(0);
                buf.delete(&mut start, &mut it);
            } else {
                buf.insert(&mut it, &format!("\n{next}"));
            }
            buf.end_user_action();
            self.view.scroll_mark_onscreen(&buf.get_insert());
            return true;
        }
        let opens = matches!(before, Some('{' | '[' | '(')) || (before == Some(':') && lang.colon_indents());
        let unit = self.shared.indent_unit(lang);
        let mut ins = String::with_capacity(indent.len() * 2 + 8);
        ins.push('\n');
        ins.push_str(&indent);
        let mut cursor_back = 0;
        if opens {
            ins.push_str(&unit);
            let pair = matches!((before, after), (Some('{'), '}') | (Some('['), ']') | (Some('('), ')'));
            if pair {
                let tail = format!("\n{indent}");
                cursor_back = tail.chars().count() as i32;
                ins.push_str(&tail);
            }
        }
        let mut it = it;
        buf.insert(&mut it, &ins);
        if cursor_back > 0 {
            it.backward_chars(cursor_back);
            buf.place_cursor(&it);
        }
        buf.end_user_action();
        self.view.scroll_mark_onscreen(&buf.get_insert());
        true
    }

    fn tab_key(&self) -> bool {
        let buf = &self.buffer;
        if let Some((a, b)) = buf.selection_bounds()
            && a.line() != b.line() {
                self.indent_lines(true);
                return true;
            }
        let unit = self.shared.indent_unit(self.lang.get());
        if unit == "\t" {
            return false;
        }
        buf.begin_user_action();
        buf.delete_selection(true, true);
        let mut it = buf.iter_at_mark(&buf.get_insert());
        let col = self.line_prefix(&it).chars().count();
        let w = unit.len();
        let n = w - col % w;
        buf.insert(&mut it, &" ".repeat(n));
        buf.end_user_action();
        true
    }

    /// Indent (or unindent) every line touched by the selection.
    pub fn indent_lines(&self, indent: bool) {
        let buf = &self.buffer;
        let (a, b) = buf.selection_bounds().unwrap_or_else(|| {
            let i = buf.iter_at_mark(&buf.get_insert());
            (i, i)
        });
        let first = a.line();
        let mut last = b.line();
        if last > first && b.line_offset() == 0 {
            last -= 1;
        }
        let unit = self.shared.indent_unit(self.lang.get());
        let width = self.shared.settings.borrow().tab_width as usize;
        buf.begin_user_action();
        for line in first..=last {
            let Some(mut s) = buf.iter_at_line(line) else { continue };
            if indent {
                if !s.ends_line() {
                    buf.insert(&mut s, &unit);
                }
            } else {
                let mut e = s;
                let mut removed = 0;
                while removed < width {
                    match e.char() {
                        '\t' => {
                            e.forward_char();
                            break;
                        }
                        ' ' => {
                            e.forward_char();
                            removed += 1;
                        }
                        _ => break,
                    }
                }
                if s != e {
                    buf.delete(&mut s, &mut e);
                }
            }
        }
        // Keep the whole block selected for repeated (un)indenting.
        if (last > first || buf.has_selection())
            && let (Some(s), Some(mut e)) = (buf.iter_at_line(first), buf.iter_at_line(last)) {
                if !e.ends_line() {
                    e.forward_to_line_end();
                }
                buf.select_range(&s, &e);
            }
        buf.end_user_action();
    }

    fn smart_backspace(&self) -> bool {
        let buf = &self.buffer;
        if buf.has_selection() {
            return false;
        }
        let it = buf.iter_at_mark(&buf.get_insert());
        let prefix = self.line_prefix(&it);
        if prefix.is_empty() || !prefix.bytes().all(|c| c == b' ') {
            return false;
        }
        let w = self.shared.settings.borrow().tab_width as usize;
        let n = (prefix.len() - 1) % w + 1;
        let mut s = it;
        s.backward_chars(n as i32);
        let mut e = it;
        buf.begin_user_action();
        buf.delete(&mut s, &mut e);
        buf.end_user_action();
        true
    }

    /// Typing a closing bracket on a blank line dedents it one level.
    fn closing_bracket(&self, key: gdk::Key) -> bool {
        let buf = &self.buffer;
        if buf.has_selection() || self.lang.get() == Lang::Plain {
            return false;
        }
        let it = buf.iter_at_mark(&buf.get_insert());
        let prefix = self.line_prefix(&it);
        if prefix.is_empty() || !prefix.chars().all(|c| c == ' ' || c == '\t') {
            return false;
        }
        let ch = match key {
            gdk::Key::braceright => "}",
            gdk::Key::bracketright => "]",
            _ => ")",
        };
        let unit = self.shared.indent_unit(self.lang.get());
        let w = self.shared.settings.borrow().tab_width as usize;
        let remove = if prefix.ends_with('\t') { 1 } else { (prefix.len() - 1) % w.max(1) + 1 }.min(unit.len().max(1));
        let mut s = it;
        s.backward_chars(remove as i32);
        let mut e = it;
        buf.begin_user_action();
        buf.delete(&mut s, &mut e);
        buf.insert(&mut s, ch);
        buf.end_user_action();
        true
    }

    /// Replace the whole text as one undoable action, keeping the cursor
    /// on the same line.
    pub fn replace_all_text(&self, text: &str) {
        let buf = &self.buffer;
        let it = buf.iter_at_mark(&buf.get_insert());
        let (line, col) = (it.line(), it.line_offset());
        buf.begin_user_action();
        let (mut a, mut b) = buf.bounds();
        buf.delete(&mut a, &mut b);
        let mut s = buf.start_iter();
        buf.insert(&mut s, text);
        buf.end_user_action();
        if let Some(mut it) = buf.iter_at_line(line) {
            let len = it.chars_in_line();
            it.set_line_offset(col.min((len - 1).max(0)));
            buf.place_cursor(&it);
        }
        self.view.scroll_mark_onscreen(&buf.get_insert());
    }

    pub fn weak(self: &Rc<Self>) -> Weak<Doc> {
        Rc::downgrade(self)
    }
}

/// Read and decode a file. With `enc == None` the encoding is detected.
pub fn read_file(path: &Path, enc: Option<Encoding>) -> std::io::Result<(String, Encoding, Eol)> {
    let bytes = std::fs::read(path)?;
    let enc = enc.unwrap_or_else(|| encoding::detect(&bytes));
    let text = encoding::decode(&bytes, enc);
    let (text, eol) = encoding::normalize_eol(text);
    Ok((text, enc, eol))
}
