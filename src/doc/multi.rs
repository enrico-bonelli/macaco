//! Multiple cursors: Alt+J adds the next occurrence of the selected text,
//! Shift+Alt+J all of them; typing, deleting, moving and the clipboard then
//! act at every cursor. Esc, a click or any other cursor move goes back to
//! one cursor.
//!
//! The buffer cursor (drawn by GTK) is the first one; the others are pairs
//! of marks (insert, selection bound) with their selections painted by a
//! tag and their carets drawn on a layer over each view, created only
//! while there is more than one cursor.

use super::Doc;
use crate::symbols;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use std::rc::Rc;

/// More cursors than this are not created ("select all occurrences").
const MAX_CURSORS: usize = 1000;

pub(super) struct Multi {
    /// Extra cursors: (insert, selection bound).
    cursors: Vec<(gtk::TextMark, gtk::TextMark)>,
    /// The text whose occurrences are added, whole words only or not.
    needle: String,
    whole: bool,
    /// Offset of the buffer cursor after the last command: a move to
    /// elsewhere goes back to one cursor.
    pos: i32,
    /// Alt+J searches the next occurrence from here.
    next: gtk::TextMark,
    /// Caret layers over the views and the scroll handlers redrawing them.
    layers: Vec<(gtk::Overlay, gtk::DrawingArea)>,
    handlers: Vec<(gtk::Adjustment, glib::SignalHandlerId)>,
}

enum Move {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

impl Doc {
    pub fn has_multi(&self) -> bool {
        self.multi.borrow().is_some()
    }

    /// Alt+J. Without a selection: selects the word at the cursor. Then:
    /// adds the next occurrence of the selected text (from the top after
    /// the end of the file). Returns the message for the status bar.
    pub fn select_next_occurrence(self: &Rc<Self>) -> Result<String, String> {
        if !self.loaded.get() {
            return Err("Documento non caricato".into());
        }
        let buf = self.buffer.clone();
        if !self.has_multi() {
            let Some((s, e)) = buf.selection_bounds() else {
                let (s, e) = self.word_at_cursor().ok_or("Nessuna parola sotto il cursore: seleziona un testo")?;
                self.word_pick.set(Some((s.offset(), e.offset())));
                self.busy(|| buf.select_range(&s, &e));
                return Ok("Parola selezionata: Alt+J aggiunge l'occorrenza successiva, Shift+Alt+J tutte".into());
            };
            let needle = buf.text(&s, &e, true).to_string();
            let whole = self.word_pick.get() == Some((s.offset(), e.offset()));
            // The cursor at the start of each occurrence.
            self.busy(|| buf.select_range(&s, &e));
            self.start_multi(needle, whole, &e);
        }
        let (needle, whole, from) = {
            let m = self.multi.borrow();
            let m = m.as_ref().expect("multi");
            (m.needle.clone(), m.whole, buf.iter_at_mark(&m.next))
        };
        let found = find(&from, None, &needle, whole).or_else(|| find(&buf.start_iter(), Some(&from), &needle, whole));
        let n = self.cursor_count();
        let Some((a, b)) = found.filter(|(a, _)| !self.selection_starts_at(a.offset())) else {
            if n == 1 {
                self.clear_multi();
                return Ok(format!("Nessun'altra occorrenza di «{}»", short(&needle)));
            }
            return Ok(format!("Nessun'altra occorrenza: {n} selezionate"));
        };
        {
            let mut m = self.multi.borrow_mut();
            let m = m.as_mut().expect("multi");
            m.cursors.push((buf.create_mark(None, &a, false), buf.create_mark(None, &b, false)));
            buf.move_mark(&m.next, &b);
        }
        let mark = buf.create_mark(None, &a, false);
        self.active_view().scroll_mark_onscreen(&mark);
        buf.delete_mark(&mark);
        self.refresh_multi();
        Ok(format!("{} occorrenze selezionate · Esc per tornare a un cursore", n + 1))
    }

    /// Shift+Alt+J: every occurrence of the selected text (or of the word
    /// at the cursor).
    pub fn select_all_occurrences(self: &Rc<Self>) -> Result<String, String> {
        if !self.loaded.get() {
            return Err("Documento non caricato".into());
        }
        let buf = self.buffer.clone();
        let (s, e) = match buf.selection_bounds() {
            Some(r) => r,
            None => {
                let (s, e) = self.word_at_cursor().ok_or("Nessuna parola sotto il cursore: seleziona un testo")?;
                self.word_pick.set(Some((s.offset(), e.offset())));
                (s, e)
            }
        };
        let whole = self.word_pick.get() == Some((s.offset(), e.offset()));
        let needle = buf.text(&s, &e, true).to_string();
        self.clear_multi();
        let first = s.offset();
        self.busy(|| buf.select_range(&s, &e));
        let mut found = Vec::new();
        let mut it = buf.start_iter();
        let mut capped = false;
        while let Some((a, b)) = find(&it, None, &needle, whole) {
            if a.offset() != first {
                if found.len() + 1 >= MAX_CURSORS {
                    capped = true;
                    break;
                }
                found.push((a, b));
            }
            it = b;
        }
        if found.is_empty() {
            return Ok(format!("Nessun'altra occorrenza di «{}»", short(&needle)));
        }
        let e = buf.iter_at_offset(buf.iter_at_mark(&buf.selection_bound()).offset());
        self.start_multi(needle, whole, &e);
        {
            let mut m = self.multi.borrow_mut();
            let m = m.as_mut().expect("multi");
            m.cursors = found.iter().map(|(a, b)| (buf.create_mark(None, a, false), buf.create_mark(None, b, false))).collect();
        }
        self.refresh_multi();
        let n = found.len() + 1;
        Ok(if capped {
            format!("Selezionate le prime {n} occorrenze (limite) · Esc per tornare a un cursore")
        } else {
            format!("{n} occorrenze selezionate · Esc per tornare a un cursore")
        })
    }

    /// Back to the buffer cursor alone.
    pub fn clear_multi(&self) {
        let Some(m) = self.multi.borrow_mut().take() else { return };
        let buf = &self.buffer;
        for (a, b) in &m.cursors {
            buf.delete_mark(a);
            buf.delete_mark(b);
        }
        buf.delete_mark(&m.next);
        super::remove_tag_where_applied(buf, &self.shared.tags.multi_sel);
        for (adj, id) in m.handlers {
            adj.disconnect(id);
        }
        for (overlay, da) in &m.layers {
            overlay.remove_overlay(da);
        }
    }

    /// A key pressed while there are several cursors. True if handled.
    pub fn multi_key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> bool {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        if state.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK) {
            return false;
        }
        use gdk::Key as K;
        match key {
            K::Escape => self.clear_multi(),
            K::Left | K::KP_Left => self.move_cursors(Move::Left, ctrl, shift),
            K::Right | K::KP_Right => self.move_cursors(Move::Right, ctrl, shift),
            K::Up | K::KP_Up if !ctrl => self.move_cursors(Move::Up, false, shift),
            K::Down | K::KP_Down if !ctrl => self.move_cursors(Move::Down, false, shift),
            K::Home | K::KP_Home if !ctrl => self.move_cursors(Move::Home, false, shift),
            K::End | K::KP_End if !ctrl => self.move_cursors(Move::End, false, shift),
            K::BackSpace => self.delete_at_cursors(true, ctrl),
            K::Delete | K::KP_Delete => self.delete_at_cursors(false, ctrl),
            K::Return | K::KP_Enter => {
                self.edit_cursors(|buf, _, ins| {
                    let mut ls = buf.iter_at_mark(ins);
                    ls.set_line_offset(0);
                    let line = buf.text(&ls, &buf.iter_at_mark(ins), false);
                    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                    format!("\n{indent}")
                });
            }
            K::Tab if !shift && !ctrl => {
                let unit = self.shared.indent_unit(self.lang.get()).into_owned();
                self.edit_cursors(|_, _, _| unit.clone());
            }
            K::ISO_Left_Tab | K::Tab => {}
            _ if ctrl => match key {
                K::c | K::C => return self.multi_copy(),
                K::x | K::X => return self.multi_cut(),
                K::v | K::V => self.multi_paste(),
                _ => return false,
            },
            _ => match key.to_unicode().filter(|c| !c.is_control()) {
                Some(c) => {
                    let s = c.to_string();
                    self.edit_cursors(|_, _, _| s.clone());
                }
                None => return false,
            },
        }
        true
    }

    /// Copies the selections (in document order, one per line). False if
    /// nothing is selected.
    pub fn multi_copy(&self) -> bool {
        let buf = &self.buffer;
        let parts: Vec<String> = self
            .sorted_selections()
            .into_iter()
            .filter(|(a, b)| a != b)
            .map(|(a, b)| buf.text(&buf.iter_at_offset(a), &buf.iter_at_offset(b), false).to_string())
            .collect();
        if parts.is_empty() {
            return false;
        }
        self.view.clipboard().set_text(&parts.join("\n"));
        true
    }

    pub fn multi_cut(self: &Rc<Self>) -> bool {
        if !self.multi_copy() {
            return false;
        }
        self.edit_cursors(|_, _, _| String::new());
        true
    }

    /// Pastes the clipboard at every cursor; a text with one line per
    /// cursor is split among them.
    pub fn multi_paste(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.view.clipboard().read_text_async(None::<&gio::Cancellable>, move |res| {
            let (Some(d), Ok(Some(text))) = (w.upgrade(), res) else { return };
            if !d.has_multi() {
                return;
            }
            let lines: Vec<&str> = text.split('\n').collect();
            let split = lines.len() == d.cursor_count();
            d.edit_cursors(|_, i, _| if split { lines[i].to_string() } else { text.to_string() });
        });
    }

    // ------------------------------------------------------------ internals

    fn start_multi(self: &Rc<Self>, needle: String, whole: bool, next: &gtk::TextIter) {
        let next = self.buffer.create_mark(None, next, true);
        let pos = self.cursor_iter().offset();
        *self.multi.borrow_mut() = Some(Multi { cursors: Vec::new(), needle, whole, pos, next, layers: Vec::new(), handlers: Vec::new() });
        self.add_layers();
    }

    /// A drawing area over each view for the extra carets.
    fn add_layers(self: &Rc<Self>) {
        let mut layers = Vec::new();
        let mut handlers = Vec::new();
        for (view, overlay, scroller) in self.views_with_overlay() {
            let da = gtk::DrawingArea::new();
            da.set_can_target(false);
            da.set_can_focus(false);
            let (w, wv) = (Rc::downgrade(self), view.downgrade());
            da.set_draw_func(move |da, cr, _, _| {
                if let (Some(d), Some(v)) = (w.upgrade(), wv.upgrade()) {
                    d.draw_carets(&v, da, cr);
                }
            });
            overlay.add_overlay(&da);
            for adj in [scroller.vadjustment(), scroller.hadjustment()] {
                let g = da.downgrade();
                let id = adj.connect_value_changed(move |_| {
                    if let Some(g) = g.upgrade() {
                        g.queue_draw();
                    }
                });
                handlers.push((adj, id));
            }
            layers.push((overlay, da));
        }
        if let Some(m) = self.multi.borrow_mut().as_mut() {
            m.layers = layers;
            m.handlers = handlers;
        }
    }

    fn draw_carets(&self, view: &gtk::TextView, da: &gtk::DrawingArea, cr: &gtk::cairo::Context) {
        let m = self.multi.borrow();
        let Some(m) = m.as_ref() else { return };
        let buf = &self.buffer;
        let rect = view.visible_rect();
        let top = view.line_at_y(rect.y().max(0)).0.line();
        let bottom = view.line_at_y(rect.y() + rect.height()).0.line();
        let c = if self.shared.settings.borrow().dark { (0.85, 0.86, 0.88) } else { (0.15, 0.15, 0.2) };
        cr.set_source_rgb(c.0, c.1, c.2);
        for (ins, _) in &m.cursors {
            let it = buf.iter_at_mark(ins);
            if it.line() < top || it.line() > bottom {
                continue;
            }
            let loc = view.iter_location(&it);
            let (x, y) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, loc.x(), loc.y());
            if let Some(p) = view.compute_point(da, &graphene::Point::new(x as f32, y as f32)) {
                cr.rectangle(p.x() as f64, p.y() as f64, 2.0, loc.height() as f64);
            }
        }
        let _ = cr.fill();
    }

    /// Runs `f` without leaving the multi-cursor mode (a cursor move not
    /// made here goes back to one cursor).
    fn busy<R>(&self, f: impl FnOnce() -> R) -> R {
        let was = self.multi_busy.replace(true);
        let r = f();
        self.multi_busy.set(was);
        r
    }

    /// The buffer cursor moved or the text changed outside of the
    /// multi-cursor commands.
    pub(super) fn multi_cursor_moved(&self) {
        if self.multi_busy.get() {
            return;
        }
        let moved = self.multi.borrow().as_ref().is_some_and(|m| m.pos != self.cursor_iter().offset());
        if moved {
            self.clear_multi();
        }
    }

    fn cursor_count(&self) -> usize {
        1 + self.multi.borrow().as_ref().map_or(0, |m| m.cursors.len())
    }

    /// Whether a cursor's selection starts at `offset`.
    fn selection_starts_at(&self, offset: i32) -> bool {
        self.sorted_selections().iter().any(|(a, _)| *a == offset)
    }

    /// (start, end) offsets of every cursor, in document order.
    fn sorted_selections(&self) -> Vec<(i32, i32)> {
        let buf = &self.buffer;
        let pair = |i: &gtk::TextMark, b: &gtk::TextMark| {
            let (x, y) = (buf.iter_at_mark(i).offset(), buf.iter_at_mark(b).offset());
            (x.min(y), x.max(y))
        };
        let mut v = vec![pair(&buf.get_insert(), &buf.selection_bound())];
        if let Some(m) = self.multi.borrow().as_ref() {
            v.extend(m.cursors.iter().map(|(i, b)| pair(i, b)));
        }
        v.sort_unstable();
        v
    }

    fn word_at_cursor(&self) -> Option<(gtk::TextIter, gtk::TextIter)> {
        let buf = &self.buffer;
        let cur = buf.iter_at_mark(&buf.get_insert());
        let mut a = cur;
        while a.backward_char() {
            if !symbols::is_ident_char(a.char()) {
                a.forward_char();
                break;
            }
        }
        let mut b = cur;
        while symbols::is_ident_char(b.char()) && b.forward_char() {}
        (a.offset() < b.offset()).then_some((a, b))
    }

    /// Runs `f` on every cursor (marks of insert and bound) in document
    /// order, the buffer cursor included, as one undo step.
    fn for_cursors(&self, edit: bool, f: impl Fn(&gtk::TextBuffer, usize, &gtk::TextMark, &gtk::TextMark)) {
        let buf = self.buffer.clone();
        self.busy(|| {
            let p_ins = buf.create_mark(None, &buf.iter_at_mark(&buf.get_insert()), false);
            let p_bound = buf.create_mark(None, &buf.iter_at_mark(&buf.selection_bound()), false);
            let mut all = vec![(p_ins.clone(), p_bound.clone())];
            if let Some(m) = self.multi.borrow().as_ref() {
                all.extend(m.cursors.iter().cloned());
            }
            all.sort_by_key(|(i, b)| buf.iter_at_mark(i).offset().min(buf.iter_at_mark(b).offset()));
            if edit {
                buf.begin_user_action();
            }
            for (k, (i, b)) in all.iter().enumerate() {
                f(&buf, k, i, b);
            }
            if edit {
                buf.end_user_action();
            }
            buf.select_range(&buf.iter_at_mark(&p_ins), &buf.iter_at_mark(&p_bound));
            buf.delete_mark(&p_ins);
            buf.delete_mark(&p_bound);
        });
        self.merge_cursors();
        self.refresh_multi();
        self.active_view().scroll_mark_onscreen(&self.buffer.get_insert());
    }

    /// Replaces the selection of every cursor with the text `f` gives it.
    fn edit_cursors(&self, f: impl Fn(&gtk::TextBuffer, usize, &gtk::TextMark) -> String) {
        self.for_cursors(true, |buf, k, ins, bound| {
            let text = f(buf, k, ins);
            let (mut a, mut b) = (buf.iter_at_mark(ins), buf.iter_at_mark(bound));
            if a != b {
                buf.delete(&mut a, &mut b);
            }
            if !text.is_empty() {
                let mut it = buf.iter_at_mark(ins);
                buf.insert(&mut it, &text);
            }
            // Both marks after the inserted text.
            let it = buf.iter_at_mark(ins);
            buf.move_mark(bound, &it);
        });
    }

    fn delete_at_cursors(&self, backward: bool, word: bool) {
        self.for_cursors(true, |buf, _, ins, bound| {
            let (mut a, mut b) = (buf.iter_at_mark(ins), buf.iter_at_mark(bound));
            if a == b {
                match (backward, word) {
                    (true, false) => {
                        buf.backspace(&mut a, true, true);
                        buf.move_mark(bound, &buf.iter_at_mark(ins));
                        return;
                    }
                    (true, true) => {
                        a.backward_word_start();
                    }
                    (false, false) => {
                        b.forward_cursor_position();
                    }
                    (false, true) => {
                        b.forward_word_end();
                    }
                }
            }
            if a != b {
                buf.delete(&mut a, &mut b);
            }
            buf.move_mark(bound, &buf.iter_at_mark(ins));
        });
    }

    fn move_cursors(&self, mv: Move, word: bool, extend: bool) {
        self.for_cursors(false, |buf, _, ins, bound| {
            let (i, b) = (buf.iter_at_mark(ins), buf.iter_at_mark(bound));
            let mut it = i;
            match mv {
                // Left / Right without Shift collapse a selection.
                Move::Left if i != b && !extend => it = if i.offset() < b.offset() { i } else { b },
                Move::Right if i != b && !extend => it = if i.offset() > b.offset() { i } else { b },
                Move::Left => {
                    if word {
                        it.backward_word_start();
                    } else {
                        it.backward_cursor_position();
                    }
                }
                Move::Right => {
                    if word {
                        it.forward_word_end();
                    } else {
                        it.forward_cursor_position();
                    }
                }
                Move::Up | Move::Down => {
                    let col = it.line_offset();
                    let line = it.line() + if matches!(mv, Move::Up) { -1 } else { 1 };
                    it = match buf.iter_at_line(line) {
                        Some(mut l) if line >= 0 => {
                            if !l.ends_line() {
                                let mut e = l;
                                e.forward_to_line_end();
                                l.set_line_offset(col.min(e.line_offset()));
                            }
                            l
                        }
                        _ if matches!(mv, Move::Up) => {
                            let mut l = it;
                            l.set_line_offset(0);
                            l
                        }
                        _ => {
                            let mut l = it;
                            if !l.ends_line() {
                                l.forward_to_line_end();
                            }
                            l
                        }
                    };
                }
                // First non-blank character, then the start of the line.
                Move::Home => {
                    let mut first = it;
                    first.set_line_offset(0);
                    while !first.ends_line() && matches!(first.char(), ' ' | '\t') {
                        first.forward_char();
                    }
                    if first == it {
                        it.set_line_offset(0);
                    } else {
                        it = first;
                    }
                }
                Move::End => {
                    if !it.ends_line() {
                        it.forward_to_line_end();
                    }
                }
            }
            buf.move_mark(ins, &it);
            if !extend {
                buf.move_mark(bound, &it);
            }
        });
    }

    /// Cursors that ended in the same place become one.
    fn merge_cursors(&self) {
        let buf = &self.buffer;
        let mut seen = vec![buf.iter_at_mark(&buf.get_insert()).offset()];
        let mut m = self.multi.borrow_mut();
        let Some(m) = m.as_mut() else { return };
        m.cursors.retain(|(i, b)| {
            let o = buf.iter_at_mark(i).offset();
            if seen.contains(&o) {
                buf.delete_mark(i);
                buf.delete_mark(b);
                false
            } else {
                seen.push(o);
                true
            }
        });
    }

    /// Paints the selections of the extra cursors and redraws their carets.
    fn refresh_multi(&self) {
        let buf = &self.buffer;
        let tag = &self.shared.tags.multi_sel;
        super::remove_tag_where_applied(buf, tag);
        let pos = self.cursor_iter().offset();
        let mut m = self.multi.borrow_mut();
        let Some(m) = m.as_mut() else { return };
        m.pos = pos;
        for (i, b) in &m.cursors {
            let (a, b) = (buf.iter_at_mark(i), buf.iter_at_mark(b));
            if a != b {
                buf.apply_tag(tag, &a, &b);
            }
        }
        for (_, da) in &m.layers {
            da.queue_draw();
        }
    }
}

/// The next occurrence of `needle` from `from` (before `limit`); with
/// `whole`, only where it is not part of a longer word.
fn find(from: &gtk::TextIter, limit: Option<&gtk::TextIter>, needle: &str, whole: bool) -> Option<(gtk::TextIter, gtk::TextIter)> {
    let mut it = *from;
    loop {
        let (a, b) = it.forward_search(needle, gtk::TextSearchFlags::empty(), limit)?;
        let mut before = a;
        let joined = (before.backward_char() && symbols::is_ident_char(before.char())) || symbols::is_ident_char(b.char());
        if !whole || !joined {
            return Some((a, b));
        }
        it = a;
        if !it.forward_char() {
            return None;
        }
    }
}

/// `s` shortened for messages.
fn short(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() > 40 || first.len() < s.len() {
        format!("{}…", first.chars().take(40).collect::<String>())
    } else {
        first.to_string()
    }
}
