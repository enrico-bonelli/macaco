//! Main window: the editor state, its actions, the documents and the two
//! tab panes. The other parts of the window are in the submodules
//! (`window/*.rs`): menus, search bar, preferences, session, terminal…

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

mod agentui;
mod bookmarks;
mod credentials;
mod diffview;
mod editing;
mod external;
mod gitui;
mod history;
mod inspect;
mod lspui;
mod menus;
mod merge;
mod prefs;
mod problems;
mod quickopen;
mod searchbar;
mod session;
mod term;
mod textsearch;
mod workdir;
#[cfg(debug_assertions)]
mod debug;

pub const APP_ID: &str = "io.github.macaco.Editor";

/// Hidden documents are unloaded from memory after this delay.
const UNLOAD_AFTER: Duration = Duration::from_secs(30);

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
    /// One small button per installed coding agent: opens the assistant.
    agents: Vec<(&'static crate::acp::AgentSpec, gtk::Button)>,
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
    /// Favourite files and folders, and the "Preferiti" menu listing them.
    bookmarks: RefCell<Vec<PathBuf>>,
    bookmarks_menu: gio::Menu,
    search: SearchUi,
    status: StatusUi,
    lang_action: gio::SimpleAction,
    eol_action: gio::SimpleAction,
    layout_pending: Cell<bool>,
    autosave_pending: Cell<bool>,
    autosave_writing: Cell<bool>,
    autosave_again: Cell<bool>,
    message_gen: Rc<Cell<u32>>,
    shut_down: Cell<bool>,
    diff_on: Cell<bool>,
    diff_pending: Cell<bool>,
    diff_gen: Cell<u64>,
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
    /// The editor area and the assistant panel (created at first use).
    agent_paned: gtk::Paned,
    assistant: RefCell<Option<Rc<agentui::AgentPanel>>>,
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
    /// Cached symbols of the active document (for fast local completion).
    doc_symbols: RefCell<Option<inspect::DocSymbols>>,
    /// Completion popup and its pending debounce timer.
    completion: RefCell<Option<Rc<inspect::Completion>>>,
    completion_timer: Cell<Option<glib::SourceId>>,
    /// Running language servers.
    lsp_clients: RefCell<Vec<Rc<crate::lsp::Client>>>,
    /// Servers that could not start in this folder (not tried again).
    lsp_failed: RefCell<std::collections::HashSet<&'static str>>,
    /// Progress text of each server.
    lsp_status: RefCell<std::collections::HashMap<&'static str, String>>,
    /// Documents with changes not yet sent to their server.
    lsp_dirty: RefCell<std::collections::HashMap<String, lspui::PendingChanges>>,
    lsp_flush_scheduled: Cell<bool>,
    /// Latest diagnostics of the servers, the running "Verifica il file"
    /// and the problems listed in its panel.
    lsp_diags: RefCell<problems::DiagStore>,
    lsp_check: RefCell<Option<problems::Check>>,
    problems: problems::ProblemsUi,
    problems_shown: RefCell<problems::Shown>,
    /// Files changed by other programs: bar and documents waiting for a check.
    disk_bar: external::DiskBar,
    disk_pending: RefCell<std::collections::HashSet<String>>,
    /// Open "history of a file" window.
    history: RefCell<Option<Rc<history::History>>>,
    /// Socket answering the password questions of git and ssh (started at
    /// the first push, pull or fetch) and whether the user cancelled one.
    askpass: RefCell<Option<crate::askpass::Server>>,
    askpass_cancelled: Cell<bool>,
    /// The settings changed and are written shortly.
    settings_save_pending: Cell<bool>,
    /// Working folder and whether its language servers are enabled.
    lsp_enabled_cache: RefCell<Option<(PathBuf, bool)>>,
    /// Session text and cached documents last written by `persist`.
    persisted: RefCell<(String, Vec<String>)>,
    /// Files of the tabs when their labels were last computed, and the
    /// width of a character of the tab labels (0: not measured yet).
    tab_paths: RefCell<Vec<Option<PathBuf>>>,
    tab_char_px: Cell<i32>,
    /// Open compare-and-merge windows.
    merges: RefCell<Vec<Rc<merge::Merge>>>,
}

/// For each path, the last folders that tell it from the other paths with
/// the same file name (empty if the name is unique).
fn tab_contexts(paths: &[Option<PathBuf>]) -> Vec<String> {
    let folders = |p: &Path| -> Vec<String> {
        p.parent().map(|d| d.iter().map(|c| c.to_string_lossy().into_owned()).collect()).unwrap_or_default()
    };
    paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let Some(p) = p else { return String::new() };
            let others: Vec<&PathBuf> =
                paths.iter().enumerate().filter(|(j, q)| *j != i && q.as_ref().is_some_and(|q| q.file_name() == p.file_name() && q != p)).filter_map(|(_, q)| q.as_ref()).collect();
            if others.is_empty() {
                return String::new();
            }
            let mine = folders(p);
            let tail = |f: &[String], k: usize| f[f.len().saturating_sub(k)..].to_vec();
            let k = (1..=mine.len().min(4))
                .find(|&k| others.iter().all(|o| tail(&folders(o), k) != tail(&mine, k)))
                .unwrap_or(mine.len().min(4));
            tail(&mine, k).join("/")
        })
        .collect()
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
    pub fn new(app: &gtk::Application, settings: Settings) -> Rc<Editor> {
        let first_run = !Settings::exists();
        let mut settings = settings;
        if first_run {
            // Follow the system theme the first time.
            settings.dark = prefs::system_prefers_dark();
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
        let shared_menus = menus::SharedMenus::new();
        let (status_box, status) = Self::build_status_bar(&shared_menus);
        let problems = problems::build_panel();
        let disk_bar = external::build_bar();

        let recent_menu = gio::Menu::new();
        let bookmarks_menu = gio::Menu::new();
        let menubar = gtk::PopoverMenuBar::from_model(Some(&Self::build_menu(&recent_menu, &bookmarks_menu, &shared_menus)));

        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        vbox.append(&menubar);
        vbox.append(&disk_bar.revealer);
        let agent_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        agent_paned.set_start_child(Some(&hpaned));
        agent_paned.set_shrink_start_child(false);
        agent_paned.set_shrink_end_child(false);
        agent_paned.set_resize_end_child(false);
        agent_paned.set_wide_handle(true);
        agent_paned.set_vexpand(true);
        vbox.append(&agent_paned);
        vbox.append(&problems.revealer);
        vbox.append(&search_box);
        vbox.append(&status_box);
        win.set_child(Some(&vbox));

        let lang_action = gio::SimpleAction::new_stateful("lang", Some(glib::VariantTy::STRING), &"plain".to_variant());
        let eol_action = gio::SimpleAction::new_stateful("eol", Some(glib::VariantTy::STRING), &"lf".to_variant());

        // Rules that never change are parsed once; `css` holds the font and
        // the colors, loaded again when the settings change.
        let display = gtk::prelude::WidgetExt::display(&win);
        let static_css = gtk::CssProvider::new();
        static_css.load_from_string(prefs::STATIC_CSS);
        gtk::style_context_add_provider_for_display(&display, &static_css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let css = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

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
            bookmarks: RefCell::new(config::load_bookmarks()),
            bookmarks_menu,
            search,
            status,
            lang_action,
            eol_action,
            layout_pending: Cell::new(false),
            autosave_pending: Cell::new(false),
            autosave_writing: Cell::new(false),
            autosave_again: Cell::new(false),
            message_gen: Rc::new(Cell::new(0)),
            shut_down: Cell::new(false),
            diff_on: Cell::new(false),
            diff_pending: Cell::new(false),
            diff_gen: Cell::new(0),
            diff_pair: RefCell::new(Vec::new()),
            diff_result: RefCell::new(None),
            syncing: Cell::new(false),
            restoring: Cell::new(false),
            vpaned,
            terminal: RefCell::new(None),
            term_accels: RefCell::new(Vec::new()),
            hpaned,
            agent_paned,
            assistant: RefCell::new(None),
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
            doc_symbols: RefCell::new(None),
            completion: RefCell::new(None),
            completion_timer: Cell::new(None),
            lsp_clients: RefCell::new(Vec::new()),
            lsp_failed: RefCell::new(Default::default()),
            lsp_status: RefCell::new(Default::default()),
            lsp_dirty: RefCell::new(Default::default()),
            lsp_flush_scheduled: Cell::new(false),
            lsp_diags: RefCell::new(Default::default()),
            lsp_check: RefCell::new(None),
            problems,
            problems_shown: RefCell::new(Default::default()),
            disk_bar,
            disk_pending: RefCell::new(Default::default()),
            history: RefCell::new(None),
            askpass: RefCell::new(None),
            askpass_cancelled: Cell::new(false),
            settings_save_pending: Cell::new(false),
            lsp_enabled_cache: RefCell::new(None),
            persisted: RefCell::new((String::new(), Vec::new())),
            tab_paths: RefCell::new(Vec::new()),
            tab_char_px: Cell::new(0),
            merges: RefCell::new(Vec::new()),
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

    // ------------------------------------------------------------- actions

    /// Adds the window action `name`, with its shortcuts, running `f`.
    pub(super) fn add_action(self: &Rc<Self>, name: &str, accels: &[&str], f: impl Fn(&Rc<Editor>) + 'static) {
        let a = gio::SimpleAction::new(name, None);
        let ed = self.clone();
        a.connect_activate(move |_, _| f(&ed));
        self.win.add_action(&a);
        if !accels.is_empty() {
            self.app.set_accels_for_action(&format!("win.{name}"), accels);
        }
    }

    /// Adds the window action `name` taking a string.
    pub(super) fn add_str_action(self: &Rc<Self>, name: &str, f: impl Fn(&Rc<Editor>, String) + 'static) {
        let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let ed = self.clone();
        a.connect_activate(move |_, v| {
            if let Some(s) = v.and_then(|v| v.get::<String>()) {
                f(&ed, s);
            }
        });
        self.win.add_action(&a);
    }

    /// Adds the window action `name` with a check mark (off at first), run
    /// by `f`, which also updates the mark.
    fn add_toggle(self: &Rc<Self>, name: &str, accels: &[&str], f: impl Fn(&Rc<Editor>) + 'static) {
        let a = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
        let ed = self.clone();
        a.connect_activate(move |_, _| f(&ed));
        self.win.add_action(&a);
        self.app.set_accels_for_action(&format!("win.{name}"), accels);
    }

    /// The window action `name`.
    pub(super) fn action(&self, name: &str) -> Option<gio::SimpleAction> {
        self.win.lookup_action(name).and_downcast::<gio::SimpleAction>()
    }

    /// Check mark of the window action `name`.
    pub(super) fn set_action_state(&self, name: &str, on: bool) {
        if let Some(a) = self.action(name) {
            a.set_state(&on.to_variant());
        }
    }

    fn setup_actions(self: &Rc<Self>) {
        let win = &self.win;
        macro_rules! act {
            ($name:expr, $accels:expr, $f:expr) => {
                self.add_action($name, &$accels, $f)
            };
        }
        macro_rules! act_str {
            ($name:expr, $f:expr) => {
                self.add_str_action($name, $f)
            };
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
        act!("join-lines", ["<Control>j"], |ed: &Rc<Editor>| ed.join_lines());
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

        act!("fold", ["<Control><Shift>minus", "<Control><Shift>KP_Subtract", "<Control><Shift>bracketleft"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.foldable_doc()
                && !d.fold_at_cursor()
            {
                ed.message("Nessun blocco da piegare qui");
            }
        });
        act!("unfold", ["<Control><Shift>plus", "<Control><Shift>equal", "<Control><Shift>KP_Add", "<Control><Shift>bracketright"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.foldable_doc()
                && !d.unfold_at_cursor()
            {
                ed.message("Nessun blocco piegato sulla riga del cursore");
            }
        });
        act!("fold-all", ["<Control><Alt>minus", "<Control><Alt>KP_Subtract"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.foldable_doc() {
                let n = d.fold_all();
                ed.message(&match n {
                    0 => "Nessun blocco da piegare".to_string(),
                    1 => "1 blocco piegato".to_string(),
                    n => format!("{n} blocchi piegati"),
                });
            }
        });
        act!("unfold-all", ["<Control><Alt>plus", "<Control><Alt>equal", "<Control><Alt>KP_Add"], |ed: &Rc<Editor>| {
            if let Some(d) = ed.active_doc() {
                d.unfold_all();
            }
        });

        act!("split", ["<Control>backslash", "F8"], |ed: &Rc<Editor>| ed.toggle_split());
        // Panels with a check mark (set by the functions they call).
        self.add_toggle("diff", &["F9"], |ed| ed.set_diff(!ed.diff_on.get()));
        self.add_toggle("terminal", &["F12", "<Control>grave"], |ed| ed.set_terminal(!ed.terminal_visible()));
        act!("terminal-here", ["<Shift>F12"], |ed: &Rc<Editor>| ed.terminal_at(ed.doc_dir()));
        self.add_toggle("sidebar", &["<Control>b", "F7"], |ed| ed.set_sidebar(!ed.sidebar_visible()));
        act!("open-folder", ["<Control><Shift>o"], |ed: &Rc<Editor>| ed.open_folder_dialog());
        act!("close-folder", [], |ed: &Rc<Editor>| ed.close_workdir());
        act!("merge", ["<Control>F9"], |ed: &Rc<Editor>| ed.merge_tabs());
        act!("merge-with-file", [], |ed: &Rc<Editor>| ed.merge_with_file());
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
        self.setup_assistant_actions();
        self.setup_problems_actions();
        self.setup_bookmark_actions();
        self.setup_disk_actions();
    }

    fn connect_ui(self: &Rc<Self>) {
        // Back to the editor: files may have been changed and git used
        // elsewhere in the meantime.
        let ed = Rc::downgrade(self);
        self.win.connect_is_active_notify(move |w| {
            if let (true, Some(ed)) = (w.is_active(), ed.upgrade()) {
                ed.check_disk_all();
                ed.git_refresh();
                ed.update_memory();
            }
        });
        for (pane, nb) in self.notebooks.iter().enumerate() {
            let ed = self.clone();
            nb.connect_switch_page(move |_, page, _| {
                ed.active_pane.set(pane);
                if let Some(d) = ed.doc_for_widget(page) {
                    let was_loaded = d.is_loaded();
                    if let Err(e) = d.ensure_loaded() {
                        ed.message(&format!("Errore: {e}"));
                    }
                    d.hidden_since.set(None);
                    ed.update_status_for(&d);
                    ed.sync_sidebar();
                    ed.git_tab_switched();
                    ed.hide_completion();
                    // Starts the servers of the language if needed.
                    ed.lsp_open(&d);
                    if !was_loaded && d.is_loaded() {
                        // Servers that kept it while unloaded get the text again.
                        ed.lsp_changed(&d);
                    }
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
        self.win.connect_close_request(move |win| {
            // Tabs are kept in the session, the result of a merge is not.
            let pending = ed.merges.borrow().iter().find(|m| m.is_modified()).cloned();
            if let Some(m) = pending {
                m.win.present();
                let (ed2, win2) = (ed.clone(), win.clone());
                let detail = "Uscendo, le scelte fatte nella finestra di confronto e unione andranno perse.";
                crate::ui::confirm(Some(&m.win), "Il risultato di un confronto non è salvato", detail, "Esci comunque", false, move |ok| {
                    if ok {
                        for m in ed2.merges.borrow().iter() {
                            m.discard();
                        }
                        win2.close();
                    }
                });
                return glib::Propagation::Stop;
            }
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
                    buf.place_cursor(&d.iter_at_line_col(line - 1, col - 1));
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
            // Nobody looks at it while the window is in the background (it
            // is updated again when the window is activated).
            if ed.win.is_active() {
                ed.update_memory();
            }
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
        self.new_doc_with_id(config::new_id())
    }

    /// A new document using the cache file `id`.
    fn new_doc_with_id(self: &Rc<Self>, id: String) -> Rc<Doc> {
        let d = Doc::new(&self.shared, id);
        self.wire_doc(&d);
        self.docs.borrow_mut().push(d.clone());
        d
    }

    /// Window-level signal handlers of a document.
    /// A signal handler calling `f` with the editor and `d`, holding only
    /// weak references to them.
    fn doc_handler(self: &Rc<Self>, d: &Rc<Doc>, f: impl Fn(&Rc<Editor>, &Rc<Doc>) + 'static) -> impl Fn() + 'static {
        let (ed, wd) = (Rc::downgrade(self), d.weak());
        move || {
            if let (Some(ed), Some(d)) = (ed.upgrade(), wd.upgrade()) {
                f(&ed, &d);
            }
        }
    }

    fn wire_doc(self: &Rc<Self>, d: &Rc<Doc>) {
        self.wire_inspect(d);
        self.watch_doc(d);
        let close = self.doc_handler(d, |ed, d| ed.close_doc(d));
        d.close_button.connect_clicked(move |_| close());
        let middle = gtk::GestureClick::builder().button(2).build();
        let close = self.doc_handler(d, |ed, d| ed.close_doc(d));
        middle.connect_released(move |_, _, _, _| close());
        d.tab.add_controller(middle);

        let focus = gtk::EventControllerFocus::new();
        let entered = self.doc_handler(d, |ed, d| {
            if let Some(p) = ed.pane_of(d) {
                ed.active_pane.set(p);
            }
            ed.update_status_for(d);
        });
        focus.connect_enter(move |_| entered());
        d.view.add_controller(focus);

        let changed = self.doc_handler(d, |ed, d| {
            if d.is_loaded() {
                ed.schedule_autosave();
                if ed.in_diff(d) {
                    ed.schedule_diff();
                }
            }
        });
        d.buffer.connect_changed(move |_| changed());
        let moved = self.doc_handler(d, |ed, d| {
            if ed.is_active(d) {
                ed.update_pos(d);
            }
        });
        d.buffer.connect_cursor_position_notify(move |_| moved());
        let modified = self.doc_handler(d, |ed, d| {
            if ed.is_active(d) {
                ed.update_title(d);
            }
        });
        d.buffer.connect_modified_changed(move |_| modified());

        // Diff mode: the other view follows the scrolling.
        let scrolled = self.doc_handler(d, |ed, d| ed.sync_scroll(d));
        d.scroller.vadjustment().connect_value_changed(move |_| scrolled());

        // Ctrl + mouse wheel zooms.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ed = Rc::downgrade(self);
        // A touchpad sends many small deltas: one step per unit of scroll,
        // like a wheel notch.
        let acc = Cell::new(0.0f64);
        scroll.connect_scroll(move |c, _, dy| {
            if c.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                let total = acc.get() + dy;
                let steps = total.trunc();
                acc.set(total - steps);
                if steps != 0.0
                    && let Some(ed) = ed.upgrade()
                {
                    ed.zoom(-steps as i32);
                }
                return glib::Propagation::Stop;
            }
            acc.set(0.0);
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

    /// Whether `d` is the active document (`active_doc` without looking
    /// for the document among all the open ones: called at every change).
    fn is_active(&self, d: &Rc<Doc>) -> bool {
        let page = |pane: usize| {
            let nb = &self.notebooks[pane];
            nb.current_page().and_then(|i| nb.nth_page(Some(i)))
        };
        let p = self.active_pane.get();
        page(p).or_else(|| page(1 - p)).is_some_and(|w| w == *d.scroller.upcast_ref::<gtk::Widget>())
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
        d.lang.set(lang::detect(Some(&path), &text));
        d.set_content(&text, false, 0);
        if reuse.is_none() {
            self.add_doc(&d, self.active_pane.get());
        } else {
            d.view.grab_focus();
        }
        self.add_recent(&path);
        self.update_status_for(&d);
        self.sync_sidebar();
        // Opened in the empty tab no tab switch happens: the repository
        // shown may change all the same.
        self.git_tab_switched();
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

    /// Ask before overwriting a file changed by another program, then
    /// warn about characters the target encoding cannot represent.
    fn save_checked(self: &Rc<Self>, d: &Rc<Doc>, path: PathBuf, then: Option<Box<dyn FnOnce(bool)>>) {
        let (ed, d2, p2) = (self.clone(), d.clone(), path.clone());
        self.confirm_overwrite(
            d,
            &path,
            Box::new(move |go| {
                if go {
                    ed.save_encoded(&d2, p2, then);
                } else if let Some(t) = then {
                    t(false);
                }
            }),
        );
    }

    fn save_encoded(self: &Rc<Self>, d: &Rc<Doc>, path: PathBuf, then: Option<Box<dyn FnOnce(bool)>>) {
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
                    let l = lang::detect(Some(path), &d.line_text(0));
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
        let detail = "Il file verrà riletto dal disco e le modifiche non salvate andranno perse.";
        crate::ui::confirm(Some(self.win.upcast_ref()), "Scartare le modifiche?", detail, "Rileggi", false, move |ok| {
            if ok {
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

    // ------------------------------------------------------------ search

    // -------------------------------------------------------------- diff

    // ------------------------------------------------------ panes & tabs

    fn toggle_split(self: &Rc<Self>) {
        let right = &self.notebooks[1];
        if right.is_visible() {
            self.collapse_split();
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
            self.collapse_split();
            self.schedule_layout();
        }
    }

    /// Moves every tab back into the left pane and hides the right one.
    fn collapse_split(self: &Rc<Self>) {
        let docs: Vec<Rc<Doc>> = self.docs.borrow().iter().filter(|d| self.pane_of(d) == Some(1)).cloned().collect();
        for d in docs {
            self.move_doc(&d, 0);
        }
        self.notebooks[1].set_visible(false);
        self.active_pane.set(0);
        self.set_diff(false);
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
        // Tabs with the same file name (Next.js `page.tsx`, `index.js`…)
        // also show their folders: computed again only when the files of
        // the tabs change, not at every frame of a resize.
        {
            let docs = self.docs.borrow();
            let paths: Vec<Option<PathBuf>> = docs.iter().map(|d| d.path.borrow().clone()).collect();
            let mut last = self.tab_paths.borrow_mut();
            if *last != paths {
                let contexts = tab_contexts(&paths);
                for (d, c) in docs.iter().zip(contexts) {
                    d.set_label_context(c);
                }
                *last = paths;
            }
        }
        // Average width of a character of the interface font.
        let char_px = match self.tab_char_px.get() {
            0 => {
                let layout = self.win.create_pango_layout(Some("abcdefghijklmnopqrstuvwxyz"));
                let px = (layout.pixel_size().0 / 26).max(4);
                self.tab_char_px.set(px);
                px
            }
            px => px,
        };
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

    // ------------------------------------------------------------ terminal

    // ---------------------------------------------------------- file panel

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
        let kb = crate::proc::rss_kb("self");
        let fmt = |kb: u64| {
            let mb = kb as f64 / 1024.0;
            if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
        };
        let lsp_kb = self.lsp_memory_kb();
        let agent_kb = self.assistant_memory_kb();
        let mut text = format!("RAM {}", fmt(kb));
        if lsp_kb > 0 {
            text.push_str(&format!(" + LSP {}", fmt(lsp_kb)));
        }
        if agent_kb > 0 {
            text.push_str(&format!(" + agente {}", fmt(agent_kb)));
        }
        self.status.mem.set_text(&text);
        let docs = self.docs.borrow();
        let loaded = docs.iter().filter(|d| d.is_loaded()).count();
        self.status.mem.set_tooltip_text(Some(&format!(
            "Memoria residente del processo (RSS): {kb} kB\nLanguage server: {lsp_kb} kB\nAgente dell'assistente: {agent_kb} kB\nSchede caricate in memoria: {loaded} di {}",
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
        self.update_bookmark_state();
        // The file may have changed while another tab was shown.
        if d.is_loaded() {
            d.disk_change.set(d.check_disk());
        }
        self.update_disk_bar();
    }

    fn update_pos(&self, d: &Doc) {
        let it = d.cursor_iter();
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

    // ------------------------------------------------- settings & fonts

    // ------------------------------------------------ session & caching

    pub fn present(&self) {
        self.win.present();
        if let Some(d) = self.active_doc() {
            d.view.grab_focus();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts() {
        let p = |s: &str| Some(PathBuf::from(s));
        let c = tab_contexts(&[p("/w/app/dashboard/page.tsx"), p("/w/app/blog/page.tsx"), p("/w/app/layout.tsx"), None]);
        assert_eq!(c, ["dashboard", "blog", "", ""]);
        let c = tab_contexts(&[p("/a/src/index.js"), p("/b/src/index.js")]);
        assert_eq!(c, ["a/src", "b/src"]);
    }
}
