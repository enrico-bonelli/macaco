//! Settings, fonts, colors and the preferences dialog.

use super::*;

/// Settings are written this long after the last change (zoom with the
/// wheel, spin buttons of the preferences…).
pub(super) const SETTINGS_SAVE_DELAY: Duration = Duration::from_secs(1);

/// Style rules that do not depend on the settings.
pub(super) const STATIC_CSS: &str = "\
button.tab-close { min-width: 16px; min-height: 16px; padding: 0 2px; margin: 0; }
notebook > header tab { padding: 2px 6px; min-height: 0; }
.statusbar { padding: 0 4px; border-top: 1px solid alpha(currentColor, 0.12); }
.statusbar button { padding: 0 8px; min-height: 22px; font-size: 0.9em; }
.statusbar label { font-size: 0.9em; }
.diff-info { margin: 0 8px; font-weight: bold; }
.searchbar-box { padding: 6px; border-top: 1px solid alpha(currentColor, 0.12); }
.terminal-panel vte-terminal { padding: 2px 4px; }
.sidebar-header { padding: 2px 2px 2px 6px; border-bottom: 1px solid alpha(currentColor, 0.12); }
.sidebar-header label.workdir { font-weight: bold; }
.git-mark { font-weight: bold; margin-right: 4px; }
.disk-bar { padding: 6px 10px; }
.problems-panel { padding: 4px 6px; border-top: 1px solid alpha(currentColor, 0.12); }
.problems-title { font-weight: bold; }
.problems-list { background: transparent; }
.problems-list row { padding: 2px 4px; }
.problem-error, .problem-warning, .problem-info { font-weight: bold; }
.problem-pos { font-family: monospace; opacity: 0.75; }
paned > separator { background: alpha(currentColor, 0.15); }
.agent-panel { min-width: 240px; }
.agent-user { background: alpha(currentColor, 0.07); border-radius: 8px; padding: 6px 10px; }
.agent-thought { opacity: 0.65; font-style: italic; }
.agent-notice { opacity: 0.85; }
.agent-tool { border: 1px solid alpha(currentColor, 0.14); border-radius: 8px; padding: 6px 8px; }
.agent-permission { border-color: alpha(currentColor, 0.4); }
button.agent-link { padding: 0 2px; min-height: 0; }
.statusbar button.agent-button { padding: 0 6px; font-size: 0.85em; opacity: 0.75; }
.statusbar button.agent-button.agent-shown { opacity: 1; background: alpha(currentColor, 0.1); }
.statusbar button.agent-button.agent-running { font-weight: bold; opacity: 1; }
";

/// Whether the current GTK theme is dark (light foreground color).
pub(super) fn system_prefers_dark() -> bool {
    let probe = gtk::Label::new(None);
    let c = probe.color();
    0.299 * c.red() + 0.587 * c.green() + 0.114 * c.blue() > 0.5
}

impl Editor {
    pub(super) fn settings_changed(self: &Rc<Self>) {
        self.apply_font();
        self.apply_theme();
        for d in self.docs.borrow().iter() {
            d.apply_settings();
        }
        let s = self.shared.settings.borrow();
        if let Some(t) = self.terminal.borrow().as_ref() {
            t.apply_style(&self.shared.font.borrow(), s.dark, s.bell);
        }
        for (name, v) in [("line-numbers", s.line_numbers), ("wrap", s.wrap), ("highlight-line", s.highlight_line), ("dark", s.dark)] {
            self.set_action_state(name, v);
        }
        drop(s);
        // Written once after a burst of changes (and always at shutdown).
        if !self.settings_save_pending.replace(true) {
            let ed = Rc::downgrade(self);
            glib::timeout_add_local_once(SETTINGS_SAVE_DELAY, move || {
                if let Some(ed) = ed.upgrade()
                    && ed.settings_save_pending.replace(false)
                {
                    ed.shared.settings.borrow().save();
                }
            });
        }
    }

    pub(super) fn apply_font(&self) {
        let (family, size, dark) = {
            let s = self.shared.settings.borrow();
            (s.font_family.clone(), s.font_size, s.dark)
        };
        // The editor area uses its own colors so that the syntax palette
        // stays readable whatever the system GTK theme is.
        let (bg, fg, sel) = if dark { ("#1e2127", "#abb2bf", "#3e4f6a") } else { ("#ffffff", "#24292e", "#b6d2f5") };
        let (git_new, git_staged, git_mod, git_conflict) =
            if dark { ("#98c379", "#61afef", "#e5c07b", "#e06c75") } else { ("#22863a", "#0366d6", "#b08800", "#d73a49") };
        let (p_err, p_warn, p_info) = if dark { ("#ff6b6b", "#e5c07b", "#61afef") } else { ("#d32f2f", "#b08800", "#0366d6") };
        let mut desc = pango::FontDescription::new();
        desc.set_family(&family);
        desc.set_size(size as i32 * pango::SCALE);
        let layout = self.win.create_pango_layout(Some("0123456789"));
        layout.set_font_description(Some(&desc));
        self.shared.digit_width.set((layout.pixel_size().0 + 9) / 10);
        *self.shared.font.borrow_mut() = desc;
        let fam = family.replace('\\', "\\\\").replace('"', "\\\"");
        self.css.load_from_string(&format!(
            "textview.macaco {{ font-family: \"{fam}\"; font-size: {size}pt; }}\n\
             textview.macaco, textview.macaco > text {{ background-color: {bg}; color: {fg}; caret-color: {fg}; }}\n\
             textview.macaco > text > selection {{ background-color: {sel}; color: {fg}; }}\n\
             .sidebar .git-untracked label, .changes-list .git-untracked label.git-mark, button.git-branch.git-untracked {{ color: {git_new}; }}\n\
             .sidebar .git-staged label, .changes-list .git-staged label.git-mark {{ color: {git_staged}; }}\n\
             .sidebar .git-modified label, .changes-list .git-modified label.git-mark {{ color: {git_mod}; }}\n\
             .sidebar .git-conflict label, .changes-list .git-conflict label.git-mark, button.git-branch.git-conflict {{ color: {git_conflict}; }}\n\
             .disk-bar {{ background: alpha({p_warn}, 0.18); border-bottom: 1px solid alpha({p_warn}, 0.5); }}\n\
             .problem-error {{ color: {p_err}; }}\n\
             .problem-warning {{ color: {p_warn}; }}\n\
             .problem-info {{ color: {p_info}; }}"
        ));
    }

    pub(super) fn apply_theme(&self) {
        let (dark, bell, blink) = {
            let s = self.shared.settings.borrow();
            (s.dark, s.bell, s.cursor_blink)
        };
        if let Some(s) = gtk::Settings::default() {
            s.set_gtk_error_bell(bell);
            // GTK 4 fades the cursor in and out redrawing the view at every
            // frame: about 15% of a CPU core for 10 s after each key press.
            s.set_gtk_cursor_blink(blink);
            // GTK >= 4.20 replaced "prefer-dark-theme" with a color scheme.
            let scheme = s
                .find_property("gtk-interface-color-scheme")
                .and_then(|p| glib::EnumClass::with_type(p.value_type()))
                .and_then(|c| c.to_value_by_nick(if dark { "dark" } else { "light" }));
            if let Some(v) = scheme {
                s.set_property_from_value("gtk-interface-color-scheme", &v);
            } else {
                s.set_gtk_application_prefer_dark_theme(dark);
            }
        }
        self.shared.tags.set_palette(dark);
    }

    pub(super) fn zoom(self: &Rc<Self>, delta: i32) {
        {
            let mut s = self.shared.settings.borrow_mut();
            let n = (s.font_size as i32 + delta).clamp(config::MIN_FONT as i32, config::MAX_FONT as i32);
            if n as u32 == s.font_size {
                return;
            }
            s.font_size = n as u32;
        }
        self.settings_changed();
        let size = self.shared.settings.borrow().font_size;
        self.message(&format!("Dimensione carattere: {size} pt"));
    }

    pub(super) fn preferences(self: &Rc<Self>) {
        let s = self.shared.settings.borrow().clone();
        let dlg = gtk::Window::builder()
            .title("Preferenze")
            .transient_for(&self.win)
            .modal(true)
            .resizable(false)
            .build();
        let grid = gtk::Grid::builder()
            .row_spacing(10)
            .column_spacing(16)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        let label = |t: &str| {
            let l = gtk::Label::new(Some(t));
            l.set_xalign(0.0);
            l
        };

        let font_dialog = gtk::FontDialog::builder().title("Scegli il carattere").modal(true).build();
        let font_btn = gtk::FontDialogButton::new(Some(font_dialog.clone()));
        font_btn.set_level(gtk::FontLevel::Family);
        font_btn.set_use_font(true);
        let mut desc = pango::FontDescription::new();
        desc.set_family(&s.font_family);
        font_btn.set_font_desc(&desc);
        font_btn.set_hexpand(true);

        let mono_only = gtk::CheckButton::with_label("Mostra solo caratteri a spaziatura fissa");
        let apply_filter = {
            let font_dialog = font_dialog.clone();
            move |mono: bool| {
                if mono {
                    let f = gtk::CustomFilter::new(|o| o.downcast_ref::<pango::FontFamily>().is_some_and(|f| f.is_monospace()));
                    font_dialog.set_filter(Some(&f));
                } else {
                    font_dialog.set_filter(None::<&gtk::Filter>);
                }
            }
        };
        mono_only.set_active(true);
        apply_filter(true);
        mono_only.connect_toggled(move |c| apply_filter(c.is_active()));

        let size = gtk::SpinButton::with_range(config::MIN_FONT as f64, config::MAX_FONT as f64, 1.0);
        size.set_value(s.font_size as f64);
        let tab = gtk::SpinButton::with_range(1.0, 16.0, 1.0);
        tab.set_value(s.tab_width as f64);
        let switch = |v: bool| {
            let w = gtk::Switch::new();
            w.set_active(v);
            w.set_halign(gtk::Align::Start);
            w
        };
        let spaces = switch(s.insert_spaces);
        let wrap = switch(s.wrap);
        let numbers = switch(s.line_numbers);
        let curline = switch(s.highlight_line);
        let dark = switch(s.dark);
        let bell = switch(s.bell);
        let blink = switch(s.cursor_blink);
        let autocomplete = switch(s.autocomplete);

        let rows: [(&str, &gtk::Widget); 12] = [
            ("Carattere", font_btn.upcast_ref()),
            ("", mono_only.upcast_ref()),
            ("Dimensione (pt)", size.upcast_ref()),
            ("Larghezza tabulazione", tab.upcast_ref()),
            ("Inserisci spazi al posto dei tab", spaces.upcast_ref()),
            ("A capo automatico", wrap.upcast_ref()),
            ("Numeri di riga", numbers.upcast_ref()),
            ("Evidenzia riga corrente", curline.upcast_ref()),
            ("Tema scuro", dark.upcast_ref()),
            ("Avviso sonoro (terminale, tasti non validi)", bell.upcast_ref()),
            ("Cursore lampeggiante (usa più CPU)", blink.upcast_ref()),
            ("Completamento automatico durante la digitazione", autocomplete.upcast_ref()),
        ];
        for (i, (t, w)) in rows.iter().enumerate() {
            grid.attach(&label(t), 0, i as i32, 1, 1);
            grid.attach(*w, 1, i as i32, 1, 1);
        }
        let note = label("Le preferenze sono salvate in ~/.macaco/config/");
        note.add_css_class("dim-label");
        grid.attach(&note, 0, rows.len() as i32, 2, 1);
        dlg.set_child(Some(&grid));

        let ed = self.clone();
        font_btn.connect_font_desc_notify(move |b| {
            if let Some(fam) = b.font_desc().and_then(|d| d.family()) {
                ed.shared.settings.borrow_mut().font_family = fam.to_string();
                ed.settings_changed();
            }
        });
        let ed = self.clone();
        size.connect_value_changed(move |b| {
            ed.shared.settings.borrow_mut().font_size = b.value() as u32;
            ed.settings_changed();
        });
        let ed = self.clone();
        tab.connect_value_changed(move |b| {
            ed.shared.settings.borrow_mut().tab_width = b.value() as u32;
            ed.settings_changed();
        });
        let bind = |sw: &gtk::Switch, f: fn(&mut Settings, bool)| {
            let ed = self.clone();
            sw.connect_active_notify(move |sw| {
                f(&mut ed.shared.settings.borrow_mut(), sw.is_active());
                ed.settings_changed();
            });
        };
        bind(&spaces, |s, v| s.insert_spaces = v);
        bind(&wrap, |s, v| s.wrap = v);
        bind(&numbers, |s, v| s.line_numbers = v);
        bind(&curline, |s, v| s.highlight_line = v);
        bind(&dark, |s, v| s.dark = v);
        bind(&bell, |s, v| s.bell = v);
        bind(&blink, |s, v| s.cursor_blink = v);
        bind(&autocomplete, |s, v| s.autocomplete = v);

        crate::ui::close_on_escape(&dlg);
        dlg.present();
    }
}
