//! The search and replace bar of the window.

use super::*;

/// Do not paint more search matches than this.
pub(super) const MAX_MATCH_HIGHLIGHT: usize = 20_000;

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
                && a.line() == b.line() {
                    self.search.find.set_text(&d.buffer.text(&a, &b, true));
                }
        self.search.find.grab_focus();
        self.incremental_search();
    }

    pub(super) fn hide_search(&self) {
        self.search.revealer.set_reveal_child(false);
        for d in self.docs.borrow().iter() {
            let (a, b) = d.buffer.bounds();
            d.buffer.remove_tag(&self.shared.tags.search, &a, &b);
        }
        self.search.info.set_text("");
        if let Some(d) = self.active_doc() {
            d.view.grab_focus();
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

    /// Paint every match and report how many there are.
    pub(super) fn highlight_matches(&self, d: &Rc<Doc>) -> usize {
        match self.query() {
            Some(q) => self.paint_matches(d, Some(&q), &d.text()),
            None => self.paint_matches(d, None, ""),
        }
    }

    /// `highlight_matches` with the query and the text of `d` at hand.
    pub(super) fn paint_matches(&self, d: &Rc<Doc>, q: Option<&Query>, text: &str) -> usize {
        let buf = &d.buffer;
        let tag = &self.shared.tags.search;
        crate::doc::remove_tag_where_applied(buf, tag);
        let Some(q) = q else { return 0 };
        let mut count = 0;
        let mut last = (0usize, 0usize);
        for m in q.re.find_iter(text) {
            if m.start() == m.end() {
                continue;
            }
            count += 1;
            if count <= MAX_MATCH_HIGHLIGHT {
                let s = crate::search::char_offset(text, last, m.start());
                let e = s + text[m.start()..m.end()].chars().count();
                last = (m.end(), e);
                buf.apply_tag(tag, &buf.iter_at_offset(s as i32), &buf.iter_at_offset(e as i32));
            }
        }
        self.search.info.set_tooltip_text(None);
        self.search.info.set_text(&match count {
            0 => "nessun risultato".to_string(),
            1 => "1 risultato".to_string(),
            n => format!("{n} risultati"),
        });
        count
    }

    pub(super) fn incremental_search(&self) {
        let Some(d) = self.active_doc() else { return };
        // One copy of the text and one compiled query for each key press.
        let Some(q) = self.query() else {
            self.paint_matches(&d, None, "");
            return;
        };
        let text = d.text();
        if self.paint_matches(&d, Some(&q), &text) > 0 {
            // Restart from the beginning of the current selection.
            let buf = &d.buffer;
            let start = buf.selection_bounds().map(|(a, _)| a).unwrap_or_else(|| buf.iter_at_mark(&buf.get_insert()));
            buf.place_cursor(&start);
            self.select_match(&d, &q, &text, true);
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
        self.select_match(&d, &q, &d.text(), forward);
    }

    /// `find_step` with the query and the text of `d` at hand.
    pub(super) fn select_match(&self, d: &Rc<Doc>, q: &Query, text: &str, forward: bool) {
        let buf = &d.buffer;
        let (sel_a, sel_b) = buf.selection_bounds().unwrap_or_else(|| {
            let i = buf.iter_at_mark(&buf.get_insert());
            (i, i)
        });
        let pivot_char = if forward { sel_b.offset() } else { sel_a.offset() } as usize;
        let pivot = text.char_indices().nth(pivot_char).map_or(text.len(), |(b, _)| b);
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
        match found {
            Some((s, e)) => {
                let cs = text[..s].chars().count() as i32;
                let ce = cs + text[s..e].chars().count() as i32;
                buf.select_range(&buf.iter_at_offset(cs), &buf.iter_at_offset(ce));
                d.view.scroll_to_mark(&buf.get_insert(), 0.1, false, 0.0, 0.0);
            }
            None => self.search.info.set_text("nessun risultato"),
        }
    }

    pub(super) fn replace_one(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else { return };
        let buf = &d.buffer;
        let mut text = d.text();
        if let Some((mut a, mut b)) = buf.selection_bounds() {
            let start_b = text.char_indices().nth(a.offset() as usize).map_or(text.len(), |(i, _)| i);
            let sel = buf.text(&a, &b, true);
            let end_b = start_b + sel.len();
            // The selection must be exactly a match.
            if let Some(m) = q.re.find_at(&text, start_b)
                && m.start() == start_b && m.end() == end_b {
                    let rep = q.replacement(&text, start_b, &self.search.replace.text());
                    buf.begin_user_action();
                    buf.delete(&mut a, &mut b);
                    buf.insert(&mut a, &rep);
                    buf.end_user_action();
                    text.replace_range(start_b..end_b, &rep);
                }
        }
        self.select_match(&d, &q, &text, true);
        self.paint_matches(&d, Some(&q), &text);
    }

    pub(super) fn replace_all(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else { return };
        let text = d.text();
        let repl = self.search.replace.text().to_string();
        let mut edits: Vec<(i32, i32, String)> = Vec::new();
        let mut last = (0usize, 0usize);
        for (start, end, rep) in crate::search::replacements(&q, &text, &repl) {
            let s = crate::search::char_offset(&text, last, start);
            let e = s + text[start..end].chars().count();
            last = (end, e);
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
        self.paint_matches(&d, Some(&q), &d.text());
        self.message(&format!("{n} sostituzioni effettuate"));
    }
}
