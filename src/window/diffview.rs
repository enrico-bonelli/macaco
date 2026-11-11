//! Side by side comparison of the two visible tabs.

use super::*;

impl Editor {
    pub(super) fn set_diff(self: &Rc<Self>, mut on: bool) {
        if on && !self.notebooks[1].is_visible() {
            if self.docs.borrow().len() < 2 {
                self.message("Apri almeno due schede per confrontarle");
                on = false;
            } else {
                self.toggle_split();
            }
        }
        self.diff_on.set(on);
        self.set_action_state("diff", on);
        if on {
            self.compute_diff();
        } else {
            self.clear_diff();
            self.status.diff.set_visible(false);
        }
    }

    pub(super) fn in_diff(&self, d: &Rc<Doc>) -> bool {
        self.diff_on.get() && self.diff_pair.borrow().iter().any(|w| w.upgrade().is_some_and(|x| Rc::ptr_eq(&x, d)))
    }

    pub(super) fn clear_diff(&self) {
        self.diff_gen.set(self.diff_gen.get().wrapping_add(1));
        self.clear_diff_tags();
    }

    fn clear_diff_tags(&self) {
        let tags = &self.shared.tags;
        for d in self.diff_pair.borrow_mut().drain(..).filter_map(|w| w.upgrade()) {
            for t in tags.diff_tags() {
                doc::remove_tag_where_applied(&d.buffer, t);
            }
        }
        *self.diff_result.borrow_mut() = None;
    }

    /// Recompute the diff shortly after the last change.
    pub(super) fn schedule_diff(self: &Rc<Self>) {
        if !self.diff_on.get() || self.diff_pending.replace(true) {
            return;
        }
        let ed = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            if let Some(ed) = ed.upgrade() {
                ed.diff_pending.set(false);
                if ed.diff_on.get() {
                    ed.compute_diff();
                }
            }
        });
    }

    pub(super) fn compute_diff(self: &Rc<Self>) {
        let seq = self.diff_gen.get().wrapping_add(1);
        self.diff_gen.set(seq);
        self.status.diff.set_visible(true);
        let (Some(l), Some(r)) = (self.current_doc(0), self.current_doc(1)) else {
            self.clear_diff_tags();
            self.status.diff.set_text("Diff: serve una scheda per pannello");
            return;
        };
        for d in [&l, &r] {
            if let Err(e) = d.ensure_loaded() {
                self.clear_diff_tags();
                self.message(&format!("Errore: {e}"));
                return;
            }
        }
        if self.diff_docs().is_some_and(|(ol, or)| !Rc::ptr_eq(&ol, &l) || !Rc::ptr_eq(&or, &r)) {
            self.clear_diff_tags();
        }
        let (l_gen, r_gen) = (l.edit_gen(), r.edit_gen());
        let (l_text, r_text) = (l.text(), r.text());
        let ed = Rc::downgrade(self);
        let (wl, wr) = (l.weak(), r.weak());
        glib::spawn_future_local(async move {
            let Ok(res) = gio::spawn_blocking(move || diff::compute(&l_text, &r_text)).await else { return };
            let Some(ed) = ed.upgrade() else { return };
            if !ed.diff_on.get() || ed.diff_gen.get() != seq {
                return;
            }
            let (Some(l), Some(r)) = (wl.upgrade(), wr.upgrade()) else { return };
            if ed.current_doc(0).is_none_or(|c| !Rc::ptr_eq(&c, &l)) || ed.current_doc(1).is_none_or(|c| !Rc::ptr_eq(&c, &r)) {
                return;
            }
            if l.edit_gen() != l_gen || r.edit_gen() != r_gen {
                ed.schedule_diff();
                return;
            }
            ed.apply_diff(&l, &r, res);
        });
    }

    fn apply_diff(&self, l: &Rc<Doc>, r: &Rc<Doc>, res: DiffResult) {
        self.clear_diff_tags();
        let tags = &self.shared.tags;
        let mark_lines = |d: &Doc, lines: &Range<usize>, tag: &gtk::TextTag| {
            if lines.is_empty() {
                return;
            }
            let buf = &d.buffer;
            let Some(s) = buf.iter_at_line(lines.start as i32) else { return };
            let e = if lines.end as i32 >= buf.line_count() {
                buf.end_iter()
            } else {
                buf.iter_at_line(lines.end as i32).unwrap_or_else(|| buf.end_iter())
            };
            buf.apply_tag(tag, &s, &e);
        };
        for h in &res.hunks {
            match h.kind {
                HunkKind::Added => mark_lines(r, &h.new, &tags.diff_add),
                HunkKind::Removed => mark_lines(l, &h.old, &tags.diff_del),
                HunkKind::Changed => {
                    mark_lines(l, &h.old, &tags.diff_chg);
                    mark_lines(r, &h.new, &tags.diff_chg);
                }
            }
        }
        let mark_words = |d: &Doc, spans: &[Span], tag: &gtk::TextTag| {
            for sp in spans {
                if let Some(base) = d.buffer.iter_at_line(sp.line as i32) {
                    let (mut s, mut e) = (base, base);
                    s.set_line_index(sp.start as i32);
                    e.set_line_index(sp.end as i32);
                    d.buffer.apply_tag(tag, &s, &e);
                }
            }
        };
        mark_words(l, &res.old_words, &tags.diff_word_del);
        mark_words(r, &res.new_words, &tags.diff_word_add);

        let (add, del, chg) = res.stats();
        let n = res.hunks.len();
        self.status.diff.set_text(&match n {
            0 => "Diff: nessuna differenza".to_string(),
            1 => format!("Diff: 1 differenza  +{add} −{del} ~{chg}"),
            _ => format!("Diff: {n} differenze  +{add} −{del} ~{chg}"),
        });
        *self.diff_pair.borrow_mut() = vec![l.weak(), r.weak()];
        *self.diff_result.borrow_mut() = Some(res);
    }

    pub(super) fn diff_docs(&self) -> Option<(Rc<Doc>, Rc<Doc>)> {
        let p = self.diff_pair.borrow();
        Some((p.first()?.upgrade()?, p.get(1)?.upgrade()?))
    }

    /// Move both documents to the next/previous difference.
    pub(super) fn diff_jump(&self, forward: bool) {
        if !self.diff_on.get() {
            self.message("Confronto non attivo (F9)");
            return;
        }
        let Some((l, r)) = self.diff_docs() else { return };
        let res = self.diff_result.borrow();
        let Some(res) = res.as_ref().filter(|r| !r.hunks.is_empty()) else {
            self.message("Nessuna differenza");
            return;
        };
        let on_right = self.active_doc().is_some_and(|a| Rc::ptr_eq(&a, &r));
        let doc = if on_right { &r } else { &l };
        let start_of = |h: &diff::Hunk| if on_right { h.new.start } else { h.old.start };
        let cur = doc.buffer.iter_at_mark(&doc.buffer.get_insert()).line() as usize;
        let n = res.hunks.len();
        let idx = if forward {
            res.hunks.iter().position(|h| start_of(h) > cur).unwrap_or(0)
        } else {
            res.hunks.iter().rposition(|h| start_of(h) < cur).unwrap_or(n - 1)
        };
        let h = &res.hunks[idx];
        for (d, line) in [(&l, h.old.start), (&r, h.new.start)] {
            let buf = &d.buffer;
            let line = (line as i32).min(buf.line_count() - 1);
            if let Some(it) = buf.iter_at_line(line) {
                buf.place_cursor(&it);
                d.view.scroll_to_mark(&buf.get_insert(), 0.0, true, 0.0, 0.3);
            }
        }
        doc.view.grab_focus();
        self.message(&format!("Differenza {} di {n}", idx + 1));
    }

    /// Keep the other compared view aligned with `src`.
    pub(super) fn sync_scroll(&self, src: &Rc<Doc>) {
        if !self.diff_on.get() || self.syncing.get() {
            return;
        }
        let Some((l, r)) = self.diff_docs() else { return };
        let from_old = Rc::ptr_eq(src, &l);
        if !from_old && !Rc::ptr_eq(src, &r) {
            return;
        }
        let dst = if from_old { &r } else { &l };
        let res = self.diff_result.borrow();
        let Some(res) = res.as_ref() else { return };
        // The adjustment value is the buffer y of the top of the view; the
        // visible rect is only updated at the next frame.
        let y = src.scroller.vadjustment().value() as i32;
        let (it, ly) = src.view.line_at_y(y.max(0));
        let lh = src.view.line_yrange(&it).1;
        let frac = (y - ly) as f64 / lh.max(1) as f64;
        let max_line = (dst.buffer.line_count() - 1).max(0) as usize;
        let other = res.map_line(it.line() as usize, from_old).min(max_line);
        let Some(oit) = dst.buffer.iter_at_line(other as i32) else { return };
        let (oy, oh) = dst.view.line_yrange(&oit);
        self.syncing.set(true);
        dst.scroller.vadjustment().set_value(oy as f64 + frac * oh as f64);
        self.syncing.set(false);
    }
}
