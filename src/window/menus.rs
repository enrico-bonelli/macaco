//! Menu bar, status bar, the recent files and working folders.

use super::*;

/// Menus shared by the menu bar and the status bar (a menu model can
/// be shown in several places).
pub(super) struct SharedMenus {
    pub(super) lang: gio::Menu,
    pub(super) convert: gio::Menu,
    pub(super) reopen: gio::Menu,
    pub(super) eol: gio::Menu,
}

impl SharedMenus {
    pub(super) fn new() -> SharedMenus {
        let lang = gio::Menu::new();
        for l in lang::ALL {
            lang.append(Some(l.label()), Some(&format!("win.lang::{}", l.id())));
        }
        let (convert, reopen) = (gio::Menu::new(), gio::Menu::new());
        for e in encoding::ALL {
            convert.append(Some(e.label()), Some(&format!("win.convert-enc::{}", e.id())));
            reopen.append(Some(e.label()), Some(&format!("win.reopen-enc::{}", e.id())));
        }
        let eol = gio::Menu::new();
        eol.append(Some("LF (Unix)"), Some("win.eol::lf"));
        eol.append(Some("CRLF (Windows)"), Some("win.eol::crlf"));
        SharedMenus { lang, convert, reopen, eol }
    }
}

impl Editor {
    pub(super) fn build_status_bar(menus: &SharedMenus) -> (gtk::Box, StatusUi) {
        let message = gtk::Label::new(None);
        message.set_hexpand(true);
        message.set_xalign(0.0);
        message.set_ellipsize(pango::EllipsizeMode::End);
        message.set_margin_start(8);

        let goto = gtk::Entry::new();
        goto.set_placeholder_text(Some("riga[:colonna]"));
        goto.set_width_chars(14);
        let pop = gtk::Popover::new();
        pop.set_child(Some(&goto));
        let pos = gtk::MenuButton::builder().label("Ri 1, Col 1").popover(&pop).tooltip_text("Vai alla riga (Ctrl+L)").build();

        let enc_menu = gio::Menu::new();
        enc_menu.append_section(Some("Converti in (al salvataggio)"), &menus.convert);
        enc_menu.append_section(Some("Riapri con codifica"), &menus.reopen);
        let enc = gtk::MenuButton::builder().label("UTF-8").menu_model(&enc_menu).tooltip_text("Codifica").build();

        let lang = gtk::MenuButton::builder().label("Testo").menu_model(&menus.lang).tooltip_text("Linguaggio").build();

        let eol = gtk::MenuButton::builder().label("LF").menu_model(&menus.eol).tooltip_text("Fine riga").build();

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        bar.add_css_class("statusbar");
        let diff = gtk::Label::new(None);
        diff.add_css_class("diff-info");
        diff.set_visible(false);
        diff.set_tooltip_text(Some("F9: attiva/disattiva il confronto · Alt+↓ / Alt+↑: differenza successiva / precedente"));
        let lsp = gtk::Label::new(None);
        lsp.add_css_class("dim-label");
        lsp.set_margin_start(6);
        lsp.set_margin_end(6);
        lsp.set_visible(false);
        let branch = gtk::Button::new();
        branch.add_css_class("flat");
        branch.add_css_class("git-branch");
        branch.set_action_name(Some("win.git-branches"));
        branch.set_visible(false);
        let mem = gtk::Label::new(Some("RAM –"));
        mem.add_css_class("dim-label");
        mem.set_margin_start(6);
        mem.set_margin_end(6);
        bar.append(&message);
        bar.append(&diff);
        bar.append(&lsp);
        bar.append(&branch);
        for b in [&pos, &lang, &enc, &eol] {
            b.add_css_class("flat");
            b.set_always_show_arrow(false);
            bar.append(b);
        }
        // The assistant, on demand: nothing runs until one is clicked.
        let agents: Vec<(&'static crate::acp::AgentSpec, gtk::Button)> = crate::acp::installed()
            .into_iter()
            .map(|a| {
                let b = gtk::Button::with_label(a.short);
                b.add_css_class("flat");
                b.add_css_class("agent-button");
                b.set_tooltip_text(Some(&format!("Assistente con {} (clic di nuovo per nasconderlo)", a.label)));
                b.set_action_name(Some("win.assistant-agent"));
                b.set_action_target_value(Some(&a.id.to_variant()));
                bar.append(&b);
                (a, b)
            })
            .collect();
        bar.append(&mem);
        (bar, StatusUi { message, pos, goto, lang, enc, eol, diff, branch, lsp, mem, agents })
    }

    pub(super) fn build_menu(recent: &gio::Menu, recent_folders: &gio::Menu, bookmarks: &gio::Menu, menus: &SharedMenus) -> gio::Menu {
        let bar = gio::Menu::new();

        let file = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Nuovo"), Some("win.new"));
        s1.append(Some("Apri…"), Some("win.open"));
        s1.append_submenu(Some("File recenti"), recent);
        file.append_section(None, &s1);
        let sw = gio::Menu::new();
        sw.append(Some("Apri cartella di lavoro…"), Some("win.open-folder"));
        sw.append_submenu(Some("Cartelle di lavoro recenti"), recent_folders);
        sw.append(Some("Cerca file nella cartella di lavoro…"), Some("win.quick-open"));
        sw.append(Some("Indicizza la cartella di lavoro"), Some("win.index-folder"));
        sw.append(Some("Analisi del codice con language server"), Some("win.lsp-folder"));
        sw.append(Some("Chiudi cartella di lavoro"), Some("win.close-folder"));
        file.append_section(None, &sw);
        let s2 = gio::Menu::new();
        s2.append(Some("Salva"), Some("win.save"));
        s2.append(Some("Salva come…"), Some("win.save-as"));
        s2.append(Some("Ricarica dal disco"), Some("win.reload"));
        file.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Chiudi scheda"), Some("win.close-tab"));
        s3.append(Some("Esci"), Some("win.quit"));
        file.append_section(None, &s3);
        bar.append_submenu(Some("_File"), &file);

        let edit = gio::Menu::new();
        let e1 = gio::Menu::new();
        e1.append(Some("Annulla"), Some("win.undo"));
        e1.append(Some("Ripeti"), Some("win.redo"));
        edit.append_section(None, &e1);
        let e2 = gio::Menu::new();
        e2.append(Some("Taglia"), Some("win.cut"));
        e2.append(Some("Copia"), Some("win.copy"));
        e2.append(Some("Incolla"), Some("win.paste"));
        e2.append(Some("Seleziona tutto"), Some("win.select-all"));
        edit.append_section(None, &e2);
        let em = gio::Menu::new();
        em.append(Some("Seleziona occorrenza successiva"), Some("win.select-next-occurrence"));
        em.append(Some("Seleziona tutte le occorrenze"), Some("win.select-all-occurrences"));
        em.append(Some("Torna a un solo cursore (Esc)"), Some("win.clear-cursors"));
        edit.append_section(None, &em);
        let e3 = gio::Menu::new();
        e3.append(Some("Commenta/decommenta"), Some("win.toggle-comment"));
        e3.append(Some("Aumenta rientro"), Some("win.indent"));
        e3.append(Some("Riduci rientro"), Some("win.unindent"));
        e3.append(Some("Duplica riga"), Some("win.duplicate-line"));
        e3.append(Some("Elimina riga"), Some("win.delete-line"));
        e3.append(Some("Unisci righe"), Some("win.join-lines"));
        e3.append(Some("Formatta documento"), Some("win.format"));
        e3.append(Some("Rimuovi spazi finali"), Some("win.trim"));
        edit.append_section(None, &e3);
        let e4 = gio::Menu::new();
        e4.append(Some("Preferenze…"), Some("win.preferences"));
        edit.append_section(None, &e4);
        bar.append_submenu(Some("_Modifica"), &edit);

        let find = gio::Menu::new();
        find.append(Some("Trova…"), Some("win.find"));
        find.append(Some("Sostituisci…"), Some("win.replace"));
        find.append(Some("Trova successivo"), Some("win.find-next"));
        find.append(Some("Trova precedente"), Some("win.find-prev"));
        find.append(Some("Vai alla riga…"), Some("win.goto"));
        find.append(Some("Cerca nei file della cartella di lavoro…"), Some("win.find-in-folder"));
        let code = gio::Menu::new();
        code.append(Some("Vai alla definizione"), Some("win.goto-definition"));
        code.append(Some("Trova utilizzi"), Some("win.find-usages"));
        code.append(Some("Completa"), Some("win.complete"));
        code.append(Some("Verifica il file"), Some("win.check-file"));
        find.append_section(None, &code);
        bar.append_submenu(Some("_Cerca"), &find);

        let view = gio::Menu::new();
        let v1 = gio::Menu::new();
        v1.append(Some("Affianca schede"), Some("win.split"));
        view.append_section(None, &v1);
        let vs = gio::Menu::new();
        vs.append(Some("Dividi la scheda (sopra e sotto)"), Some("win.split-view-below"));
        vs.append(Some("Dividi la scheda (affiancata)"), Some("win.split-view-side"));
        vs.append(Some("Passa all'altra vista della scheda"), Some("win.other-view"));
        view.append_section(None, &vs);
        let v1 = gio::Menu::new();
        v1.append(Some("Sposta scheda nell'altro pannello"), Some("win.move-tab"));
        v1.append(Some("Scheda successiva"), Some("win.next-tab"));
        v1.append(Some("Scheda precedente"), Some("win.prev-tab"));
        view.append_section(None, &v1);
        let vd = gio::Menu::new();
        vd.append(Some("Confronta schede affiancate (diff)"), Some("win.diff"));
        vd.append(Some("Confronta e unisci (tre colonne)…"), Some("win.merge"));
        vd.append(Some("Confronta il file corrente con…"), Some("win.merge-with-file"));
        vd.append(Some("Differenza successiva"), Some("win.diff-next"));
        vd.append(Some("Differenza precedente"), Some("win.diff-prev"));
        view.append_section(None, &vd);
        let v2 = gio::Menu::new();
        v2.append(Some("Ingrandisci testo"), Some("win.zoom-in"));
        v2.append(Some("Riduci testo"), Some("win.zoom-out"));
        v2.append(Some("Dimensione normale"), Some("win.zoom-reset"));
        view.append_section(None, &v2);
        let v3 = gio::Menu::new();
        v3.append(Some("Numeri di riga"), Some("win.line-numbers"));
        v3.append(Some("A capo automatico"), Some("win.wrap"));
        v3.append(Some("Evidenzia riga corrente"), Some("win.highlight-line"));
        v3.append(Some("Tema scuro"), Some("win.dark"));
        view.append_section(None, &v3);
        let vf = gio::Menu::new();
        vf.append(Some("Piega blocco"), Some("win.fold"));
        vf.append(Some("Espandi blocco"), Some("win.unfold"));
        vf.append(Some("Piega tutte le funzioni"), Some("win.fold-all"));
        vf.append(Some("Espandi tutto"), Some("win.unfold-all"));
        view.append_section(None, &vf);
        let v4 = gio::Menu::new();
        v4.append(Some("Pannello file"), Some("win.sidebar"));
        v4.append(Some("Terminale"), Some("win.terminal"));
        v4.append(Some("Terminale nella cartella del file"), Some("win.terminal-here"));
        v4.append(Some("Assistente (Claude Code, Codex)"), Some("win.assistant"));
        view.append_section(None, &v4);
        bar.append_submenu(Some("_Visualizza"), &view);
        bar.append_submenu(Some("_Preferiti"), bookmarks);

        bar.append_submenu(Some("_Linguaggio"), &menus.lang);

        let enc = gio::Menu::new();
        enc.append_submenu(Some("Converti in"), &menus.convert);
        enc.append_submenu(Some("Riapri con codifica"), &menus.reopen);
        enc.append_submenu(Some("Fine riga"), &menus.eol);
        bar.append_submenu(Some("C_odifica"), &enc);

        bar.append_submenu(Some("_Git"), &Self::build_git_menu());

        let help = gio::Menu::new();
        help.append(Some("Informazioni su macaco"), Some("win.about"));
        bar.append_submenu(Some("_Aiuto"), &help);
        bar
    }

    pub(super) fn add_recent(&self, path: &Path) {
        // Saving the same file again changes nothing.
        if self.recent.borrow().first().is_some_and(|p| p == path) {
            return;
        }
        config::push_recent(&mut self.recent.borrow_mut(), path);
        config::save_recent(&self.recent.borrow());
        self.rebuild_recent_menu();
    }

    pub(super) fn rebuild_recent_menu(&self) {
        let m = &self.recent_menu;
        m.remove_all();
        m.append_section(None, &crate::ui::path_menu(&self.recent.borrow(), "win.open-recent", "(nessun file recente)"));
        let clear = gio::Menu::new();
        clear.append(Some("Svuota elenco"), Some("win.clear-recent"));
        m.append_section(None, &clear);
    }

    /// Move `dir` to the front of the recent working folders.
    pub(super) fn add_recent_folder(&self, dir: &Path) {
        if self.recent_folders.borrow().first().is_some_and(|p| p == dir) {
            return;
        }
        config::push_recent(&mut self.recent_folders.borrow_mut(), dir);
        config::save_recent_folders(&self.recent_folders.borrow());
        self.rebuild_recent_folders_menu();
    }

    pub(super) fn rebuild_recent_folders_menu(&self) {
        let m = &self.recent_folders_menu;
        m.remove_all();
        m.append_section(None, &crate::ui::path_menu(&self.recent_folders.borrow(), "win.open-recent-folder", "(nessuna cartella recente)"));
        let clear = gio::Menu::new();
        clear.append(Some("Svuota elenco"), Some("win.clear-recent-folders"));
        m.append_section(None, &clear);
    }

    /// Open a recent working folder; one that no longer exists is dropped
    /// from the list.
    pub(super) fn open_recent_folder(self: &Rc<Self>, dir: &Path) {
        if dir.is_dir() {
            self.set_workdir(Some(dir.to_path_buf()));
            return;
        }
        self.recent_folders.borrow_mut().retain(|p| p != dir);
        config::save_recent_folders(&self.recent_folders.borrow());
        self.rebuild_recent_folders_menu();
        self.message(&format!("{} non esiste più: tolta dalle cartelle recenti", config::tilde(dir)));
    }

    /// A folder was renamed or moved in the file panel.
    pub(super) fn recent_folders_renamed(&self, old: &Path, new: &Path) {
        let dirs: Vec<PathBuf> =
            self.recent_folders.borrow().iter().map(|p| p.strip_prefix(old).map_or_else(|_| p.clone(), |rest| new.join(rest))).collect();
        if dirs != *self.recent_folders.borrow() {
            config::save_recent_folders(&dirs);
            *self.recent_folders.borrow_mut() = dirs;
            self.rebuild_recent_folders_menu();
        }
    }
}
