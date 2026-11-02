//! Scripted test runs, in debug builds only:
//! `MACACO_DEBUG=act1,act2:param,…,shot:/x.png` runs window actions one
//! after another and can save screenshots.

use super::*;

impl Editor {
    pub fn debug_script(self: &Rc<Self>) {
        let Ok(script) = std::env::var("MACACO_DEBUG") else { return };
        let steps: Vec<String> = script.split(',').map(str::to_string).collect();
        let ed = self.clone();
        let idx = Rc::new(Cell::new(0usize));
        glib::timeout_add_local(Duration::from_millis(400), move || {
            let i = idx.get();
            idx.set(i + 1);
            let Some(step) = steps.get(i) else {
                for m in ed.merges.borrow().iter() {
                    m.discard();
                }
                ed.win.close();
                return glib::ControlFlow::Break;
            };
            let (name, arg) = step.split_once(':').map_or((step.as_str(), None), |(a, b)| (a, Some(b)));
            let started = Instant::now();
            match name {
                "shot" => ed.debug_shot(arg.unwrap_or("/tmp/macaco.png")),
                "type" => {
                    if let Some(d) = ed.active_doc() {
                        d.buffer.insert_at_cursor(&arg.unwrap_or("").replace("\\n", "\n"));
                    }
                }
                "find" => ed.search.find.set_text(arg.unwrap_or("")),
                "scroll" => {
                    if let Some(d) = ed.active_doc() {
                        d.scroller.vadjustment().set_value(arg.and_then(|a| a.parse().ok()).unwrap_or(0.0));
                    }
                }
                "key" => {
                    if let (Some(d), Some(k)) = (ed.active_doc(), gdk::Key::from_name(arg.unwrap_or("")))
                        && !d.key_hook.borrow().as_ref().is_some_and(|h| h(k, gdk::ModifierType::empty()))
                        && !d.handle_key(k, gdk::ModifierType::empty())
                        && let Some(c) = k.to_unicode()
                    {
                        d.buffer.insert_at_cursor(&c.to_string());
                    }
                }
                "text" => {
                    if let Some(d) = ed.active_doc() {
                        eprintln!("TEXT {:?}", d.text());
                    }
                }
                "replace-with" => ed.search.replace.set_text(arg.unwrap_or("")),
                "side" => {
                    let (action, path) = arg.unwrap_or("").split_once('@').map_or((arg.unwrap_or(""), None), |(a, p)| (a, Some(PathBuf::from(p))));
                    if let Some(s) = ed.sidebar.borrow().clone() {
                        s.debug_action(action, path);
                    }
                }
                // Fill the entry / press a button of the open dialogs.
                "entry" | "filter" | "click" | "select" | "activate" => {
                    let want = arg.unwrap_or("");
                    for top in gtk::Window::list_toplevels() {
                        if top == *ed.win.upcast_ref::<gtk::Widget>() {
                            continue;
                        }
                        let mut stack = vec![top];
                        while let Some(w) = stack.pop() {
                            if name == "entry" && let Some(e) = w.downcast_ref::<gtk::Entry>() {
                                e.set_text(want);
                            } else if name == "filter" && let Some(e) = w.downcast_ref::<gtk::SearchEntry>() {
                                e.set_text(want);
                            } else if name == "entry" && let Some(t) = w.downcast_ref::<gtk::TextView>() {
                                t.buffer().set_text(want);
                            } else if name == "click" && let Some(b) = w.downcast_ref::<gtk::Button>() && b.label().as_deref() == Some(want) {
                                b.emit_clicked();
                            } else if name == "click"
                                && let Some(c) = w.downcast_ref::<gtk::CheckButton>()
                                && (c.label().is_some_and(|l| l.starts_with(want))
                                    || c.next_sibling().and_then(|l| l.next_sibling()).and_downcast::<gtk::Label>().is_some_and(|l| l.text() == want))
                            {
                                c.set_active(!c.is_active());
                            } else if (name == "select" || name == "activate")
                                && let Some(l) = w.downcast_ref::<gtk::Label>()
                                && l.text() == want
                                && let Some(row) = l.ancestor(gtk::ListBoxRow::static_type()).and_downcast::<gtk::ListBoxRow>()
                            {
                                if name == "select" {
                                    if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
                                        list.select_row(Some(&row));
                                    }
                                } else {
                                    row.activate();
                                }
                            }
                            let mut c = w.first_child();
                            while let Some(x) = c {
                                c = x.next_sibling();
                                stack.push(x);
                            }
                        }
                    }
                }
                "chg" => {
                    let (action, target) = arg.unwrap_or("").split_once('@').unwrap_or((arg.unwrap_or(""), ""));
                    if let Some(c) = ed.changes.borrow().last() {
                        let _ = WidgetExt::activate_action(&c.win, &format!("chg.{action}"), Some(&target.to_variant()));
                    }
                }
                "ts" => {
                    let (a, n) = arg.unwrap_or("").split_once('@').unwrap_or((arg.unwrap_or(""), "0"));
                    ed.debug_text_search(a, n.parse().unwrap_or(0));
                }
                // cursor:LINE.COL (from 1)
                "cursor" => {
                    if let Some(d) = ed.active_doc() {
                        let (l, c) = arg.unwrap_or("1.1").split_once('.').unwrap_or((arg.unwrap_or("1"), "1"));
                        let (l, c): (i32, i32) = (l.parse().unwrap_or(1), c.parse().unwrap_or(1));
                        if let Some(mut it) = d.buffer.iter_at_line(l - 1) {
                            it.set_line_offset(c - 1);
                            d.buffer.place_cursor(&it);
                        }
                    }
                }
                // fold-line:LINE (from 1): click on the fold handle.
                "fold-line" => {
                    if let Some(d) = ed.active_doc() {
                        d.toggle_fold_line(arg.and_then(|a| a.parse::<i32>().ok()).unwrap_or(1) - 1);
                    }
                }
                "gutter-hover" => {
                    if let Some(d) = ed.active_doc() {
                        d.debug_gutter_hover(arg != Some("off"));
                    }
                }
                "problems" => ed.debug_problems(),
                // merge-take:LINK.BLOCK[+] (+: after the block), merge-dump, merge-save:N
                "merge-take" | "merge-dump" | "merge-save" => {
                    let m = ed.merges.borrow().last().cloned();
                    if let Some(m) = m {
                        let a = arg.unwrap_or("");
                        match name {
                            "merge-take" => {
                                let after = a.ends_with('+');
                                let (k, i) = a.trim_end_matches('+').split_once('.').unwrap_or(("1", "0"));
                                m.debug_take(k.parse().unwrap_or(1), i.parse().unwrap_or(0), after);
                            }
                            "merge-save" => m.debug_save(a.parse().unwrap_or(1)),
                            _ => m.debug_dump(),
                        }
                    }
                }
                // history-page:patch|version|current
                "history-page" => {
                    if let Some(h) = ed.history.borrow().as_ref() {
                        h.debug_page(arg.unwrap_or("patch"));
                    }
                }
                "problem" => ed.jump_to_problem(arg.and_then(|a| a.parse().ok()).unwrap_or(0)),
                // sel:LINE.COL-LINE.COL[@action] (from 1): the action runs
                // at once, before the desktop can take the selection away.
                "sel" => {
                    if let Some(d) = ed.active_doc() {
                        let at = |p: &str| {
                            let (l, c) = p.split_once('.').unwrap_or((p, "1"));
                            let mut it = d.buffer.iter_at_line(l.parse::<i32>().unwrap_or(1) - 1)?;
                            it.set_line_offset(c.parse::<i32>().unwrap_or(1) - 1);
                            Some(it)
                        };
                        let (range, action) = arg.unwrap_or("").split_once('@').map_or((arg.unwrap_or(""), None), |(r, a)| (r, Some(a)));
                        let (a, b) = range.split_once('-').unwrap_or(("1", "1"));
                        if let (Some(a), Some(b)) = (at(a), at(b)) {
                            d.buffer.select_range(&a, &b);
                        }
                        if let Some(action) = action {
                            let _ = WidgetExt::activate_action(&ed.win, &format!("win.{action}"), None);
                        }
                    }
                }
                "inspect" => ed.debug_inspect(),
                "shot-popup" => ed.debug_shot_popup(arg.unwrap_or("/tmp/popup.png")),
                "qo-open" => ed.debug_quick_open(arg.and_then(|a| a.parse().ok()).unwrap_or(0)),
                "term-feed" => {
                    if let Some(t) = ed.terminal.borrow().as_ref() {
                        t.debug_feed(&arg.unwrap_or("").replace("\\n", "\r"));
                    }
                }
                // Assistant: send a message, answer the first question.
                "agent-send" => {
                    if let Some(p) = ed.assistant.borrow().clone() {
                        p.debug_send(arg.unwrap_or(""));
                    }
                }
                "agent-start" => {
                    if let Some(p) = ed.assistant.borrow().clone() {
                        p.debug_start();
                    }
                }
                "agent-info" => {
                    if let Some(p) = ed.assistant.borrow().clone() {
                        p.debug_info();
                    }
                }
                "agent-answer" => {
                    if let Some(p) = ed.assistant.borrow().clone() {
                        p.debug_answer(arg.and_then(|a| a.parse().ok()).unwrap_or(0));
                    }
                }
                "open" => {
                    let t = Instant::now();
                    ed.open_path(Path::new(arg.unwrap_or("")));
                    eprintln!("open took {:?}", t.elapsed());
                }
                "rss" => {
                    let rss = crate::proc::rss_kb("self");
                    let (docs, buffers) = doc::live::counts();
                    eprintln!("VmRSS: {rss} kB docs={docs} buffers={buffers} {}", arg.unwrap_or(""));
                }
                "dump" => {
                    for d in ed.docs.borrow().iter() {
                        eprintln!(
                            "DOC title={:?} path={:?} loaded={} unsaved={} lang={:?} enc={:?} pane={:?} label={:?}",
                            d.title(), d.path.borrow(), d.is_loaded(), d.is_unsaved(), d.lang.get(), d.encoding.get(),
                            ed.pane_of(d), d.label.text()
                        );
                    }
                }
                "unload-hidden" => {
                    let visible = ed.visible_docs();
                    for d in ed.docs.borrow().iter() {
                        if !visible.iter().any(|v| Rc::ptr_eq(v, d)) {
                            d.unload();
                        }
                    }
                }
                _ => {
                    let target = arg.map(|a| a.to_variant());
                    let _ = WidgetExt::activate_action(&ed.win, &format!("win.{name}"), target.as_ref());
                }
            }
            let took = started.elapsed();
            if took > Duration::from_millis(5) {
                eprintln!("STEP {name} {took:?}");
            }
            glib::ControlFlow::Continue
        });
    }

    pub(super) fn debug_shot(&self, path: &str) {
        // Capture the most recently opened window (dialogs included).
        let win = gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Window>().ok())
            .filter(|w| w.is_visible() && w.child().is_some())
            .find(|w| w != self.win.upcast_ref::<gtk::Window>())
            .unwrap_or_else(|| self.win.clone().upcast());
        let (w, h) = (win.width(), win.height());
        let child = win.child();
        let paintable = gtk::WidgetPaintable::new(child.as_ref());
        let snap = gtk::Snapshot::new();
        // The window background is not part of the child: use the theme's.
        let fg = win.color();
        let bg = if fg.red() > 0.5 { gdk::RGBA::new(0.16, 0.17, 0.19, 1.0) } else { gdk::RGBA::new(0.96, 0.96, 0.96, 1.0) };
        snap.append_color(&bg, &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
        paintable.snapshot(&snap, w as f64, h as f64);
        let Some(node) = snap.to_node() else {
            eprintln!("screenshot: empty snapshot ({w}x{h})");
            return;
        };
        let Some(renderer) = win.native().and_then(|n| n.renderer()) else {
            eprintln!("screenshot: no renderer");
            return;
        };
        let tex = renderer.render_texture(node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
        let _ = tex.save_to_png(path);
        eprintln!("screenshot {path}");
    }
}
