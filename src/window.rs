//! Main window: menus, the two tab panes, search bar, status bar,
//! preferences and session management.

use crate::config::{self, Session, Settings, TabRef};
use crate::diff::{self, DiffResult, HunkKind, Span};
use crate::doc::{self, Doc, Shared, Tags};
use crate::encoding::{self, Encoding, Eol};
use crate::format;
use crate::git::Status;
use crate::lang::{self, Lang};
use crate::search::{Options, Query};
use crate::sidebar::{self, Sidebar};
use crate::terminal::Terminal;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

mod gitui;
mod inspect;
mod lspui;
mod quickopen;
mod textsearch;

pub const APP_ID: &str = "io.github.macaco.Editor";

/// Hidden documents are unloaded from memory after this delay.
const UNLOAD_AFTER: Duration = Duration::from_secs(30);
/// Delay between an edit and writing unsaved content to the cache.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(1500);
/// Do not paint more search matches than this.
const MAX_MATCH_HIGHLIGHT: usize = 20_000;

struct SearchUi {
    revealer: gtk::Revealer,
    find: gtk::SearchEntry,
    replace: gtk::Entry,
    replace_row: gtk::Box,
    case: gtk::ToggleButton,
    word: gtk::ToggleButton,
    regex: gtk::ToggleButton,
    info: gtk::Label,
}

struct StatusUi {
    message: gtk::Label,
    pos: gtk::MenuButton,
    goto: gtk::Entry,
    lang: gtk::MenuButton,
    enc: gtk::MenuButton,
    eol: gtk::MenuButton,
    diff: gtk::Label,
    /// Current git branch; opens the branch dialog.
    branch: gtk::Button,
    /// Language servers running.
    lsp: gtk::Label,
    mem: gtk::Label,
}

pub struct Editor {
    app: gtk::Application,
    win: gtk::ApplicationWindow,
    shared: Rc<Shared>,
    css: gtk::CssProvider,
    notebooks: [gtk::Notebook; 2],
    paned: gtk::Paned,
    docs: RefCell<Vec<Rc<Doc>>>,
    active_pane: Cell<usize>,
    recent: RefCell<Vec<PathBuf>>,
    recent_menu: gio::Menu,
    search: SearchUi,
    status: StatusUi,
    lang_action: gio::SimpleAction,
    eol_action: gio::SimpleAction,
    layout_pending: Cell<bool>,
    autosave_pending: Cell<bool>,
    message_gen: Rc<Cell<u32>>,
    shut_down: Cell<bool>,
    diff_on: Cell<bool>,
    diff_pending: Cell<bool>,
    /// Documents currently compared (left, right) and their diff.
    diff_pair: RefCell<Vec<Weak<Doc>>>,
    diff_result: RefCell<Option<DiffResult>>,
    /// Set while scrolling one view to follow the other.
    syncing: Cell<bool>,
    restoring: Cell<bool>,
    /// Splits the tab panes (top) from the terminal panel (bottom).
    vpaned: gtk::Paned,
    /// Created the first time the panel is opened, dropped when the shell exits.
    terminal: RefCell<Option<Terminal>>,
    /// Keyboard shortcuts suspended while the terminal has the focus.
    term_accels: RefCell<Vec<(String, Vec<glib::GString>)>>,
    /// Splits the side panel (left) from the editor and terminal.
    hpaned: gtk::Paned,
    /// File panel, created the first time it is shown.
    sidebar: RefCell<Option<Rc<Sidebar>>>,
    /// Working folder: the root of the file panel.
    workdir: RefCell<Option<PathBuf>>,
    /// Status of the current git repository.
    git: RefCell<Option<Rc<Status>>>,
    /// A git command that changes the repository is running.
    git_busy: Cell<bool>,
    git_refreshing: Cell<bool>,
    git_refresh_again: Cell<bool>,
    git_refresh_pending: Cell<bool>,
    /// Monitor of the .git folder.
    git_monitor: RefCell<Option<(PathBuf, gio::FileMonitor)>>,
    /// Open "git changes" windows.
    changes: RefCell<Vec<Rc<gitui::Changes>>>,
    trim_pending: Cell<bool>,
    /// List of files of the working folder, if it is indexed.
    index: RefCell<Option<quickopen::Index>>,
    /// Open quick search window.
    quick: RefCell<Option<Rc<quickopen::QuickOpen>>>,
    /// Open "search in files" window.
    text_search: RefCell<Option<Rc<textsearch::TextSearch>>>,
    /// Definitions found in the indexed working folder.
    symbols: RefCell<Option<Rc<inspect::Symbols>>>,
    symbols_building: Cell<bool>,
    /// Completion popup.
    completion: RefCell<Option<Rc<inspect::Completion>>>,
    /// Running language servers.
    lsp_clients: RefCell<Vec<Rc<crate::lsp::Client>>>,
    /// Servers that could not start in this folder (not tried again).
    lsp_failed: RefCell<std::collections::HashSet<&'static str>>,
    /// Progress text of each server.
    lsp_status: RefCell<std::collections::HashMap<&'static str, String>>,
    /// Documents with changes not yet sent to their server.
    lsp_dirty: RefCell<std::collections::HashSet<String>>,
    lsp_flush_scheduled: Cell<bool>,
}

/// Shortcuts that keep working while the terminal has the focus; all the
/// others are passed to the shell (Ctrl+C, Ctrl+R, Ctrl+W, F-keys, …).
const TERMINAL_ACCELS: &[&str] = &[
    "win.terminal",
    "win.terminal-here",
    "win.next-tab",
    "win.prev-tab",
    "win.zoom-in",
    "win.zoom-out",
    "win.zoom-reset",
];

/// Whether the current GTK theme is dark (light foreground color).
fn system_prefers_dark() -> bool {
    let probe = gtk::Label::new(None);
    let c = probe.color();
    0.299 * c.red() + 0.587 * c.green() + 0.114 * c.blue() > 0.5
}

fn menu_item(label: &str, action: &str) -> gio::MenuItem {
    gio::MenuItem::new(Some(label), Some(action))
}

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("flat");
    b
}

fn toggle(label: &str, tip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::with_label(label);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("flat");
    b
}

impl Editor {
    pub fn new(app: &gtk::Application) -> Rc<Editor> {
        let first_run = !Settings::exists();
        let mut settings = Settings::load();
        if first_run {
            // Follow the system theme the first time.
            settings.dark = system_prefers_dark();
        }
        let tags = Tags::new(settings.dark);
        let shared = Rc::new(Shared {
            settings: RefCell::new(settings.clone()),
            tags,
            font: RefCell::new(pango::FontDescription::new()),
            digit_width: Cell::new(8),
        });

        let win = gtk::ApplicationWindow::builder()
            .application(app)
            .title("macaco")
            .default_width(settings.width)
            .default_height(settings.height)
            .build();

        let notebooks = [gtk::Notebook::new(), gtk::Notebook::new()];
        for nb in &notebooks {
            nb.set_scrollable(true);
            nb.set_group_name(Some("macaco-tabs"));
            nb.set_show_border(false);
            nb.set_hexpand(true);
            nb.set_vexpand(true);
        }
        notebooks[1].set_visible(false);
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&notebooks[0]));
        paned.set_end_child(Some(&notebooks[1]));
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_wide_handle(true);
        let vpaned = gtk::Paned::new(gtk::Orientation::Vertical);
        vpaned.set_start_child(Some(&paned));
        vpaned.set_shrink_start_child(false);
        vpaned.set_shrink_end_child(false);
        vpaned.set_resize_end_child(false);
        vpaned.set_wide_handle(true);
        vpaned.set_vexpand(true);
        let hpaned = gtk::Paned::new(gtk::Orientation::Horizontal);
        hpaned.set_end_child(Some(&vpaned));
        hpaned.set_shrink_start_child(false);
        hpaned.set_shrink_end_child(false);
        hpaned.set_resize_start_child(false);
        hpaned.set_wide_handle(true);
        hpaned.set_vexpand(true);

        let (search_box, search) = Self::build_search_bar();
        let (status_box, status) = Self::build_status_bar();

        let recent_menu = gio::Menu::new();
        let menubar = gtk::PopoverMenuBar::from_model(Some(&Self::build_menu(&recent_menu)));

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        vbox.append(&menubar);
        vbox.append(&hpaned);
        vbox.append(&search_box);
        vbox.append(&status_box);
        win.set_child(Some(&vbox));

        let lang_action = gio::SimpleAction::new_stateful("lang", Some(glib::VariantTy::STRING), &"plain".to_variant());
        let eol_action = gio::SimpleAction::new_stateful("eol", Some(glib::VariantTy::STRING), &"lf".to_variant());

        let css = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            &gtk::prelude::WidgetExt::display(&win),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let ed = Rc::new(Editor {
            app: app.clone(),
            win,
            shared,
            css,
            notebooks,
            paned,
            docs: RefCell::new(Vec::new()),
            active_pane: Cell::new(0),
            recent: RefCell::new(config::load_recent()),
            recent_menu,
            search,
            status,
            lang_action,
            eol_action,
            layout_pending: Cell::new(false),
            autosave_pending: Cell::new(false),
            message_gen: Rc::new(Cell::new(0)),
            shut_down: Cell::new(false),
            diff_on: Cell::new(false),
            diff_pending: Cell::new(false),
            diff_pair: RefCell::new(Vec::new()),
            diff_result: RefCell::new(None),
            syncing: Cell::new(false),
            restoring: Cell::new(false),
            vpaned,
            terminal: RefCell::new(None),
            term_accels: RefCell::new(Vec::new()),
            hpaned,
            sidebar: RefCell::new(None),
            workdir: RefCell::new(None),
            git: RefCell::new(None),
            git_busy: Cell::new(false),
            git_refreshing: Cell::new(false),
            git_refresh_again: Cell::new(false),
            git_refresh_pending: Cell::new(false),
            git_monitor: RefCell::new(None),
            changes: RefCell::new(Vec::new()),
            trim_pending: Cell::new(false),
            index: RefCell::new(None),
            quick: RefCell::new(None),
            text_search: RefCell::new(None),
            symbols: RefCell::new(None),
            symbols_building: Cell::new(false),
            completion: RefCell::new(None),
            lsp_clients: RefCell::new(Vec::new()),
            lsp_failed: RefCell::new(Default::default()),
            lsp_status: RefCell::new(Default::default()),
            lsp_dirty: RefCell::new(Default::default()),
            lsp_flush_scheduled: Cell::new(false),
        });
        ed.apply_font();
        ed.apply_theme();
        ed.setup_actions();
        ed.connect_ui();
        ed.rebuild_recent_menu();
        ed.restore_session();
        if ed.shared.settings.borrow().sidebar || ed.workdir.borrow().is_some() {
            ed.set_sidebar(true);
        }
        ed.sync_index();
        ed.sync_lsp();
        ed.git_refresh();
        ed.start_timers();
        ed
    }

    // ------------------------------------------------------------- widgets

    fn build_search_bar() -> (gtk::Revealer, SearchUi) {
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

    fn build_status_bar() -> (gtk::Box, StatusUi) {
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
        let reopen = gio::Menu::new();
        let convert = gio::Menu::new();
        for e in encoding::ALL {
            reopen.append(Some(e.label()), Some(&format!("win.reopen-enc::{}", e.id())));
            convert.append(Some(e.label()), Some(&format!("win.convert-enc::{}", e.id())));
        }
        enc_menu.append_section(Some("Converti in (al salvataggio)"), &convert);
        enc_menu.append_section(Some("Riapri con codifica"), &reopen);
        let enc = gtk::MenuButton::builder().label("UTF-8").menu_model(&enc_menu).tooltip_text("Codifica").build();

        let lang_menu = gio::Menu::new();
        for l in lang::ALL {
            lang_menu.append(Some(l.label()), Some(&format!("win.lang::{}", l.id())));
        }
        let lang = gtk::MenuButton::builder().label("Testo").menu_model(&lang_menu).tooltip_text("Linguaggio").build();

        let eol_menu = gio::Menu::new();
        eol_menu.append(Some("LF (Unix)"), Some("win.eol::lf"));
        eol_menu.append(Some("CRLF (Windows)"), Some("win.eol::crlf"));
        let eol = gtk::MenuButton::builder().label("LF").menu_model(&eol_menu).tooltip_text("Fine riga").build();

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
        bar.append(&mem);
        (bar, StatusUi { message, pos, goto, lang, enc, eol, diff, branch, lsp, mem })
    }

    fn build_menu(recent: &gio::Menu) -> gio::Menu {
        let bar = gio::Menu::new();

        let file = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Nuovo"), Some("win.new"));
        s1.append(Some("Apri…"), Some("win.open"));
        s1.append_submenu(Some("File recenti"), recent);
        file.append_section(None, &s1);
        let sw = gio::Menu::new();
        sw.append(Some("Apri cartella di lavoro…"), Some("win.open-folder"));
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
        let e3 = gio::Menu::new();
        e3.append(Some("Commenta/decommenta"), Some("win.toggle-comment"));
        e3.append(Some("Aumenta rientro"), Some("win.indent"));
        e3.append(Some("Riduci rientro"), Some("win.unindent"));
        e3.append(Some("Duplica riga"), Some("win.duplicate-line"));
        e3.append(Some("Elimina riga"), Some("win.delete-line"));
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
        find.append_section(None, &code);
        bar.append_submenu(Some("_Cerca"), &find);

        let view = gio::Menu::new();
        let v1 = gio::Menu::new();
        v1.append(Some("Affianca schede"), Some("win.split"));
        v1.append(Some("Sposta scheda nell'altro pannello"), Some("win.move-tab"));
        v1.append(Some("Scheda successiva"), Some("win.next-tab"));
        v1.append(Some("Scheda precedente"), Some("win.prev-tab"));
        view.append_section(None, &v1);
        let vd = gio::Menu::new();
        vd.append(Some("Confronta schede affiancate (diff)"), Some("win.diff"));
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
        let v4 = gio::Menu::new();
        v4.append(Some("Pannello file"), Some("win.sidebar"));
        v4.append(Some("Terminale"), Some("win.terminal"));
        v4.append(Some("Terminale nella cartella del file"), Some("win.terminal-here"));
        view.append_section(None, &v4);
        bar.append_submenu(Some("_Visualizza"), &view);

        let langm = gio::Menu::new();
        for l in lang::ALL {
            langm.append(Some(l.label()), Some(&format!("win.lang::{}", l.id())));
        }
        bar.append_submenu(Some("_Linguaggio"), &langm);

        let enc = gio::Menu::new();
        let conv = gio::Menu::new();
        let reopen = gio::Menu::new();
        for e in encoding::ALL {
            conv.append(Some(e.label()), Some(&format!("win.convert-enc::{}", e.id())));
            reopen.append(Some(e.label()), Some(&format!("win.reopen-enc::{}", e.id())));
        }
        enc.append_submenu(Some("Converti in"), &conv);
        enc.append_submenu(Some("Riapri con codifica"), &reopen);
        let eol = gio::Menu::new();
        eol.append(Some("LF (Unix)"), Some("win.eol::lf"));
        eol.append(Some("CRLF (Windows)"), Some("win.eol::crlf"));
        enc.append_submenu(Some("Fine riga"), &eol);
        bar.append_submenu(Some("C_odifica"), &enc);

        bar.append_submenu(Some("_Git"), &Self::build_git_menu());

        let help = gio::Menu::new();
        help.append_item(&menu_item("Informazioni su macaco", "win.about"));
        bar.append_submenu(Some("_Aiuto"), &help);
        bar
    }

    // ------------------------------------------------------------- actions

    fn setup_actions(self: &Rc<Self>) {
        let win = &self.win;
        macro_rules! act {
            ($name:expr, $accels:expr, $f:expr) => {{
                let a = gio::SimpleAction::new($name, None);
                let ed = self.clone();
                let f = $f;
                a.connect_activate(move |_, _| f(&ed));
                win.add_action(&a);
                let accels: &[&str] = &$accels;
                if !accels.is_empty() {
                    self.app.set_accels_for_action(&format!("win.{}", $name), accels);
                }
            }};
        }
        macro_rules! act_str {
            ($name:expr, $f:expr) => {{
                let a = gio::SimpleAction::new($name, Some(glib::VariantTy::STRING));
                let ed = self.clone();
                let f = $f;
                a.connect_activate(move |_, v| {
                    if let Some(s) = v.and_then(|v| v.get::<String>()) {
                        f(&ed, s);
                    }
                });
                win.add_action(&a);
            }};
        }
        macro_rules! act_bool {
            ($name:expr, $accels:expr, $get:expr, $set:expr) => {{
                let a = gio::SimpleAction::new_stateful($name, None, &$get(&self.shared.settings.borrow()).to_variant());
                let ed = self.clone();
                a.connect_activate(move |a, _| {
                    let v = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                    a.set_state(&v.to_variant());
                    $set(&mut ed.shared.settings.borrow_mut(), v);
                    ed.settings_changed();
                });
                win.add_action(&a);
                let accels: &[&str] = &$accels;
                if !accels.is_empty() {
                    self.app.set_accels_for_action(&format!("win.{}", $name), accels);
                }
            }};
        }

        act!("new", ["<Control>n", "<Control>t"], |ed: &Rc<Editor>| {
            let d = ed.new_doc();
            ed.add_doc(&d, ed.active_pane.get());
        });
        act!("open", ["<Control>o"], |ed: &Rc<Editor>| ed.open_dialog());
        act!("save", ["<Control>s"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                ed.save_doc(&d, false, None);
            }
        });
        act!("save-as", ["<Control><Shift>s"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                ed.save_doc(&d, true, None);
            }
        });
        act!("reload", [], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                let enc = d.encoding.get();
                ed.reopen_with(&d, enc);
            }
        });
        act!("close-tab", ["<Control>w"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                ed.close_doc(&d);
            }
        });
        act!("quit", ["<Control>q"], |ed: &Rc<Editor>| ed.win.close());
        act!("clear-recent", [], |ed: &Rc<Editor>| {
            ed.recent.borrow_mut().clear();
            config::save_recent(&[]);
            ed.rebuild_recent_menu();
        });
        act_str!("open-recent", |ed: &Rc<Editor>, p: String| ed.open_path(Path::new(&p)));

        // Clipboard and undo act on whatever widget has the focus (the
        // editor or one of the search entries).
        for (name, target, accels) in [
            ("undo", "text.undo", ["<Control>z", ""]),
            ("redo", "text.redo", ["<Control><Shift>z", "<Control>y"]),
            ("cut", "clipboard.cut", ["<Control>x", ""]),
            ("copy", "clipboard.copy", ["<Control>c", ""]),
            ("paste", "clipboard.paste", ["<Control>v", ""]),
            ("select-all", "selection.select-all", ["<Control>a", ""]),
        ] {
            let accels: Vec<&str> = accels.into_iter().filter(|a| !a.is_empty()).collect();
            let a = gio::SimpleAction::new(name, None);
            let w = self.win.downgrade();
            a.connect_activate(move |_, _| {
                if let Some(focus) = w.upgrade().and_then(|w| gtk::prelude::GtkWindowExt::focus(&w)) {
                    let _ = focus.activate_action(target, None);
                }
            });
            win.add_action(&a);
            self.app.set_accels_for_action(&format!("win.{name}"), &accels);
        }

        act!("toggle-comment", ["<Control>slash", "<Control>KP_Divide"], |ed: &Rc<Editor>| ed.toggle_comment());
        act!("indent", ["<Control>bracketright"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                d.indent_lines(true);
            }
        });
        act!("unindent", ["<Control>bracketleft"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                d.indent_lines(false);
            }
        });
        act!("duplicate-line", ["<Control><Shift>d"], |ed: &Rc<Editor>| ed.duplicate_line());
        act!("delete-line", ["<Control><Shift>k"], |ed: &Rc<Editor>| ed.delete_line());
        act!("format", ["<Control><Shift>i"], |ed: &Rc<Editor>| ed.format_doc());
        act!("trim", [], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                let text = d.text();
                let t = format::tidy(&text, None);
                if t != text {
                    d.replace_all_text(&t);
                }
                ed.message("Spazi finali rimossi");
            }
        });
        act!("preferences", ["<Control>comma"], |ed: &Rc<Editor>| ed.preferences());

        act!("find", ["<Control>f"], |ed: &Rc<Editor>| ed.show_search(false));
        act!("replace", ["<Control>h", "<Control>r"], |ed: &Rc<Editor>| ed.show_search(true));
        act!("find-next", ["F3", "<Control>g"], |ed: &Rc<Editor>| ed.find_step(true));
        act!("find-prev", ["<Shift>F3", "<Control><Shift>g"], |ed: &Rc<Editor>| ed.find_step(false));
        act!("search-close", [], |ed: &Rc<Editor>| ed.hide_search());
        act!("replace-one", [], |ed: &Rc<Editor>| ed.replace_one());
        act!("replace-all", [], |ed: &Rc<Editor>| ed.replace_all());
        act!("goto", ["<Control>l"], |ed: &Rc<Editor>| {
            ed.status.pos.popup();
            ed.status.goto.grab_focus();
        });

        act!("split", ["<Control>backslash", "F8"], |ed: &Rc<Editor>| ed.toggle_split());
        {
            let a = gio::SimpleAction::new_stateful("diff", None, &false.to_variant());
            let ed = self.clone();
            a.connect_activate(move |_, _| ed.set_diff(!ed.diff_on.get()));
            win.add_action(&a);
            self.app.set_accels_for_action("win.diff", &["F9"]);
        }
        {
            let a = gio::SimpleAction::new_stateful("terminal", None, &false.to_variant());
            let ed = self.clone();
            a.connect_activate(move |_, _| ed.set_terminal(!ed.terminal_visible()));
            win.add_action(&a);
            self.app.set_accels_for_action("win.terminal", &["F12", "<Control>grave"]);
        }
        act!("terminal-here", ["<Shift>F12"], |ed: &Rc<Editor>| ed.terminal_at(ed.doc_dir()));
        {
            let a = gio::SimpleAction::new_stateful("sidebar", None, &false.to_variant());
            let ed = self.clone();
            a.connect_activate(move |_, _| ed.set_sidebar(!ed.sidebar_visible()));
            win.add_action(&a);
            self.app.set_accels_for_action("win.sidebar", &["<Control>b", "F7"]);
        }
        act!("open-folder", ["<Control><Shift>o"], |ed: &Rc<Editor>| ed.open_folder_dialog());
        act!("close-folder", [], |ed: &Rc<Editor>| ed.close_workdir());
        act!("diff-next", ["<Alt>Down"], |ed: &Rc<Editor>| ed.diff_jump(true));
        act!("diff-prev", ["<Alt>Up"], |ed: &Rc<Editor>| ed.diff_jump(false));
        act!("move-tab", ["<Control><Shift>m"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                ed.move_to_other_pane(&d);
            }
        });
        act!("next-tab", ["<Control>Tab", "<Control>Page_Down"], |ed: &Rc<Editor>| ed.cycle_tab(1));
        act!("prev-tab", ["<Control><Shift>ISO_Left_Tab", "<Control>Page_Up"], |ed: &Rc<Editor>| ed.cycle_tab(-1));
        act!("zoom-in", ["<Control>plus", "<Control>equal", "<Control>KP_Add"], |ed: &Rc<Editor>| ed.zoom(1));
        act!("zoom-out", ["<Control>minus", "<Control>KP_Subtract"], |ed: &Rc<Editor>| ed.zoom(-1));
        act!("zoom-reset", ["<Control>0", "<Control>KP_0"], |ed: &Rc<Editor>| {
            ed.shared.settings.borrow_mut().font_size = Settings::default().font_size;
            ed.settings_changed();
        });
        act_bool!("line-numbers", [], |s: &Settings| s.line_numbers, |s: &mut Settings, v| s.line_numbers = v);
        act_bool!("wrap", ["<Alt>z"], |s: &Settings| s.wrap, |s: &mut Settings, v| s.wrap = v);
        act_bool!("highlight-line", [], |s: &Settings| s.highlight_line, |s: &mut Settings, v| s.highlight_line = v);
        act_bool!("dark", [], |s: &Settings| s.dark, |s: &mut Settings, v| s.dark = v);

        {
            let ed = self.clone();
            self.lang_action.connect_activate(move |a, v| {
                let Some(id) = v.and_then(|v| v.get::<String>()) else { return };
                let Some(l) = Lang::from_id(&id) else { return };
                a.set_state(&id.to_variant());
                if let Some(d) = ed.active_doc() {
                    d.set_lang(l);
                    ed.update_status();
                }
            });
            win.add_action(&self.lang_action);
            let ed = self.clone();
            self.eol_action.connect_activate(move |a, v| {
                let Some(id) = v.and_then(|v| v.get::<String>()) else { return };
                let Some(e) = Eol::from_id(&id) else { return };
                a.set_state(&id.to_variant());
                if let Some(d) = ed.active_doc() {
                    if d.eol.get() != e {
                        d.eol.set(e);
                        d.buffer.set_modified(true);
                        ed.message(&format!("Fine riga impostato a {} (applicato al salvataggio)", e.label()));
                    }
                    ed.update_status();
                }
            });
            win.add_action(&self.eol_action);
        }
        act_str!("convert-enc", |ed: &Rc<Editor>, id: String| {
            if let (Some(d), Some(e)) = (ed.active_doc(), Encoding::from_id(&id)) {
                ed.convert_encoding(&d, e);
            }
        });
        act_str!("reopen-enc", |ed: &Rc<Editor>, id: String| {
            if let (Some(d), Some(e)) = (ed.active_doc(), Encoding::from_id(&id)) {
                ed.reopen_with(&d, e);
            }
        });
        act!("about", [], |ed: &Rc<Editor>| {
            gtk::AboutDialog::builder()
                .transient_for(&ed.win)
                .modal(true)
                .program_name("macaco")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Editor di testo leggero e minimale.\nGTK 4 + Rust.")
                .logo_icon_name(APP_ID)
                .build()
                .present();
        });
        self.setup_git_actions();
        self.setup_quickopen_actions();
        self.setup_textsearch_actions();
        self.setup_inspect_actions();
        self.setup_lsp_actions();
    }

    fn connect_ui(self: &Rc<Self>) {
        // Git may have been used elsewhere in the meantime.
        let ed = Rc::downgrade(self);
        self.win.connect_is_active_notify(move |w| {
            if let (true, Some(ed)) = (w.is_active(), ed.upgrade()) {
                ed.git_refresh();
            }
        });
        for (pane, nb) in self.notebooks.iter().enumerate() {
            let ed = self.clone();
            nb.connect_switch_page(move |_, page, _| {
                ed.active_pane.set(pane);
                if let Some(d) = ed.doc_for_widget(page) {
                    if let Err(e) = d.ensure_loaded() {
                        ed.message(&format!("Errore: {e}"));
                    }
                    d.hidden_since.set(None);
                    ed.update_status_for(&d);
                    ed.sync_sidebar();
                    ed.git_refresh();
                    ed.hide_completion();
                    ed.lsp_open(&d);
                    if ed.search.revealer.reveals_child() {
                        ed.highlight_matches(&d);
                    }
                    ed.schedule_diff();
                }
            });
            let ed = self.clone();
            nb.connect_page_added(move |nb, child, _| {
                nb.set_tab_reorderable(child, true);
                nb.set_tab_detachable(child, true);
                ed.schedule_layout();
                ed.schedule_autosave();
            });
            let ed = self.clone();
            nb.connect_page_removed(move |_, _, _| {
                ed.schedule_layout();
                ed.schedule_autosave();
                // Drag & drop may have emptied a pane.
                let ed2 = ed.clone();
                glib::idle_add_local_once(move || ed2.check_panes());
            });
            let ed = self.clone();
            nb.connect_page_reordered(move |_, _, _| ed.schedule_autosave());
            // No detaching into new windows.
            nb.connect_create_window(|_, _| None);
        }

        // Window size changes alter the space available to tab labels.
        let ed = self.clone();
        self.win.connect_default_width_notify(move |_| ed.schedule_layout());
        let ed = self.clone();
        self.win.connect_maximized_notify(move |_| ed.schedule_layout());

        let ed = self.clone();
        self.win.connect_close_request(move |_| {
            ed.shutdown();
            glib::Propagation::Proceed
        });

        // Search bar.
        let ed = self.clone();
        self.search.find.connect_search_changed(move |_| ed.incremental_search());
        let ed = self.clone();
        self.search.find.connect_activate(move |_| ed.find_step(true));
        let ed = self.clone();
        self.search.find.connect_stop_search(move |_| ed.hide_search());
        for t in [&self.search.case, &self.search.word, &self.search.regex] {
            let ed = self.clone();
            t.connect_toggled(move |_| ed.incremental_search());
        }
        let ed = self.clone();
        self.search.replace.connect_activate(move |_| ed.replace_one());
        let esc = gtk::EventControllerKey::new();
        let ed = self.clone();
        esc.connect_key_pressed(move |_, key, _, state| {
            if key == gdk::Key::Escape {
                ed.hide_search();
                return glib::Propagation::Stop;
            }
            if (key == gdk::Key::Return || key == gdk::Key::KP_Enter) && state.contains(gdk::ModifierType::SHIFT_MASK) {
                ed.find_step(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.search.revealer.add_controller(esc);

        // Go to line.
        let ed = self.clone();
        self.status.goto.connect_activate(move |e| {
            let t = e.text();
            let mut parts = t.split(':');
            let line: i32 = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let col: i32 = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
            ed.status.pos.popdown();
            e.set_text("");
            if let Some(d) = ed.active_doc() {
                if line > 0 {
                    let buf = &d.buffer;
                    let mut it = buf.iter_at_line(line - 1).unwrap_or_else(|| buf.end_iter());
                    let mut end = it;
                    if !end.ends_line() {
                        end.forward_to_line_end();
                    }
                    it.set_line_offset((col - 1).clamp(0, end.line_offset()));
                    buf.place_cursor(&it);
                    d.view.scroll_to_mark(&buf.get_insert(), 0.1, true, 0.0, 0.4);
                }
                d.view.grab_focus();
            }
        });

        // Drop files to open them.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let ed = self.clone();
        drop.connect_drop(move |_, value, _, _| {
            if let Ok(list) = value.get::<gdk::FileList>() {
                for f in list.files() {
                    if let Some(p) = f.path() {
                        ed.open_path(&p);
                    }
                }
                return true;
            }
            false
        });
        self.win.add_controller(drop);
    }

    fn start_timers(self: &Rc<Self>) {
        // Memory indicator in the status bar.
        self.update_memory();
        let ed = Rc::downgrade(self);
        glib::timeout_add_seconds_local(2, move || {
            let Some(ed) = ed.upgrade() else { return glib::ControlFlow::Break };
            ed.update_memory();
            glib::ControlFlow::Continue
        });

        // Unload documents that have been hidden for a while.
        let ed = Rc::downgrade(self);
        glib::timeout_add_seconds_local(10, move || {
            let Some(ed) = ed.upgrade() else { return glib::ControlFlow::Break };
            let now = Instant::now();
            let visible = ed.visible_docs();
            for d in ed.docs.borrow().iter() {
                if visible.iter().any(|v| Rc::ptr_eq(v, d)) {
                    d.hidden_since.set(None);
                } else if d.is_loaded() {
                    match d.hidden_since.get() {
                        None => d.hidden_since.set(Some(now)),
                        Some(t) if now.duration_since(t) >= UNLOAD_AFTER && d.unload() => ed.release_memory(),
                        _ => {}
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // ---------------------------------------------------------- documents

    fn new_doc(self: &Rc<Self>) -> Rc<Doc> {
        let d = Doc::new(&self.shared, config::new_id());
        self.wire_doc(&d);
        self.docs.borrow_mut().push(d.clone());
        d
    }

    /// Window-level signal handlers of a document.
    fn wire_doc(self: &Rc<Self>, d: &Rc<Doc>) {
        self.wire_inspect(d);
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.close_button.connect_clicked(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) {
                ed.close_doc(&d);
            }
        });
        let middle = gtk::GestureClick::builder().button(2).build();
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        middle.connect_released(move |_, _, _, _| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) {
                ed.close_doc(&d);
            }
        });
        d.tab.add_controller(middle);

        let focus = gtk::EventControllerFocus::new();
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        focus.connect_enter(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) {
                if let Some(p) = ed.pane_of(&d) {
                    ed.active_pane.set(p);
                }
                ed.update_status_for(&d);
            }
        });
        d.view.add_controller(focus);

        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.buffer.connect_changed(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade())
                && d.is_loaded()
            {
                ed.schedule_autosave();
                if ed.in_diff(&d) {
                    ed.schedule_diff();
                }
            }
        });
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.buffer.connect_cursor_position_notify(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade())
                && ed.is_active(&d) {
                    ed.update_pos(&d);
                }
        });
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.buffer.connect_modified_changed(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade())
                && ed.is_active(&d) {
                    ed.update_title(&d);
                }
        });

        // Diff mode: the other view follows the scrolling.
        let ed = Rc::downgrade(self);
        let wd = d.weak();
        d.scroller.vadjustment().connect_value_changed(move |_| {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) {
                ed.sync_scroll(&d);
            }
        });

        // Ctrl + mouse wheel zooms.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ed = Rc::downgrade(self);
        scroll.connect_scroll(move |c, _, dy| {
            if c.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                if let Some(ed) = ed.upgrade() {
                    ed.zoom(if dy < 0.0 { 1 } else { -1 });
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        d.view.add_controller(scroll);
    }

    fn add_doc(self: &Rc<Self>, d: &Rc<Doc>, pane: usize) {
        let pane = if self.notebooks[1].is_visible() { pane } else { 0 };
        let nb = &self.notebooks[pane];
        let idx = nb.append_page(&d.scroller, Some(&d.tab));
        nb.set_current_page(Some(idx));
        if !self.restoring.get() {
            d.view.grab_focus();
        }
    }

    fn doc_for_widget(&self, w: &gtk::Widget) -> Option<Rc<Doc>> {
        self.docs.borrow().iter().find(|d| d.scroller.upcast_ref::<gtk::Widget>() == w).cloned()
    }

    fn pane_of(&self, d: &Doc) -> Option<usize> {
        self.notebooks.iter().position(|nb| nb.page_num(&d.scroller).is_some())
    }

    fn current_doc(&self, pane: usize) -> Option<Rc<Doc>> {
        let nb = &self.notebooks[pane];
        nb.current_page().and_then(|i| nb.nth_page(Some(i))).and_then(|w| self.doc_for_widget(&w))
    }

    pub fn active_doc(&self) -> Option<Rc<Doc>> {
        let p = self.active_pane.get();
        self.current_doc(p).or_else(|| self.current_doc(1 - p))
    }

    fn is_active(&self, d: &Rc<Doc>) -> bool {
        self.active_doc().is_some_and(|a| Rc::ptr_eq(&a, d))
    }

    fn visible_docs(&self) -> Vec<Rc<Doc>> {
        (0..2)
            .filter(|&p| self.notebooks[p].is_visible())
            .filter_map(|p| self.current_doc(p))
            .collect()
    }

    fn focus_doc(&self, d: &Rc<Doc>) {
        if let Some(p) = self.pane_of(d) {
            let nb = &self.notebooks[p];
            if let Some(i) = nb.page_num(&d.scroller) {
                nb.set_current_page(Some(i));
            }
            self.active_pane.set(p);
            d.view.grab_focus();
        }
    }

    /// Open a file (or focus it if it is already open).
    pub fn open_path(self: &Rc<Self>, path: &Path) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| {
            if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) }
        });
        if path.is_dir() {
            self.set_workdir(Some(path));
            return;
        }
        let existing = self.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path.as_path())).cloned();
        if let Some(d) = existing {
            self.focus_doc(&d);
            return;
        }
        let (text, enc, eol) = if path.exists() {
            match doc::read_file(&path, None) {
                Ok(r) => r,
                Err(e) => {
                    self.error_dialog("Impossibile aprire il file", &format!("{}: {e}", path.display()));
                    return;
                }
            }
        } else {
            (String::new(), Encoding::Utf8, Eol::Lf)
        };
        // Reuse an empty untitled tab.
        let reuse = self.active_doc().filter(|d| d.is_loaded() && d.is_pristine() && !d.buffer.is_modified());
        let d = reuse.clone().unwrap_or_else(|| self.new_doc());
        *d.path.borrow_mut() = Some(path.clone());
        d.encoding.set(enc);
        d.eol.set(eol);
        let head: String = text.chars().take(256).collect();
        d.lang.set(lang::detect(Some(&path), &head));
        d.set_content(&text, false, 0);
        if reuse.is_none() {
            self.add_doc(&d, self.active_pane.get());
        } else {
            d.view.grab_focus();
        }
        self.add_recent(&path);
        self.update_status_for(&d);
        self.sync_sidebar();
        self.lsp_open(&d);
        if !path.exists() {
            self.message(&format!("Nuovo file: {}", path.display()));
        }
    }

    fn open_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title("Apri file").modal(true).build();
        if let Some(dir) = self.active_doc().and_then(|d| d.path.borrow().as_ref().and_then(|p| p.parent().map(Path::to_path_buf))) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let ed = self.clone();
        dialog.open_multiple(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            if let Ok(list) = res {
                for i in 0..list.n_items() {
                    if let Some(p) = list.item(i).and_downcast::<gio::File>().and_then(|f| f.path()) {
                        ed.open_path(&p);
                    }
                }
            }
        });
    }

    /// Save a document; `then` receives whether it was saved.
    fn save_doc(self: &Rc<Self>, d: &Rc<Doc>, ask_path: bool, then: Option<Box<dyn FnOnce(bool)>>) {
        if let Err(e) = d.ensure_loaded() {
            self.error_dialog("Impossibile salvare", &e);
            if let Some(t) = then {
                t(false);
            }
            return;
        }
        let path = d.path.borrow().clone();
        match path {
            Some(p) if !ask_path => self.save_checked(d, p, then),
            _ => {
                let dialog = gtk::FileDialog::builder().title("Salva con nome").modal(true).build();
                match path.as_ref() {
                    Some(p) => {
                        dialog.set_initial_file(Some(&gio::File::for_path(p)));
                    }
                    None => {
                        let name: String = d.title().chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
                        dialog.set_initial_name(Some(&if name.is_empty() { "senza-titolo.txt".to_string() } else { format!("{name}.txt") }));
                        if let Some(dir) = self.recent.borrow().first().and_then(|p| p.parent()) {
                            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
                        }
                    }
                }
                let ed = self.clone();
                let d = d.clone();
                dialog.save(Some(&self.win), None::<&gio::Cancellable>, move |res| match res.ok().and_then(|f| f.path()) {
                    Some(p) => ed.save_checked(&d, p, then),
                    None => {
                        if let Some(t) = then {
                            t(false);
                        }
                    }
                });
            }
        }
    }

    /// Warn about characters the target encoding cannot represent.
    fn save_checked(self: &Rc<Self>, d: &Rc<Doc>, path: PathBuf, then: Option<Box<dyn FnOnce(bool)>>) {
        let bad = encoding::unrepresentable(&d.text(), d.encoding.get());
        if bad == 0 {
            let ok = self.write_doc(d, &path);
            if let Some(t) = then {
                t(ok);
            }
            return;
        }
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Caratteri non rappresentabili")
            .detail(format!(
                "{bad} caratteri non possono essere codificati in {} e verranno sostituiti con '?'.",
                d.encoding.get().label()
            ))
            .buttons(["Annulla", "Salva in UTF-8", "Salva comunque"])
            .cancel_button(0)
            .default_button(1)
            .build();
        let ed = self.clone();
        let d = d.clone();
        dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            let ok = match res {
                Ok(1) => {
                    d.encoding.set(Encoding::Utf8);
                    ed.write_doc(&d, &path)
                }
                Ok(2) => ed.write_doc(&d, &path),
                _ => false,
            };
            if let Some(t) = then {
                t(ok);
            }
        });
    }

    fn write_doc(self: &Rc<Self>, d: &Rc<Doc>, path: &Path) -> bool {
        let was_untitled = d.path.borrow().is_none();
        match d.save_to(path) {
            Ok(()) => {
                if was_untitled || d.lang.get() == Lang::Plain {
                    let head: String = d.text().chars().take(256).collect();
                    let l = lang::detect(Some(path), &head);
                    if l != d.lang.get() {
                        d.set_lang(l);
                    }
                }
                self.add_recent(path);
                self.update_status_for(d);
                self.sync_sidebar();
                self.git_refresh();
                self.symbols_saved(d);
                self.lsp_saved(d);
                self.message(&format!("Salvato {} ({})", path.display(), d.encoding.get().label()));
                self.schedule_layout();
                self.schedule_autosave();
                true
            }
            Err(e) => {
                self.error_dialog("Errore di salvataggio", &e);
                false
            }
        }
    }

    fn close_doc(self: &Rc<Self>, d: &Rc<Doc>) {
        self.close_doc_then(d, |_, _| {});
    }

    /// Closes `d` (asking to save it if needed), then calls `then` with
    /// whether it was closed.
    fn close_doc_then(self: &Rc<Self>, d: &Rc<Doc>, then: impl FnOnce(&Rc<Editor>, bool) + 'static) {
        if !d.is_unsaved() {
            self.remove_doc(d);
            then(self, true);
            return;
        }
        self.focus_doc(d);
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Salvare le modifiche a «{}»?", d.title()))
            .detail("Se non salvi, le modifiche andranno perse.")
            .buttons(["Annulla", "Non salvare", "Salva"])
            .cancel_button(0)
            .default_button(2)
            .build();
        let ed = self.clone();
        let d = d.clone();
        dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| match res {
            Ok(1) => {
                ed.remove_doc(&d);
                then(&ed, true);
            }
            Ok(2) => {
                let ed2 = ed.clone();
                let d2 = d.clone();
                ed.save_doc(
                    &d,
                    false,
                    Some(Box::new(move |ok| {
                        if ok {
                            ed2.remove_doc(&d2);
                        }
                        then(&ed2, ok);
                    })),
                );
            }
            _ => then(&ed, false),
        });
    }

    /// "Close working folder": closes every document (asking to save the
    /// modified ones; cancelling stops here), then the folder, its index
    /// and the side panel.
    fn close_workdir(self: &Rc<Self>) {
        if self.workdir.borrow().is_none() {
            self.message("Nessuna cartella di lavoro aperta");
            return;
        }
        let mut docs: Vec<Rc<Doc>> = self.docs.borrow().clone();
        // Documents are taken from the end: the modified ones first, so
        // that cancelling leaves everything open.
        docs.sort_by_key(|d| d.is_unsaved());
        self.close_docs_then_workdir(docs);
    }

    fn close_docs_then_workdir(self: &Rc<Self>, mut docs: Vec<Rc<Doc>>) {
        // Documents still open (one may have been closed meanwhile).
        let Some(d) = docs.pop() else {
            self.set_sidebar(false);
            self.set_workdir(None);
            return;
        };
        if !self.docs.borrow().iter().any(|x| Rc::ptr_eq(x, &d)) {
            self.close_docs_then_workdir(docs);
            return;
        }
        self.close_doc_then(&d, move |ed, closed| {
            if closed {
                ed.close_docs_then_workdir(docs);
            } else {
                ed.message("Chiusura della cartella di lavoro annullata");
            }
        });
    }

    fn remove_doc(self: &Rc<Self>, d: &Rc<Doc>) {
        self.forget_completion(d);
        self.lsp_closed(d);
        if let Some(p) = self.pane_of(d) {
            let nb = &self.notebooks[p];
            if let Some(i) = nb.page_num(&d.scroller) {
                nb.remove_page(Some(i));
            }
        }
        d.drop_cache();
        self.docs.borrow_mut().retain(|x| !Rc::ptr_eq(x, d));
        if self.docs.borrow().is_empty() {
            let n = self.new_doc();
            self.add_doc(&n, 0);
        }
        self.check_panes();
        if let Some(a) = self.active_doc() {
            a.view.grab_focus();
            self.update_status_for(&a);
        }
        self.schedule_autosave();
        self.release_memory();
    }

    fn reopen_with(self: &Rc<Self>, d: &Rc<Doc>, enc: Encoding) {
        let Some(path) = d.path.borrow().clone() else {
            self.message("Il documento non è ancora stato salvato su disco");
            return;
        };
        let reload = {
            let ed = self.clone();
            let d = d.clone();
            move || match doc::read_file(&path, Some(enc)) {
                Ok((text, enc, eol)) => {
                    let cursor = d.cursor_offset();
                    d.encoding.set(enc);
                    d.eol.set(eol);
                    d.set_content(&text, false, cursor);
                    d.drop_cache();
                    ed.update_status_for(&d);
                    ed.message(&format!("Riaperto come {}", enc.label()));
                }
                Err(e) => ed.error_dialog("Impossibile rileggere il file", &format!("{}: {e}", path.display())),
            }
        };
        if !d.is_modified() {
            reload();
            return;
        }
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Scartare le modifiche?")
            .detail("Il file verrà riletto dal disco e le modifiche non salvate andranno perse.")
            .buttons(["Annulla", "Rileggi"])
            .cancel_button(0)
            .default_button(0)
            .build();
        dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            if res == Ok(1) {
                reload();
            }
        });
    }

    fn convert_encoding(self: &Rc<Self>, d: &Rc<Doc>, enc: Encoding) {
        if let Err(e) = d.ensure_loaded() {
            self.message(&e);
            return;
        }
        if d.encoding.get() == enc {
            return;
        }
        let bad = encoding::unrepresentable(&d.text(), enc);
        d.encoding.set(enc);
        d.buffer.set_modified(true);
        self.update_status_for(d);
        if bad > 0 {
            self.message(&format!(
                "Codifica: {} — attenzione: {bad} caratteri non rappresentabili diventeranno '?' al salvataggio",
                enc.label()
            ));
        } else {
            self.message(&format!("Codifica impostata a {} (applicata al salvataggio)", enc.label()));
        }
    }

    // ---------------------------------------------------------- editing

    /// Selected line range as (start iter at line start, end iter at line end).
    fn line_block(d: &Doc) -> (gtk::TextIter, gtk::TextIter) {
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

    fn toggle_comment(&self) {
        let Some(d) = self.active_doc() else { return };
        let (mut a, mut b) = Self::line_block(&d);
        let text = d.buffer.text(&a, &b, true);
        let lines: Vec<&str> = text.split('\n').collect();
        // HTML and Vue: the comment syntax of the script or style block.
        let before = d.buffer.text(&d.buffer.start_iter(), &a, false);
        let lang = format::embedded_lang(d.lang.get(), &before);
        let Some(new) = format::toggle_comment(lang, &lines) else {
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

    fn duplicate_line(&self) {
        let Some(d) = self.active_doc() else { return };
        let (a, mut b) = Self::line_block(&d);
        let text = d.buffer.text(&a, &b, true);
        d.buffer.begin_user_action();
        d.buffer.insert(&mut b, &format!("\n{text}"));
        d.buffer.end_user_action();
    }

    fn delete_line(&self) {
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

    fn format_doc(&self) {
        let Some(d) = self.active_doc() else { return };
        let text = d.text();
        let (tab, spaces) = {
            let s = self.shared.settings.borrow();
            (s.tab_width, s.insert_spaces)
        };
        let path = d.path.borrow().clone();
        match format::format(d.lang.get(), path.as_deref(), &text, tab, spaces) {
            Ok(f) => {
                if f.text != text {
                    d.replace_all_text(&f.text);
                    self.message(&format!("Documento formattato ({})", f.tool));
                } else {
                    self.message(&format!("Il documento è già formattato ({})", f.tool));
                }
            }
            Err(e) => self.message(&format!("Formattazione non riuscita: {e}")),
        }
    }

    // ------------------------------------------------------------ search

    fn show_search(&self, replace: bool) {
        self.search.replace_row.set_visible(replace);
        self.search.revealer.set_reveal_child(true);
        if let Some(d) = self.active_doc()
            && let Some((a, b)) = d.buffer.selection_bounds()
                && a.line() == b.line() {
                    self.search.find.set_text(&d.buffer.text(&a, &b, false));
                }
        self.search.find.grab_focus();
        self.incremental_search();
    }

    fn hide_search(&self) {
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

    fn query(&self) -> Option<Query> {
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
    fn highlight_matches(&self, d: &Rc<Doc>) -> usize {
        let buf = &d.buffer;
        let tag = &self.shared.tags.search;
        let (a, b) = buf.bounds();
        buf.remove_tag(tag, &a, &b);
        let Some(q) = self.query() else { return 0 };
        let text = d.text();
        let mut count = 0;
        let mut last = (0usize, 0usize);
        for m in q.re.find_iter(&text) {
            if m.start() == m.end() {
                continue;
            }
            count += 1;
            if count <= MAX_MATCH_HIGHLIGHT {
                let s = crate::search::char_offset(&text, last, m.start());
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

    fn incremental_search(&self) {
        let Some(d) = self.active_doc() else { return };
        if self.highlight_matches(&d) > 0 {
            // Restart from the beginning of the current selection.
            let buf = &d.buffer;
            let start = buf.selection_bounds().map(|(a, _)| a).unwrap_or_else(|| buf.iter_at_mark(&buf.get_insert()));
            buf.place_cursor(&start);
            self.find_step(true);
        }
    }

    /// Select the next/previous match, wrapping around.
    fn find_step(&self, forward: bool) {
        let Some(d) = self.active_doc() else { return };
        if self.search.find.text().is_empty() {
            self.show_search(false);
            return;
        }
        let Some(q) = self.query() else { return };
        let buf = &d.buffer;
        let text = d.text();
        let (sel_a, sel_b) = buf.selection_bounds().unwrap_or_else(|| {
            let i = buf.iter_at_mark(&buf.get_insert());
            (i, i)
        });
        let pivot_char = if forward { sel_b.offset() } else { sel_a.offset() } as usize;
        let pivot = text.char_indices().nth(pivot_char).map_or(text.len(), |(b, _)| b);
        let nonempty = |m: &regex::Match| m.start() != m.end();
        let found = if forward {
            q.re.find_iter(&text[pivot..]).find(nonempty).map(|m| (m.start() + pivot, m.end() + pivot)).or_else(|| {
                q.re.find_iter(&text).find(nonempty).map(|m| (m.start(), m.end()))
            })
        } else {
            let mut before = None;
            let mut last = None;
            for m in q.re.find_iter(&text).filter(nonempty) {
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

    fn replace_one(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else { return };
        let buf = &d.buffer;
        if let Some((mut a, mut b)) = buf.selection_bounds() {
            let text = d.text();
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
                }
        }
        self.find_step(true);
        self.highlight_matches(&d);
    }

    fn replace_all(&self) {
        let Some(d) = self.active_doc() else { return };
        let Some(q) = self.query() else { return };
        let text = d.text();
        let repl = self.search.replace.text().to_string();
        let mut edits: Vec<(i32, i32, String)> = Vec::new();
        let mut last = (0usize, 0usize);
        for m in q.re.find_iter(&text) {
            if m.start() == m.end() {
                continue;
            }
            let s = crate::search::char_offset(&text, last, m.start());
            let e = s + text[m.start()..m.end()].chars().count();
            last = (m.end(), e);
            edits.push((s as i32, e as i32, q.replacement(&text, m.start(), &repl)));
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
        self.highlight_matches(&d);
        self.message(&format!("{n} sostituzioni effettuate"));
    }

    // -------------------------------------------------------------- diff

    fn set_diff(self: &Rc<Self>, mut on: bool) {
        if on && !self.notebooks[1].is_visible() {
            if self.docs.borrow().len() < 2 {
                self.message("Apri almeno due schede per confrontarle");
                on = false;
            } else {
                self.toggle_split();
            }
        }
        self.diff_on.set(on);
        if let Some(a) = self.win.lookup_action("diff").and_downcast::<gio::SimpleAction>() {
            a.set_state(&on.to_variant());
        }
        if on {
            self.compute_diff();
        } else {
            self.clear_diff();
            self.status.diff.set_visible(false);
        }
    }

    fn in_diff(&self, d: &Rc<Doc>) -> bool {
        self.diff_on.get() && self.diff_pair.borrow().iter().any(|w| w.upgrade().is_some_and(|x| Rc::ptr_eq(&x, d)))
    }

    fn clear_diff(&self) {
        let tags = &self.shared.tags;
        for d in self.diff_pair.borrow_mut().drain(..).filter_map(|w| w.upgrade()) {
            let (a, b) = d.buffer.bounds();
            for t in tags.diff_tags() {
                d.buffer.remove_tag(t, &a, &b);
            }
        }
        *self.diff_result.borrow_mut() = None;
    }

    /// Recompute the diff shortly after the last change.
    fn schedule_diff(self: &Rc<Self>) {
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

    fn compute_diff(self: &Rc<Self>) {
        self.clear_diff();
        self.status.diff.set_visible(true);
        let (Some(l), Some(r)) = (self.current_doc(0), self.current_doc(1)) else {
            self.status.diff.set_text("Diff: serve una scheda per pannello");
            return;
        };
        for d in [&l, &r] {
            if let Err(e) = d.ensure_loaded() {
                self.message(&format!("Errore: {e}"));
                return;
            }
        }
        let res = diff::compute(&l.text(), &r.text());
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
                HunkKind::Added => mark_lines(&r, &h.new, &tags.diff_add),
                HunkKind::Removed => mark_lines(&l, &h.old, &tags.diff_del),
                HunkKind::Changed => {
                    mark_lines(&l, &h.old, &tags.diff_chg);
                    mark_lines(&r, &h.new, &tags.diff_chg);
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
        mark_words(&l, &res.old_words, &tags.diff_word_del);
        mark_words(&r, &res.new_words, &tags.diff_word_add);

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

    fn diff_docs(&self) -> Option<(Rc<Doc>, Rc<Doc>)> {
        let p = self.diff_pair.borrow();
        Some((p.first()?.upgrade()?, p.get(1)?.upgrade()?))
    }

    /// Move both documents to the next/previous difference.
    fn diff_jump(&self, forward: bool) {
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
    fn sync_scroll(&self, src: &Rc<Doc>) {
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

    // ------------------------------------------------------ panes & tabs

    fn toggle_split(self: &Rc<Self>) {
        let right = &self.notebooks[1];
        if right.is_visible() {
            // Merge everything back into the left pane.
            let docs: Vec<Rc<Doc>> = self.docs.borrow().iter().filter(|d| self.pane_of(d) == Some(1)).cloned().collect();
            for d in docs {
                self.move_doc(&d, 0);
            }
            right.set_visible(false);
            self.active_pane.set(0);
            self.set_diff(false);
        } else {
            right.set_visible(true);
            self.paned.set_position(self.paned.width() / 2);
            let left = &self.notebooks[0];
            if right.n_pages() == 0 {
                if left.n_pages() >= 2 {
                    if let Some(d) = self.current_doc(0) {
                        self.move_doc(&d, 1);
                    }
                } else {
                    let d = self.new_doc();
                    self.add_doc(&d, 1);
                }
            }
            if let Some(d) = self.current_doc(1) {
                self.focus_doc(&d);
            }
        }
        self.schedule_layout();
        self.schedule_autosave();
    }

    fn move_to_other_pane(self: &Rc<Self>, d: &Rc<Doc>) {
        let Some(p) = self.pane_of(d) else { return };
        if !self.notebooks[1].is_visible() {
            self.notebooks[1].set_visible(true);
            self.paned.set_position(self.paned.width() / 2);
        }
        self.move_doc(d, 1 - p);
        self.focus_doc(d);
        self.check_panes();
    }

    fn move_doc(&self, d: &Rc<Doc>, to: usize) {
        let Some(from) = self.pane_of(d) else { return };
        if from == to {
            return;
        }
        self.notebooks[from].detach_tab(&d.scroller);
        let nb = &self.notebooks[to];
        let i = nb.append_page(&d.scroller, Some(&d.tab));
        nb.set_current_page(Some(i));
    }

    /// Collapse the split view when one pane became empty.
    fn check_panes(self: &Rc<Self>) {
        let [l, r] = &self.notebooks;
        if !r.is_visible() {
            return;
        }
        if r.n_pages() == 0 || l.n_pages() == 0 {
            let docs: Vec<Rc<Doc>> = self.docs.borrow().iter().filter(|d| self.pane_of(d) == Some(1)).cloned().collect();
            for d in docs {
                self.move_doc(&d, 0);
            }
            r.set_visible(false);
            self.active_pane.set(0);
            self.set_diff(false);
            self.schedule_layout();
        }
    }

    fn cycle_tab(&self, delta: i32) {
        let nb = &self.notebooks[self.active_pane.get()];
        let n = nb.n_pages() as i32;
        if n == 0 {
            return;
        }
        let cur = nb.current_page().unwrap_or(0) as i32;
        nb.set_current_page(Some(((cur + delta).rem_euclid(n)) as u32));
        if let Some(d) = self.active_doc() {
            d.view.grab_focus();
        }
    }

    /// Recompute how many characters each tab label may use, once the
    /// new allocation is known (next frame).
    fn schedule_layout(self: &Rc<Self>) {
        if self.layout_pending.replace(true) {
            return;
        }
        let ed = self.clone();
        self.win.add_tick_callback(move |_, _| {
            ed.layout_pending.set(false);
            ed.layout_tabs();
            glib::ControlFlow::Break
        });
    }

    fn layout_tabs(&self) {
        let layout = self.win.create_pango_layout(Some("abcdefghijklmnopqrstuvwxyz"));
        let char_px = (layout.pixel_size().0 / 26).max(4);
        for nb in &self.notebooks {
            let n = nb.n_pages().max(1) as i32;
            let width = if nb.width() > 0 { nb.width() } else { self.win.default_width() };
            // Room for close button, padding and the modified marker.
            let per_tab = width / n - 50;
            let chars = (per_tab / char_px).clamp(1, 40) as usize;
            for i in 0..nb.n_pages() {
                if let Some(d) = nb.nth_page(Some(i)).and_then(|w| self.doc_for_widget(&w)) {
                    d.set_label_budget(chars);
                }
            }
        }
    }

    // ----------------------------------------------------------- status

    // ------------------------------------------------------------ terminal

    /// Folder of the active document, or the working folder (then the home
    /// folder) if it was never saved.
    fn doc_dir(&self) -> PathBuf {
        self.active_doc()
            .and_then(|d| d.path.borrow().as_deref().and_then(Path::parent).map(Path::to_path_buf))
            .filter(|p| p.is_dir())
            .or_else(|| self.workdir.borrow().clone())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn terminal_visible(&self) -> bool {
        self.terminal.borrow().as_ref().is_some_and(|t| t.widget.get_visible())
    }

    fn set_terminal_state(&self, on: bool) {
        if let Some(a) = self.win.lookup_action("terminal").and_downcast::<gio::SimpleAction>() {
            a.set_state(&on.to_variant());
        }
    }

    /// Shows or hides the terminal panel. The first time, the shell is
    /// started in the folder of the active document; hiding the panel keeps
    /// the shell running.
    fn set_terminal(self: &Rc<Self>, on: bool) {
        self.show_terminal(on, &self.doc_dir());
    }

    /// Like `set_terminal`; a new shell is started in `dir`.
    fn show_terminal(self: &Rc<Self>, on: bool, dir: &Path) {
        let created = on && self.terminal.borrow().is_none();
        if created {
            let weak = Rc::downgrade(self);
            let on_exit = move || {
                if let Some(ed) = weak.upgrade() {
                    // Leave the signal handler before destroying the widget.
                    glib::idle_add_local_once(move || ed.drop_terminal());
                }
            };
            let weak = Rc::downgrade(self);
            let on_error = move |e: String| {
                if let Some(ed) = weak.upgrade() {
                    ed.error_dialog("Impossibile avviare la shell", &e);
                    glib::idle_add_local_once(move || ed.drop_terminal());
                }
            };
            let t = match Terminal::new(dir, on_exit, on_error) {
                Ok(t) => t,
                Err(e) => {
                    self.set_terminal_state(false);
                    self.error_dialog(
                        "Terminale non disponibile",
                        &format!("Serve la libreria VTE per GTK 4 (pacchetto «vte4» su Arch, «libvte-2.91-gtk4-0» su Debian/Ubuntu).\n\n{e}"),
                    );
                    return;
                }
            };
            let s = self.shared.settings.borrow();
            t.apply_style(&self.shared.font.borrow(), s.dark, s.bell);
            drop(s);
            let weak = Rc::downgrade(self);
            t.connect_focus(move |focus| {
                if let Some(ed) = weak.upgrade() {
                    ed.suspend_accels(focus);
                }
            });
            t.widget.set_visible(false);
            self.vpaned.set_end_child(Some(&t.widget));
            *self.terminal.borrow_mut() = Some(t);
        }
        let Some(widget) = self.terminal.borrow().as_ref().map(|t| t.widget.clone()) else { return };
        if on {
            widget.set_visible(true);
            if created || self.vpaned.height() - self.vpaned.position() < 60 {
                let h = self.vpaned.height();
                self.vpaned.set_position(h - (h / 3).clamp(120, 400));
            }
            if let Some(t) = self.terminal.borrow().as_ref() {
                t.grab_focus();
            }
        } else {
            widget.set_visible(false);
            self.suspend_accels(false);
            if let Some(d) = self.active_doc() {
                d.view.grab_focus();
            }
        }
        self.set_terminal_state(on);
    }

    /// Opens the terminal and moves the shell to `dir`.
    fn terminal_at(self: &Rc<Self>, dir: PathBuf) {
        let res = self.terminal.borrow().as_ref().map(|t| t.cd(&dir));
        if let Some(Err(e)) = res {
            self.message(&e);
        }
        self.show_terminal(true, &dir);
    }

    // ---------------------------------------------------------- file panel

    fn sidebar_refresh(&self) {
        if let Some(s) = self.sidebar.borrow().clone() {
            s.refresh();
        }
    }

    fn sidebar_visible(&self) -> bool {
        self.sidebar.borrow().as_ref().is_some_and(|s| s.widget.get_visible())
    }

    fn set_sidebar(self: &Rc<Self>, on: bool) {
        if on && self.sidebar.borrow().is_none() {
            let w = Rc::downgrade(self);
            let w2 = w.clone();
            let w3 = w.clone();
            let w4 = w.clone();
            let w5 = w.clone();
            let w6 = w.clone();
            let s = Sidebar::new(sidebar::Handlers {
                open: Box::new(move |p| {
                    if let Some(ed) = w.upgrade() {
                        ed.open_path(p);
                    }
                }),
                terminal: Box::new(move |p| {
                    if let Some(ed) = w2.upgrade() {
                        ed.terminal_at(p.to_path_buf());
                    }
                }),
                set_workdir: Box::new(move |p| {
                    if let Some(ed) = w3.upgrade() {
                        match p {
                            Some(p) => ed.set_workdir(Some(p)),
                            None => ed.close_workdir(),
                        }
                    }
                }),
                renamed: Box::new(move |old, new| {
                    if let Some(ed) = w4.upgrade() {
                        ed.renamed(old, new);
                    }
                }),
                error: Box::new(move |title, detail| {
                    if let Some(ed) = w5.upgrade() {
                        ed.error_dialog(title, detail);
                    }
                }),
                git: Box::new(move |action, p| {
                    if let Some(ed) = w6.upgrade() {
                        ed.git_path_action(action, p);
                    }
                }),
            });
            s.set_git(self.git_status());
            self.hpaned.set_start_child(Some(&s.widget));
            self.hpaned.set_position(self.shared.settings.borrow().sidebar_width);
            *self.sidebar.borrow_mut() = Some(s);
        }
        let Some(s) = self.sidebar.borrow().clone() else { return };
        if !on && s.widget.get_visible() {
            self.shared.settings.borrow_mut().sidebar_width = self.hpaned.position();
        }
        s.widget.set_visible(on);
        if on {
            self.sync_sidebar();
        } else if s.widget.focus_child().is_some()
            && let Some(d) = self.active_doc()
        {
            d.view.grab_focus();
        }
        self.shared.settings.borrow_mut().sidebar = on;
        if let Some(a) = self.win.lookup_action("sidebar").and_downcast::<gio::SimpleAction>() {
            a.set_state(&on.to_variant());
        }
    }

    /// Shows the working folder, or the folder of the active document, and
    /// selects the active document.
    fn sync_sidebar(self: &Rc<Self>) {
        if !self.sidebar_visible() {
            return;
        }
        let Some(s) = self.sidebar.borrow().clone() else { return };
        let file = self.active_doc().and_then(|d| d.path.borrow().clone());
        let workdir = self.workdir.borrow().clone();
        match (&workdir, &file) {
            (Some(w), _) => s.set_root(w, true),
            // Keep the folder shown (the user may have gone up) while it
            // contains the document.
            (None, Some(f)) => {
                let root = s.root();
                if (root.as_os_str().is_empty() || !f.starts_with(&root))
                    && let Some(dir) = f.parent()
                {
                    s.set_root(dir, false);
                }
            }
            (None, None) => {
                if s.root().as_os_str().is_empty() {
                    s.set_root(&self.doc_dir(), false);
                }
            }
        }
        if let Some(f) = file {
            s.reveal(&f);
        }
    }

    fn set_workdir(self: &Rc<Self>, dir: Option<PathBuf>) {
        let dir = dir.map(|d| std::fs::canonicalize(&d).unwrap_or(d));
        let old = self.workdir.replace(dir.clone());
        match &dir {
            Some(d) => {
                self.message(&format!("Cartella di lavoro: {}", d.display()));
                self.set_sidebar(true);
            }
            None => {
                self.message("Cartella di lavoro chiusa");
                // Release the listing of the folder (and its monitors); the
                // panel goes back to the folder of the active document.
                if let Some(s) = self.sidebar.borrow().clone() {
                    s.clear();
                }
                // Close its git changes windows (they remove themselves
                // from the list when closed).
                if let Some(old) = old {
                    let inside: Vec<_> = self.changes.borrow().iter().filter(|c| c.folder().starts_with(&old)).cloned().collect();
                    for c in inside {
                        c.win.close();
                    }
                }
                self.release_memory();
            }
        }
        self.sync_index();
        self.sync_lsp();
        self.sync_sidebar();
        self.git_refresh();
        self.schedule_autosave();
    }

    /// Returns the memory freed by closed documents and folders to the
    /// system (the allocator would otherwise keep it for later use).
    fn release_memory(self: &Rc<Self>) {
        if self.trim_pending.replace(true) {
            return;
        }
        let ed = Rc::downgrade(self);
        // GTK frees some resources from the main loop: wait a little.
        glib::timeout_add_local_once(Duration::from_millis(800), move || {
            if let Some(ed) = ed.upgrade() {
                ed.trim_pending.set(false);
                #[cfg(target_env = "gnu")]
                // SAFETY: malloc_trim only releases unused heap memory.
                unsafe {
                    libc::malloc_trim(0);
                }
            }
        });
    }

    fn open_folder_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title("Apri cartella di lavoro").modal(true).build();
        if let Some(w) = self.workdir.borrow().as_ref() {
            dialog.set_initial_folder(Some(&gio::File::for_path(w)));
        } else {
            dialog.set_initial_folder(Some(&gio::File::for_path(self.doc_dir())));
        }
        let ed = self.clone();
        dialog.select_folder(Some(&self.win), None::<&gio::Cancellable>, move |res| {
            if let Ok(p) = res.map(|f| f.path()) {
                ed.set_workdir(p);
            }
        });
    }

    /// Updates open documents after a file or folder was renamed.
    fn renamed(self: &Rc<Self>, old: &Path, new: &Path) {
        for d in self.docs.borrow().iter() {
            let moved = d.path.borrow().as_ref().and_then(|p| p.strip_prefix(old).ok().map(|rest| new.join(rest)));
            if let Some(p) = moved {
                let p = if p.as_os_str().is_empty() { new.to_path_buf() } else { p };
                *d.path.borrow_mut() = Some(p);
                d.refresh_title();
                if self.is_active(d) {
                    self.update_status_for(d);
                }
            }
        }
        if self.workdir.borrow().as_ref().is_some_and(|w| w.starts_with(old)) {
            let w = self.workdir.borrow().as_ref().and_then(|w| w.strip_prefix(old).ok().map(|r| new.join(r)));
            *self.workdir.borrow_mut() = w;
        }
        self.schedule_layout();
        self.git_refresh();
    }

    /// Removes the terminal after its shell has exited.
    fn drop_terminal(self: &Rc<Self>) {
        let Some(t) = self.terminal.borrow_mut().take() else { return };
        let had_focus = t.has_focus();
        self.vpaned.set_end_child(None::<&gtk::Widget>);
        self.suspend_accels(false);
        self.set_terminal_state(false);
        self.release_memory();
        if had_focus && let Some(d) = self.active_doc() {
            d.view.grab_focus();
        }
    }

    /// Window shortcuts are handled before the focused widget sees the key:
    /// while the terminal has the focus they are removed (except a few), so
    /// that keys like Ctrl+C or Ctrl+R reach the shell.
    fn suspend_accels(&self, suspend: bool) {
        let mut saved = self.term_accels.borrow_mut();
        if suspend {
            if !saved.is_empty() {
                return;
            }
            for name in self.app.list_action_descriptions() {
                if TERMINAL_ACCELS.contains(&name.as_str()) {
                    continue;
                }
                let accels = self.app.accels_for_action(&name);
                if !accels.is_empty() {
                    self.app.set_accels_for_action(&name, &[]);
                    saved.push((name.to_string(), accels));
                }
            }
        } else {
            for (name, accels) in saved.drain(..) {
                let accels: Vec<&str> = accels.iter().map(|a| a.as_str()).collect();
                self.app.set_accels_for_action(&name, &accels);
            }
        }
    }

    fn message(&self, text: &str) {
        self.status.message.set_text(text);
        let token = self.message_gen.get().wrapping_add(1);
        self.message_gen.set(token);
        let cell = self.message_gen.clone();
        let label = self.status.message.downgrade();
        // Clear the message unless a newer one replaced it.
        glib::timeout_add_seconds_local_once(6, move || {
            if cell.get() == token
                && let Some(l) = label.upgrade() {
                    l.set_text("");
                }
        });
    }

    /// Resident memory of the process, shown in the status bar.
    fn update_memory(&self) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0);
        let fmt = |kb: u64| {
            let mb = kb as f64 / 1024.0;
            if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
        };
        let lsp_kb = self.lsp_memory_kb();
        self.status.mem.set_text(&if lsp_kb > 0 { format!("RAM {} + LSP {}", fmt(kb), fmt(lsp_kb)) } else { format!("RAM {}", fmt(kb)) });
        let docs = self.docs.borrow();
        let loaded = docs.iter().filter(|d| d.is_loaded()).count();
        self.status.mem.set_tooltip_text(Some(&format!(
            "Memoria residente del processo (RSS): {kb} kB\nLanguage server: {lsp_kb} kB\nSchede caricate in memoria: {loaded} di {}",
            docs.len()
        )));
    }

    fn error_dialog(&self, title: &str, detail: &str) {
        gtk::AlertDialog::builder().modal(true).message(title).detail(detail).buttons(["OK"]).build().show(Some(&self.win));
    }

    fn update_status(&self) {
        if let Some(d) = self.active_doc() {
            self.update_status_for(&d);
        }
    }

    fn update_status_for(&self, d: &Rc<Doc>) {
        if !self.is_active(d) {
            return;
        }
        self.status.lang.set_label(d.lang.get().label());
        self.status.enc.set_label(d.encoding.get().label());
        self.status.eol.set_label(d.eol.get().label());
        self.lang_action.set_state(&d.lang.get().id().to_variant());
        self.eol_action.set_state(&d.eol.get().id().to_variant());
        self.update_pos(d);
        self.update_title(d);
    }

    fn update_pos(&self, d: &Doc) {
        let it = d.buffer.iter_at_mark(&d.buffer.get_insert());
        let sel = d.buffer.selection_bounds().map(|(a, b)| b.offset() - a.offset());
        let text = match sel {
            Some(n) if n > 0 => format!("Ri {}, Col {} ({n} sel.)", it.line() + 1, it.line_offset() + 1),
            _ => format!("Ri {}, Col {}", it.line() + 1, it.line_offset() + 1),
        };
        self.status.pos.set_label(&text);
    }

    fn update_title(&self, d: &Doc) {
        let mark = if d.is_unsaved() { "• " } else { "" };
        let name = match d.path.borrow().as_ref() {
            Some(p) => p.display().to_string(),
            None => d.title(),
        };
        self.win.set_title(Some(&format!("{mark}{name} — macaco")));
    }

    // ---------------------------------------------------- recent files

    fn add_recent(&self, path: &Path) {
        config::push_recent(&mut self.recent.borrow_mut(), path);
        config::save_recent(&self.recent.borrow());
        self.rebuild_recent_menu();
    }

    fn rebuild_recent_menu(&self) {
        let m = &self.recent_menu;
        m.remove_all();
        let files = gio::Menu::new();
        for p in self.recent.borrow().iter() {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dir = p.parent().map(|d| d.display().to_string()).unwrap_or_default();
            let home = std::env::var("HOME").unwrap_or_default();
            let dir = if !home.is_empty() && dir.starts_with(&home) { format!("~{}", &dir[home.len()..]) } else { dir };
            // '_' would be taken as a mnemonic marker.
            let label = format!("{name}  —  {dir}").replace('_', "__");
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(Some("win.open-recent"), Some(&p.to_string_lossy().to_variant()));
            files.append_item(&item);
        }
        if self.recent.borrow().is_empty() {
            files.append(Some("(nessun file recente)"), Some("win.none"));
        }
        m.append_section(None, &files);
        let clear = gio::Menu::new();
        clear.append(Some("Svuota elenco"), Some("win.clear-recent"));
        m.append_section(None, &clear);
    }

    // ------------------------------------------------- settings & fonts

    fn settings_changed(self: &Rc<Self>) {
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
            if let Some(a) = self.win.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_state(&v.to_variant());
            }
        }
        s.save();
    }

    fn apply_font(&self) {
        let (family, size, dark) = {
            let s = self.shared.settings.borrow();
            (s.font_family.clone(), s.font_size, s.dark)
        };
        // The editor area uses its own colors so that the syntax palette
        // stays readable whatever the system GTK theme is.
        let (bg, fg, sel) = if dark { ("#1e2127", "#abb2bf", "#3e4f6a") } else { ("#ffffff", "#24292e", "#b6d2f5") };
        let (git_new, git_staged, git_mod, git_conflict) =
            if dark { ("#98c379", "#61afef", "#e5c07b", "#e06c75") } else { ("#22863a", "#0366d6", "#b08800", "#d73a49") };
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
             button.tab-close {{ min-width: 16px; min-height: 16px; padding: 0 2px; margin: 0; }}\n\
             notebook > header tab {{ padding: 2px 6px; min-height: 0; }}\n\
             .statusbar {{ padding: 0 4px; border-top: 1px solid alpha(currentColor, 0.12); }}\n\
             .statusbar button {{ padding: 0 8px; min-height: 22px; font-size: 0.9em; }}\n\
             .statusbar label {{ font-size: 0.9em; }}\n\
             .diff-info {{ margin: 0 8px; font-weight: bold; }}\n\
             .searchbar-box {{ padding: 6px; border-top: 1px solid alpha(currentColor, 0.12); }}\n\
             .terminal-panel vte-terminal {{ padding: 2px 4px; }}\n\
             .sidebar-header {{ padding: 2px 2px 2px 6px; border-bottom: 1px solid alpha(currentColor, 0.12); }}\n\
             .sidebar-header label.workdir {{ font-weight: bold; }}\n\
             .git-mark {{ font-weight: bold; margin-right: 4px; }}\n\
             .sidebar .git-untracked label, .changes-list .git-untracked label.git-mark, button.git-branch.git-untracked {{ color: {git_new}; }}\n\
             .sidebar .git-staged label, .changes-list .git-staged label.git-mark {{ color: {git_staged}; }}\n\
             .sidebar .git-modified label, .changes-list .git-modified label.git-mark {{ color: {git_mod}; }}\n\
             .sidebar .git-conflict label, .changes-list .git-conflict label.git-mark, button.git-branch.git-conflict {{ color: {git_conflict}; }}\n\
             paned > separator {{ background: alpha(currentColor, 0.15); }}"
        ));
    }

    fn apply_theme(&self) {
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

    fn zoom(self: &Rc<Self>, delta: i32) {
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

    fn preferences(self: &Rc<Self>) {
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

        let esc = gtk::EventControllerKey::new();
        let d2 = dlg.downgrade();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(d) = d2.upgrade() {
                    d.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dlg.add_controller(esc);
        dlg.present();
    }

    // ------------------------------------------------ session & caching

    fn schedule_autosave(self: &Rc<Self>) {
        if self.restoring.get() || self.autosave_pending.replace(true) {
            return;
        }
        let ed = Rc::downgrade(self);
        glib::timeout_add_local_once(AUTOSAVE_DELAY, move || {
            if let Some(ed) = ed.upgrade() {
                ed.autosave_pending.set(false);
                ed.persist();
            }
        });
    }

    /// Write stale caches and the session file; remove unused cache files.
    fn persist(&self) {
        if self.shut_down.get() {
            return;
        }
        let mut session = Session {
            split: self.notebooks[1].is_visible(),
            workdir: self.workdir.borrow().clone(),
            ..Default::default()
        };
        let mut keep: Vec<String> = Vec::new();
        for (pane, nb) in self.notebooks.iter().enumerate() {
            session.active[pane] = nb.current_page().unwrap_or(0) as usize;
            for i in 0..nb.n_pages() {
                let Some(d) = nb.nth_page(Some(i)).and_then(|w| self.doc_for_widget(&w)) else { continue };
                if d.is_unsaved() {
                    if d.cache_is_stale()
                        && let Err(e) = d.write_cache() {
                            self.message(&format!("Impossibile scrivere la cache: {e}"));
                        }
                    let id = d.id.borrow().clone();
                    keep.push(id.clone());
                    session.tabs.push((pane, TabRef::Cache(id)));
                } else if let Some(p) = d.path.borrow().clone() {
                    session.tabs.push((pane, TabRef::File(p)));
                }
            }
        }
        session.save();
        for id in config::cached_ids() {
            if !keep.contains(&id) {
                config::delete_cache(&id);
            }
        }
    }

    fn restore_session(self: &Rc<Self>) {
        self.restoring.set(true);
        let session = Session::load();
        *self.workdir.borrow_mut() = session.workdir.clone().filter(|p| p.is_dir());
        let mut used: Vec<String> = Vec::new();
        for (pane, tab) in &session.tabs {
            match tab {
                TabRef::File(p) => {
                    if !p.exists() || self.docs.borrow().iter().any(|d| d.path.borrow().as_ref() == Some(p)) {
                        continue;
                    }
                    let d = self.new_doc();
                    d.set_unloaded_file(p.clone(), lang::detect(Some(p), ""));
                    self.append_restored(&d, *pane, session.split);
                }
                TabRef::Cache(id) => {
                    if let Some(meta) = config::read_cache_meta(id) {
                        used.push(id.clone());
                        let d = Doc::new(&self.shared, id.clone());
                        self.wire_doc(&d);
                        self.docs.borrow_mut().push(d.clone());
                        d.set_unloaded_from_cache(&meta);
                        self.append_restored(&d, *pane, session.split);
                    }
                }
            }
        }
        // Unsaved documents not referenced by the session (e.g. after a
        // crash) are recovered as well.
        for id in config::cached_ids() {
            if used.contains(&id) {
                continue;
            }
            if let Some(meta) = config::read_cache_meta(&id) {
                let d = Doc::new(&self.shared, id.clone());
                self.wire_doc(&d);
                self.docs.borrow_mut().push(d.clone());
                d.set_unloaded_from_cache(&meta);
                self.append_restored(&d, 0, false);
            }
        }
        if session.split && self.notebooks[1].n_pages() > 0 && self.notebooks[0].n_pages() > 0 {
            self.notebooks[1].set_visible(true);
            self.paned.set_position(self.shared.settings.borrow().width / 2);
        } else {
            let docs: Vec<Rc<Doc>> = self.docs.borrow().iter().filter(|d| self.pane_of(d) == Some(1)).cloned().collect();
            for d in docs {
                self.move_doc(&d, 0);
            }
        }
        for (pane, nb) in self.notebooks.iter().enumerate() {
            if nb.n_pages() > 0 {
                let i = (session.active[pane] as u32).min(nb.n_pages() - 1);
                nb.set_current_page(Some(i));
                if let Some(d) = self.current_doc(pane)
                    && let Err(e) = d.ensure_loaded() {
                        self.message(&format!("Errore: {e}"));
                    }
            }
        }
        if self.docs.borrow().is_empty() {
            let d = self.new_doc();
            self.add_doc(&d, 0);
        }
        self.restoring.set(false);
        self.active_pane.set(0);
        self.update_status();
        self.schedule_layout();
    }

    fn append_restored(&self, d: &Rc<Doc>, pane: usize, split: bool) {
        let nb = &self.notebooks[if split { pane } else { 0 }];
        nb.append_page(&d.scroller, Some(&d.tab));
    }

    /// Called when the window closes: unsaved content stays in the cache
    /// and is restored at the next start.
    fn shutdown(&self) {
        if self.shut_down.get() {
            return;
        }
        self.persist();
        self.shut_down.set(true);
        // No language server must outlive the editor.
        for c in self.lsp_clients.borrow_mut().drain(..) {
            c.kill();
        }
        let mut s = self.shared.settings.borrow_mut();
        if self.sidebar_visible() {
            s.sidebar_width = self.hpaned.position();
        }
        if !self.win.is_maximized() {
            s.width = self.win.default_width();
            s.height = self.win.default_height();
        }
        s.save();
    }

    pub fn present(&self) {
        self.win.present();
        if let Some(d) = self.active_doc() {
            d.view.grab_focus();
        }
    }
}

/// Debug helper (debug builds only): `MACACO_DEBUG=act1,act2:param,…,shot:/x.png`
/// runs window actions one after another and can save screenshots.
#[cfg(debug_assertions)]
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
                "inspect" => ed.debug_inspect(),
                "shot-popup" => ed.debug_shot_popup(arg.unwrap_or("/tmp/popup.png")),
                "qo-open" => ed.debug_quick_open(arg.and_then(|a| a.parse().ok()).unwrap_or(0)),
                "term-feed" => {
                    if let Some(t) = ed.terminal.borrow().as_ref() {
                        t.debug_feed(&arg.unwrap_or("").replace("\\n", "\r"));
                    }
                }
                "open" => {
                    let t = Instant::now();
                    ed.open_path(Path::new(arg.unwrap_or("")));
                    eprintln!("open took {:?}", t.elapsed());
                }
                "rss" => {
                    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
                    let rss = status.lines().find(|l| l.starts_with("VmRSS")).unwrap_or("");
                    let (docs, buffers) = doc::live::counts();
                    eprintln!("{rss} docs={docs} buffers={buffers} {}", arg.unwrap_or(""));
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

    fn debug_shot(&self, path: &str) {
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
