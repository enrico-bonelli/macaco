//! The search and replace bar of the window.

use super::*;
use crate::search::Hits;

/// Do not paint more search matches than this at once.
const MAX_MATCH_HIGHLIGHT: usize = 20_000;
/// Lines painted at least above and below the visible ones.
const PAINT_MARGIN_LINES: i32 = 100;
/// Edited documents up to this size are searched again when scrolled.
const RESEARCH_ON_SCROLL_CHARS: i32 = 2_000_000;

impl Editor {
    pub(super) fn build_search_bar() -> (gtk::Revealer, SearchUi) {
        let find = gtk::SearchEntry::new();
        find.set_placeholder_text(Some("Cerca"));
        find.set_hexpand(true);
        let replace = gtk::Entry::new();
        replace.set_placeholder_text(Some("Sostituisci con"));
        replace.set_hexpand(true);
        let case = toggle("Aa", "Maiuscole/minuscole");
        let word = toggle("ab", "Parola intera");
        let regex = toggle(".*", "Espressione regolare");
        let info = gtk::Label::new(None);
        info.set_width_chars(16);
        info.add_css_class("dim-label");

        let row1 = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let prev = icon_button("go-up-symbolic", "Precedente (Maiusc+F3)");
        prev.set_action_name(Some("win.find-prev"));
        let next = icon_button("go-down-symbolic", "Successivo (F3)");
        next.set_action_name(Some("win.find-next"));
        let close = icon_button("window-close-symbolic", "Chiudi (Esc)");
        close.set_action_name(Some("win.search-close"));
        for w in [find.upcast_ref::<gtk::Widget>(), prev.upcast_ref(), next.upcast_ref(), case.upcast_ref(), word.upcast_ref(), regex.upcast_ref(), info.upcast_ref(), close.upcast_ref()] {
            row1.append(w);
        }
        let replace_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let rep_one = gtk::Button::with_label("Sostituisci");
        rep_one.set_action_name(Some("win.replace-one"));
        let rep_all = gtk::Button::with_label("Sostituisci tutto");
        rep_all.set_action_name(Some("win.replace-all"));
        replace_row.append(&replace);
        replace_row.append(&rep_one);
        replace_row.append(&rep_all);

        let vb = gtk::Box::new(gtk::Orientation::Vertical, 4);
        vb.add_css_class("searchbar-box");
        vb.append(&row1);
        vb.append(&replace_row);
        let revealer = gtk::Revealer::builder()
            .child(&vb)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .transition_duration(120)
            .build();
        (
            revealer.clone(),
            SearchUi { revealer: revealer.clone(), find, replace, replace_row, case, word, regex, info })
    }

    pub(super) fn show_search(&self, replace: bool) {
        self.search.replace_row.set_visible(replace);
        self.search.revealer.set_reveal_child(true);
        if let Some(d) = self.active_doc()
            && let Some((a, b)) = d.buffer.selection_bounds()
            && a.line() == b.line()
        {
            // Setting the same text would search again (twice: the entry
            // is emptied first).
            let sel = d.buffer.text(&a, &b, true);
            if self.search.find.text() != sel {
                self.search.find.set_text(&sel);
            }
        }
        self.search.find.grab_focus();
        self.incremental_search();
    }

    pub(super) fn hide_search(&self) {
        self.search.revealer.set_reveal_child(false);
        for d in self.docs.borrow().iter() {
            if d.is_loaded() {
                crate::doc::remove_tag_where_applied(&d.buffer, &self.shared.tags.search);
            }
            *d.search_hits.borrow_mut() = None;
        }
        self.search.info.set_text("");
        if let Some(d) = self.active_doc() {
            d.focus();
        }
    }

    pub(super) fn query(&self) -> Option<Query> {
        let opts = Options {
            case_sensitive: self.search.case.is_active(),
            whole_word: self.search.word.is_active(),
            regex: self.search.regex.is_active(),
        };
        match Query::new(&self.search.find.text(), opts) {
            Ok(q) => {
                self.search.find.remove_css_class("error");
                Some(q)
            }
            Err(e) => {
                if e.is_empty() {
                    self.search.find.remove_css_class("error");
                    self.search.info.set_text("");
                } else {
                    self.search.find.add_css_class("error");
                    self.search.info.set_text("regex non valida");
                    self.search.info.set_tooltip_text(Some(&e));
                }
                None
            }
        }
    }

    /// The matches of `q` in `d`, searched again only when the query or
    /// the text changed.
    fn hits(&self, d: &Rc<Doc>, q: &Query) -> Rc<Hits> {
        if let Some(h) = d.search_hits.borrow().as_ref()
            && h.is_for(q, d.edit_gen())
        {
            return h.clone();
        }
        let h = Rc::new(Hits::new(q, &d.buffer_text(), d.edit_gen()));
        *d.search_hits.borrow_mut() = Some(h.clone());
        h
    }

    /// `hits`, with the count shown and the visible matches painted.
    fn refresh_hits(&self, d: &Rc<Doc>, q: &Query) -> Rc<Hits> {
        let h = self.hits(d, q);
        self.show_count(h.count);
        self.paint_visible(d, &h);
        h
    }

    fn show_count(&self, count: usize) {
        self.search.info.set_tooltip_text(None);
        self.search.info.set_text(&match count {
            0 => "nessun risultato".to_string(),
            1 => "1 risultato".to_string(),
            n => format!("{n} risultati"),
        });
    }

    /// Remove the painted matches of `d` (its matches are kept).
    fn clear_paint(&self, d: &Doc) {
        crate::doc::remove_tag_where_applied(&d.buffer, &self.shared.tags.search);
        if let Some(h) = d.search_hits.borrow().as_ref() {
            h.painted.set((0, 0));
        }
    }

    /// Paint the matches around the visible lines of `d`. Painting every
    /// match of a big file costs tens of milliseconds and fills the buffer
    /// with tag toggles: the rest is painted when scrolled into view.
    fn paint_visible(&self, d: &Doc, h: &Hits) {
        let buf = &d.buffer;
        // Visible lines of each view (two when the tab is split).
        let ranges: Vec<(i32, i32)> = d
            .views()
            .iter()
            .map(|view| {
                let rect = view.visible_rect();
                if rect.height() > 0 {
                    (view.line_at_y(rect.y()).0.line(), view.line_at_y(rect.y() + rect.height()).0.line())
                } else {
                    let l = d.cursor_iter().line();
                    (l, l)
                }
            })
            .collect();
        let chars_of = |line: i32| buf.iter_at_line(line).map_or_else(|| buf.char_count(), |it| it.offset()) as u32;
        let (pa, pb) = h.painted.get();
        if ranges.len() == 1 {
            let (top, bottom) = ranges[0];
            let (a, b) = (chars_of(top), chars_of(bottom + 1));
            if pa < pb && pa <= a && b <= pb {
                return;
            }
        }
        let tag = &self.shared.tags.search;
        crate::doc::remove_tag_where_applied(buf, tag);
        let mut painted = (0, 0);
        for &(top, bottom) in &ranges {
            let margin = (bottom - top).max(PAINT_MARGIN_LINES);
            let (a, b) = (chars_of((top - margin).max(0)), chars_of(bottom + 1 + margin));
            for &(s, e) in h.within(a, b).iter().take(MAX_MATCH_HIGHLIGHT) {
                buf.apply_tag(tag, &buf.iter_at_offset(s as i32), &buf.iter_at_offset(e as i32));
            }
            painted = (a, b);
        }
        // Split tab: the two ranges are painted again at every scroll.
        h.painted.set(if ranges.len() == 1 { painted } else { (0, 0) });
    }

    /// Repaint the matches of `d` after it scrolled (once per idle).
    pub(super) fn schedule_search_paint(self: &Rc<Self>, d: &Rc<Doc>) {
        if d.search_hits.borrow().is_none() || !self.search.revealer.reveals_child() || d.search_paint_queued.replace(true) {
            return;
        }
        let (ed, wd) = (Rc::downgrade(self), d.weak());
        // Before the next frame is drawn.
        glib::idle_add_local_full(glib::Priority::HIGH_IDLE, move || {
            let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) else { return glib::ControlFlow::Break };
            d.search_paint_queued.set(false);
            if !ed.search.revealer.reveals_child() || !d.is_loaded() {
                return glib::ControlFlow::Break;
            }
            let Some(h) = d.search_hits.borrow().clone() else { return glib::ControlFlow::Break };
            if h.generation == d.edit_gen() {
                ed.paint_visible(&d, &h);
            } else if d.buffer.char_count() <= RESEARCH_ON_SCROLL_CHARS
                && let Some(q) = ed.query()
            {
                // Edited since the search: cheap to search again. In big
                // files the painted matches move with the text until the
                // next search.
                ed.refresh_hits(&d, &q);
            }
            glib::ControlFlow::Break
        });
    }

    /// Show the matches of the query in `d` (a tab switched to).
    pub(super) fn highlight_matches(&self, d: &Rc<Doc>) {
        match self.query() {
            Some(q) => {
                self.refresh_hits(d, &q);
            }
            None => self.clear_paint(d),
        }
    }

    pub(super) fn incremental_search(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else {
            self.clear_paint(&d);
            return;
        };
        let h = self.refresh_hits(&d, &q);
        if h.count > 0 {
            // Restart from the beginning of the current selection.
            let buf = &d.buffer;
            let start = buf.selection_bounds().map(|(a, _)| a).unwrap_or_else(|| buf.iter_at_mark(&buf.get_insert()));
            buf.place_cursor(&start);
            self.select_match(&d, &q, &h, true);
        }
    }

    /// Select the next/previous match, wrapping around.
    pub(super) fn find_step(&self, forward: bool) {
        let Some(d) = self.active_doc() else { return };
        if self.search.find.text().is_empty() {
            self.show_search(false);
            return;
        }
        let Some(q) = self.query() else { return };
        let h = self.refresh_hits(&d, &q);
        self.select_match(&d, &q, &h, forward);
    }

    /// `find_step` with the matches of `d` at hand.
    fn select_match(&self, d: &Rc<Doc>, q: &Query, h: &Hits, forward: bool) {
        let buf = &d.buffer;
        let (sel_a, sel_b) = buf.selection_bounds().unwrap_or_else(|| {
            let i = buf.iter_at_mark(&buf.get_insert());
            (i, i)
        });
        let pivot = if forward { sel_b.offset() } else { sel_a.offset() } as u32;
        let found = if forward { h.next(pivot) } else { h.prev(pivot) };
        let found = found.unwrap_or_else(|| Self::scan_match(&d.buffer_text(), q, pivot, forward));
        match found {
            Some((s, e)) => {
                buf.select_range(&buf.iter_at_offset(s as i32), &buf.iter_at_offset(e as i32));
                d.active_view().scroll_to_mark(&buf.get_insert(), 0.1, false, 0.0, 0.0);
            }
            None => self.search.info.set_text("nessun risultato"),
        }
    }

    /// Next/previous match from character `pivot` searching `text`, for
    /// the matches past the ones kept by `Hits`; character offsets.
    fn scan_match(text: &str, q: &Query, pivot: u32, forward: bool) -> Option<(u32, u32)> {
        let pivot = crate::search::byte_at_char(text, pivot as usize);
        let nonempty = |m: &regex::Match| m.start() != m.end();
        let found = if forward {
            q.re.find_iter(&text[pivot..]).find(nonempty).map(|m| (m.start() + pivot, m.end() + pivot)).or_else(|| {
                q.re.find_iter(text).find(nonempty).map(|m| (m.start(), m.end()))
            })
        } else {
            let mut before = None;
            let mut last = None;
            for m in q.re.find_iter(text).filter(nonempty) {
                if m.start() < pivot {
                    before = Some((m.start(), m.end()));
                }
                last = Some((m.start(), m.end()));
            }
            before.or(last)
        };
        found.map(|(s, e)| {
            let cs = text[..s].chars().count();
            (cs as u32, (cs + text[s..e].chars().count()) as u32)
        })
    }

    pub(super) fn replace_one(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else { return };
        let h = self.hits(&d, &q);
        let buf = &d.buffer;
        if let Some((mut a, mut b)) = buf.selection_bounds() {
            let (sa, sb) = (a.offset() as u32, b.offset() as u32);
            // The selection must be exactly a match.
            let text = (q.opts.regex || h.is_match(sa, sb).is_none()).then(|| d.buffer_text());
            let start_b = text.as_ref().map(|t| crate::search::byte_at_char(t, sa as usize));
            let is_match = h.is_match(sa, sb).unwrap_or_else(|| {
                let (Some(t), Some(start_b)) = (&text, start_b) else { return false };
                q.re.find_at(t, start_b).is_some_and(|m| m.start() == start_b && t[m.start()..m.end()].chars().count() == (sb - sa) as usize)
            });
            if is_match {
                let repl = self.search.replace.text();
                let rep = match (&text, start_b) {
                    (Some(t), Some(start_b)) => q.replacement(t, start_b, &repl),
                    _ => repl.to_string(),
                };
                buf.begin_user_action();
                buf.delete(&mut a, &mut b);
                buf.insert(&mut a, &rep);
                buf.end_user_action();
            }
        }
        let h = self.refresh_hits(&d, &q);
        self.select_match(&d, &q, &h, true);
    }

    pub(super) fn replace_all(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else { return };
        let text = d.buffer_text();
        let ascii = text.is_ascii();
        let repl = self.search.replace.text().to_string();
        let mut edits: Vec<(i32, i32, String)> = Vec::new();
        let mut last = (0usize, 0usize);
        for (start, end, rep) in crate::search::replacements(&q, &text, &repl) {
            let (s, e) = if ascii {
                (start, end)
            } else {
                let s = crate::search::char_offset(&text, last, start);
                let e = s + text[start..end].chars().count();
                last = (end, e);
                (s, e)
            };
            edits.push((s as i32, e as i32, rep));
        }
        let n = edits.len();
        if n == 0 {
            self.search.info.set_text("nessun risultato");
            return;
        }
        let buf = &d.buffer;
        if n > 500 {
            let new = if q.opts.regex { q.re.replace_all(&text, repl.as_str()) } else { q.re.replace_all(&text, regex::NoExpand(&repl)) };
            d.replace_all_text(&new);
        } else {
            buf.begin_user_action();
            for (s, e, r) in edits.into_iter().rev() {
                let mut a = buf.iter_at_offset(s);
                let mut b = buf.iter_at_offset(e);
                buf.delete(&mut a, &mut b);
                buf.insert(&mut a, &r);
            }
            buf.end_user_action();
        }
        self.refresh_hits(&d, &q);
        self.message(&format!("{n} sostituzioni effettuate"));
    }
}
