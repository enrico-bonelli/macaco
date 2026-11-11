//! Line editing: comments, duplicate/delete/join lines, formatting.

use super::*;

/// An external formatter is stopped after this long.
pub(super) const FORMAT_TIMEOUT: Duration = Duration::from_secs(20);

impl Editor {
    /// Selected line range as (start iter at line start, end iter at line end).
    pub(super) fn line_block(d: &Doc) -> (gtk::TextIter, gtk::TextIter) {
        let buf = &d.buffer;
        let (mut a, mut b) = buf.selection_bounds().unwrap_or_else(|| {
            let i = buf.iter_at_mark(&buf.get_insert());
            (i, i)
        });
        if b.line() > a.line() && b.line_offset() == 0 {
            b.backward_char();
        }
        a.set_line_offset(0);
        if !b.ends_line() {
            b.forward_to_line_end();
        }
        (a, b)
    }

    /// The active document if its language supports folding.
    pub(super) fn foldable_doc(&self) -> Option<Rc<Doc>> {
        let d = self.active_doc()?;
        if crate::fold::supported(d.lang.get()) {
            Some(d)
        } else {
            self.message(&format!("Piegatura del codice non disponibile per «{}»", d.lang.get().label()));
            None
        }
    }

    /// Language at `at`: in HTML and Vue the one of the script or style
    /// block around it.
    pub(super) fn lang_at(d: &Doc, at: &gtk::TextIter) -> Lang {
        match d.lang.get() {
            l @ (Lang::Html | Lang::Vue) => match d.state_at_line_start(at.line() as usize) {
                Some(state) => {
                    let mut start = *at;
                    start.set_line_offset(0);
                    crate::lexer::embedded_lang_at(l, state, &d.buffer.text(&start, at, true))
                }
                None => format::embedded_lang(l, &d.buffer.text(&d.buffer.start_iter(), at, true)),
            },
            l => l,
        }
    }

    pub(super) fn toggle_comment(&self) {
        let Some(d) = self.active_doc() else { return };
        let (mut a, mut b) = Self::line_block(&d);
        let text = d.buffer.text(&a, &b, true);
        let lines: Vec<&str> = text.split('\n').collect();
        let lang = Self::lang_at(&d, &a);
        // Inside JSX markup (React) comments are `{/* … */}`.
        let jsx = matches!(lang, Lang::JavaScript | Lang::Tsx)
            && match d.state_at_line_start(a.line() as usize).filter(|_| d.lang.get() == lang) {
                Some(state) => crate::lexer::is_jsx_text(state),
                None => crate::lexer::in_jsx_markup(lang, &d.buffer.text(&d.buffer.start_iter(), &a, true)),
            };
        let new = if jsx { Some(format::toggle_comment_with("{/*", "*/}", &lines)) } else { format::toggle_comment(lang, &lines) };
        let Some(new) = new else {
            self.message("Nessuna sintassi di commento per questo linguaggio");
            return;
        };
        let new = new.join("\n");
        let buf = &d.buffer;
        let start = a.offset();
        buf.begin_user_action();
        buf.delete(&mut a, &mut b);
        buf.insert(&mut a, &new);
        buf.end_user_action();
        let s = buf.iter_at_offset(start);
        buf.select_range(&s, &a);
    }

    pub(super) fn duplicate_line(&self) {
        let Some(d) = self.active_doc() else { return };
        let (a, mut b) = Self::line_block(&d);
        let text = d.buffer.text(&a, &b, true);
        d.buffer.begin_user_action();
        d.buffer.insert(&mut b, &format!("\n{text}"));
        d.buffer.end_user_action();
    }

    pub(super) fn delete_line(&self) {
        let Some(d) = self.active_doc() else { return };
        let (mut a, mut b) = Self::line_block(&d);
        // Remove the trailing newline, or the preceding one on the last line.
        if !b.forward_char() && a.line() > 0 {
            a.backward_char();
        }
        d.buffer.begin_user_action();
        d.buffer.delete(&mut a, &mut b);
        d.buffer.end_user_action();
    }

    /// Put the selected lines (or the cursor line and the next one) on one
    /// line.
    pub(super) fn join_lines(&self) {
        let Some(d) = self.active_doc() else { return };
        let buf = &d.buffer;
        let selected = buf.has_selection();
        let (mut a, mut b) = Self::line_block(&d);
        if a.line() == b.line() {
            // One line: join it with the next one.
            if !b.forward_line() {
                self.message("Non ci sono righe da unire");
                return;
            }
            if !b.ends_line() {
                b.forward_to_line_end();
            }
        }
        let text = buf.text(&a, &b, true);
        let lines: Vec<&str> = text.split('\n').collect();
        let lang = Self::lang_at(&d, &a);
        let joined = format::join_lines(lang, &lines);
        // Without a selection the cursor goes where the lines met.
        let meet = lines[0].trim_end().chars().count() as i32;
        let start = a.offset();
        buf.begin_user_action();
        buf.delete(&mut a, &mut b);
        buf.insert(&mut a, &joined);
        buf.end_user_action();
        let s = buf.iter_at_offset(start);
        if selected {
            buf.select_range(&s, &a);
        } else {
            buf.place_cursor(&buf.iter_at_offset(start + meet));
        }
        let n = lines.iter().skip(1).filter(|l| !l.trim().is_empty()).count() + 1;
        self.message(&if n > 1 { format!("{n} righe unite") } else { "Righe vuote rimosse".to_string() });
    }

    pub(super) fn format_doc(self: &Rc<Self>) {
        let Some(d) = self.active_doc() else { return };
        let (tab, spaces) = {
            let s = self.shared.settings.borrow();
            (s.tab_width, s.insert_spaces)
        };
        let path = d.path.borrow().clone();
        let lang = d.lang.get();
        let text = d.text();
        let Some((tool, exe, args)) = format::external_formatter(lang, path.as_deref(), tab) else {
            self.apply_format(&d, &text, Ok(format::format_internal(lang, &text, tab, spaces)));
            return;
        };
        // External tools run in the background: a slow or stuck one must
        // not freeze the editor.
        self.message(&format!("Formattazione con {tool}…"));
        let mut argv: Vec<&std::ffi::OsStr> = vec![exe.as_os_str()];
        argv.extend(args.iter().map(std::ffi::OsStr::new));
        let generation = d.edit_gen();
        let opts = crate::proc::Options { input: Some(text.clone().into_bytes()), timeout: Some(FORMAT_TIMEOUT), ..Default::default() };
        let (ed, wd) = (Rc::downgrade(self), d.weak());
        crate::proc::run(&argv, opts, move |out| {
            let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) else { return };
            if d.edit_gen() != generation {
                ed.message("Formattazione annullata: il documento è cambiato nel frattempo");
                return;
            }
            let res = if out.ok {
                String::from_utf8(out.stdout.to_vec())
                    .map(|text| format::Formatted { text, tool: tool.to_string() })
                    .map_err(|_| format!("{tool}: output non UTF-8"))
            } else {
                Err(format!("{tool}: {}", out.stderr.lines().take(3).collect::<Vec<_>>().join(" ")))
            };
            ed.apply_format(&d, &text, res);
        });
    }

    pub(super) fn apply_format(&self, d: &Doc, text: &str, res: Result<format::Formatted, String>) {
        match res {
            Ok(f) if f.text != text => {
                d.replace_all_text(&f.text);
                self.message(&format!("Documento formattato ({})", f.tool));
            }
            Ok(f) => self.message(&format!("Il documento è già formattato ({})", f.tool)),
            Err(e) => self.message(&format!("Formattazione non riuscita: {e}")),
        }
    }
}
