//! Side panel: file browser rooted at the working folder, or at the folder
//! of the active document when no working folder is set.
//!
//! Folders are listed with `GtkDirectoryList` (asynchronous and watched for
//! changes) and only when expanded. Files changed according to git are
//! marked with a letter and a color.

use crate::git::{self, Mark, Status};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

const ATTRS: &str = "standard::name,standard::display-name,standard::type,standard::is-hidden,standard::is-backup,standard::symbolic-icon";

/// What the panel asks the editor to do.
#[allow(clippy::type_complexity)]
pub struct Handlers {
    pub open: Box<dyn Fn(&Path)>,
    pub terminal: Box<dyn Fn(&Path)>,
    pub set_workdir: Box<dyn Fn(Option<PathBuf>)>,
    pub renamed: Box<dyn Fn(&Path, &Path)>,
    pub error: Box<dyn Fn(&str, &str)>,
    /// Git action ("add", "unstage", "restore") on a path.
    pub git: Box<dyn Fn(&str, &Path)>,
    /// Add a file or folder to the bookmarks.
    pub bookmark: Box<dyn Fn(&Path)>,
    /// Git history of a file.
    pub history: Box<dyn Fn(&Path)>,
}

/// A row widget of the list (rows are recycled while scrolling).
struct RowRef {
    row: glib::WeakRef<gtk::Box>,
    mark: glib::WeakRef<gtk::Label>,
    /// Item shown by the row and whether it is a folder.
    item: Option<(PathBuf, bool)>,
}

pub struct Sidebar {
    pub widget: gtk::Box,
    title: gtk::Label,
    up: gtk::Button,
    view: gtk::ListView,
    selection: gtk::SingleSelection,
    tree: RefCell<Option<gtk::TreeListModel>>,
    root: RefCell<PathBuf>,
    /// The root is the working folder.
    workdir: Cell<bool>,
    show_hidden: Cell<bool>,
    /// Item the menus act on (None: the root folder).
    context: RefCell<Option<PathBuf>>,
    menu: gtk::PopoverMenu,
    actions: gio::SimpleActionGroup,
    /// File to select once its folders have been listed.
    pending_reveal: RefCell<Option<PathBuf>>,
    reveal_scheduled: Cell<bool>,
    revealing: Cell<bool>,
    handlers: Handlers,
    git: RefCell<Option<Rc<Status>>>,
    rows: RefCell<Vec<RowRef>>,
}

fn info_path(info: &gio::FileInfo) -> Option<PathBuf> {
    info.attribute_object("standard::file").and_downcast::<gio::File>().and_then(|f| f.path())
}

fn row_info(o: &glib::Object) -> Option<gio::FileInfo> {
    o.downcast_ref::<gtk::TreeListRow>()?.item().and_downcast::<gio::FileInfo>()
}

fn is_dir(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Directory
}

/// `git`: entries (label, action) of the git section.
fn build_menu(context: bool, git: &[(&str, &str)]) -> gio::Menu {
    let menu = gio::Menu::new();
    let s1 = gio::Menu::new();
    if context {
        s1.append(Some("Apri"), Some("side.open"));
    }
    s1.append(Some("Apri terminale qui"), Some("side.terminal"));
    menu.append_section(None, &s1);
    let s2 = gio::Menu::new();
    s2.append(Some("Nuovo file…"), Some("side.new-file"));
    s2.append(Some("Nuova cartella…"), Some("side.new-folder"));
    menu.append_section(None, &s2);
    if context {
        let s3 = gio::Menu::new();
        s3.append(Some("Rinomina…"), Some("side.rename"));
        s3.append(Some("Sposta nel cestino"), Some("side.trash"));
        s3.append(Some("Copia percorso"), Some("side.copy-path"));
        s3.append(Some("Aggiungi ai preferiti"), Some("side.bookmark"));
        s3.append(Some("Cronologia git…"), Some("side.history"));
        menu.append_section(None, &s3);
    }
    let sg = gio::Menu::new();
    for (label, action) in git {
        sg.append(Some(label), Some(action));
    }
    if !context && git.is_empty() {
        sg.append(Some("Git: modifiche, commit e push…"), Some("side.git-changes"));
    }
    if sg.n_items() > 0 {
        menu.append_section(None, &sg);
    }
    let s4 = gio::Menu::new();
    s4.append(Some("Usa come cartella di lavoro"), Some("side.set-workdir"));
    if !context {
        s4.append(Some("Chiudi cartella di lavoro"), Some("side.close-workdir"));
        s4.append(Some("Mostra file nascosti"), Some("side.hidden"));
        s4.append(Some("Aggiorna"), Some("side.refresh"));
    }
    menu.append_section(None, &s4);
    menu
}

impl Sidebar {
    pub fn new(handlers: Handlers) -> Rc<Sidebar> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("sidebar");
        widget.set_width_request(120);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        header.add_css_class("sidebar-header");
        let up = gtk::Button::from_icon_name("go-up-symbolic");
        up.set_tooltip_text(Some("Cartella superiore"));
        up.add_css_class("flat");
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Azioni sulla cartella")
            .menu_model(&build_menu(false, &[]))
            .build();
        menu_button.set_has_frame(false);
        header.append(&up);
        header.append(&title);
        header.append(&menu_button);

        let selection = gtk::SingleSelection::new(None::<gio::ListModel>);
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let view = gtk::ListView::new(Some(selection.clone()), None::<gtk::ListItemFactory>);
        view.add_css_class("navigation-sidebar");
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&view)
            .build();
        widget.append(&header);
        widget.append(&scroller);

        let menu = gtk::PopoverMenu::from_model(Some(&build_menu(true, &[])));
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_parent(&widget);

        let s = Rc::new(Sidebar {
            widget,
            title,
            up,
            view,
            selection,
            tree: RefCell::new(None),
            root: RefCell::new(PathBuf::new()),
            workdir: Cell::new(false),
            show_hidden: Cell::new(false),
            context: RefCell::new(None),
            menu,
            actions: gio::SimpleActionGroup::new(),
            pending_reveal: RefCell::new(None),
            reveal_scheduled: Cell::new(false),
            revealing: Cell::new(false),
            handlers,
            git: RefCell::new(None),
            rows: RefCell::new(Vec::new()),
        });
        s.view.set_factory(Some(&s.factory()));
        s.setup_actions();

        let w = Rc::downgrade(&s);
        s.up.connect_clicked(move |_| {
            if let Some(s) = w.upgrade() {
                let parent = s.root.borrow().parent().map(Path::to_path_buf);
                if let Some(p) = parent {
                    s.set_root(&p, false);
                }
            }
        });
        let w = Rc::downgrade(&s);
        menu_button.connect_active_notify(move |b| {
            if let (true, Some(s)) = (b.is_active(), w.upgrade()) {
                s.set_context(None);
            }
        });
        let w = Rc::downgrade(&s);
        s.view.connect_activate(move |_, pos| {
            if let Some(s) = w.upgrade() {
                s.activate(pos);
            }
        });
        // Right click on the empty area: actions on the root folder.
        let g = gtk::GestureClick::new();
        g.set_button(gdk::BUTTON_SECONDARY);
        let w = Rc::downgrade(&s);
        g.connect_pressed(move |g, _, x, y| {
            if let (Some(s), Some(widget)) = (w.upgrade(), g.widget()) {
                s.popup(None, &widget, x, y);
            }
        });
        s.view.add_controller(g);
        // F2: rename, Delete: move to the trash.
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&s);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(s) = w.upgrade() else { return glib::Propagation::Proceed };
            let mods = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
            if state.intersects(mods) {
                return glib::Propagation::Proceed;
            }
            let action = match key {
                gdk::Key::F2 => "rename",
                gdk::Key::Delete | gdk::Key::KP_Delete => "trash",
                _ => return glib::Propagation::Proceed,
            };
            let Some(path) = s.selected_path() else { return glib::Propagation::Proceed };
            s.set_context(Some(path));
            gtk::prelude::ActionGroupExt::activate_action(&s.actions, action, None);
            glib::Propagation::Stop
        });
        s.view.add_controller(keys);
        s
    }

    fn factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let icon = gtk::Image::new();
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let mark = gtk::Label::new(None);
            mark.add_css_class("git-mark");
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            b.append(&icon);
            b.append(&label);
            b.append(&mark);
            if let Some(s) = weak.upgrade() {
                let mut rows = s.rows.borrow_mut();
                rows.retain(|r| r.row.upgrade().is_some());
                rows.push(RowRef { row: b.downgrade(), mark: mark.downgrade(), item: None });
            }
            let exp = gtk::TreeExpander::new();
            exp.set_child(Some(&b));
            li.set_child(Some(&exp));

            let g = gtk::GestureClick::new();
            g.set_button(gdk::BUTTON_SECONDARY);
            let weak: Weak<Sidebar> = weak.clone();
            let liw = li.downgrade();
            g.connect_pressed(move |g, _, x, y| {
                g.set_state(gtk::EventSequenceState::Claimed);
                let (Some(s), Some(li), Some(widget)) = (weak.upgrade(), liw.upgrade(), g.widget()) else { return };
                let path = li.item().and_then(|o| row_info(&o)).and_then(|i| info_path(&i));
                s.selection.set_selected(li.position());
                s.popup(path, &widget, x, y);
            });
            exp.add_controller(g);
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let Some(li) = item.downcast_ref::<gtk::ListItem>() else { return };
            let Some(row) = li.item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(exp) = li.child().and_downcast::<gtk::TreeExpander>() else { return };
            exp.set_list_row(Some(&row));
            let Some(info) = row.item().and_downcast::<gio::FileInfo>() else { return };
            let Some(b) = exp.child() else { return };
            if let Some(icon) = b.first_child().and_downcast::<gtk::Image>() {
                match info.symbolic_icon() {
                    Some(gi) => icon.set_from_gicon(&gi),
                    None => icon.set_icon_name(Some(if is_dir(&info) { "folder-symbolic" } else { "text-x-generic-symbolic" })),
                }
            }
            if let Some(label) = b.first_child().and_then(|i| i.next_sibling()).and_downcast::<gtk::Label>() {
                label.set_text(&info.display_name());
            }
            if let (Some(s), Some(b), Some(path)) = (weak.upgrade(), b.downcast_ref::<gtk::Box>(), info_path(&info)) {
                s.row_bound(b, path, is_dir(&info));
            }
        });
        factory.connect_unbind(|_, item| {
            if let Some(exp) = item.downcast_ref::<gtk::ListItem>().and_then(|li| li.child()).and_downcast::<gtk::TreeExpander>() {
                exp.set_list_row(None);
            }
        });
        factory
    }

    fn setup_actions(self: &Rc<Self>) {
        let group = &self.actions;
        let add = |name: &str, f: fn(&Rc<Sidebar>)| {
            let a = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(s) = w.upgrade() {
                    f(&s);
                }
            });
            group.add_action(&a);
        };
        add("open", |s| {
            if let Some(p) = s.context.borrow().clone() {
                (s.handlers.open)(&p);
            }
        });
        add("terminal", |s| (s.handlers.terminal)(&s.context_dir()));
        add("new-file", |s| s.new_file());
        add("new-folder", |s| s.new_folder());
        add("rename", |s| s.rename());
        add("trash", |s| s.trash());
        add("copy-path", |s| {
            let p = s.context.borrow().clone().unwrap_or_else(|| s.root());
            s.widget.clipboard().set_text(&p.to_string_lossy());
        });
        add("set-workdir", |s| (s.handlers.set_workdir)(Some(s.context_dir())));
        add("bookmark", |s| {
            if let Some(p) = s.context.borrow().clone() {
                (s.handlers.bookmark)(&p);
            }
        });
        add("history", |s| {
            if let Some(p) = s.context.borrow().clone() {
                (s.handlers.history)(&p);
            }
        });
        for name in ["add", "unstage", "restore", "changes", "resolve"] {
            let a = gio::SimpleAction::new(&format!("git-{name}"), None);
            let w = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(s) = w.upgrade() {
                    let p = s.context.borrow().clone().unwrap_or_else(|| s.root());
                    (s.handlers.git)(name, &p);
                }
            });
            group.add_action(&a);
        }
        add("close-workdir", |s| (s.handlers.set_workdir)(None));
        add("refresh", |s| {
            s.load();
            (s.handlers.git)("refresh", &s.root());
        });
        let hidden = gio::SimpleAction::new_stateful("hidden", None, &false.to_variant());
        let w = Rc::downgrade(self);
        hidden.connect_activate(move |a, _| {
            let Some(s) = w.upgrade() else { return };
            s.show_hidden.set(!s.show_hidden.get());
            a.set_state(&s.show_hidden.get().to_variant());
            s.load();
        });
        group.add_action(&hidden);
        self.widget.insert_action_group("side", Some(group));
    }

    fn enable(&self, name: &str, on: bool) {
        if let Some(a) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            a.set_enabled(on);
        }
    }

    /// Sets the item the menus act on and updates which actions apply.
    fn set_context(&self, path: Option<PathBuf>) {
        let file = path.as_ref().is_some_and(|p| !p.is_dir());
        self.enable("open", file);
        self.enable("rename", path.is_some());
        self.enable("trash", path.is_some());
        self.enable("set-workdir", !file && (path.is_some() || !self.workdir.get()));
        self.enable("close-workdir", self.workdir.get());
        let target = path.clone().unwrap_or_else(|| self.root());
        let in_repo = self.git.borrow().as_ref().is_some_and(|g| target.starts_with(&g.root));
        self.enable("git-changes", in_repo && target.is_dir());
        *self.context.borrow_mut() = path;
    }

    /// Folder the menu actions refer to.
    fn context_dir(&self) -> PathBuf {
        match self.context.borrow().as_ref() {
            Some(p) if p.is_dir() => p.clone(),
            Some(p) => p.parent().map_or_else(|| self.root(), Path::to_path_buf),
            None => self.root(),
        }
    }

    /// Git entries of the context menu for `path`, according to its status.
    fn git_items(&self, path: &Path) -> Vec<(&'static str, &'static str)> {
        let g = self.git.borrow();
        let Some(g) = g.as_ref().filter(|g| path.starts_with(&g.root)) else { return Vec::new() };
        let mut v = Vec::new();
        if path.is_dir() {
            match g.decoration(path, true).map(|d| d.0) {
                Some(Mark::Untracked) => v.push(("Git: aggiungi la cartella al repository (add)", "side.git-add")),
                Some(Mark::Staged) => v.push(("Git: rimuovi la cartella dallo stage", "side.git-unstage")),
                Some(_) => {
                    v.push(("Git: metti in stage le modifiche della cartella (add)", "side.git-add"));
                    v.push(("Git: rimuovi la cartella dallo stage", "side.git-unstage"));
                    v.push(("Git: annulla le modifiche nella cartella…", "side.git-restore"));
                }
                None => {}
            }
            v.push(("Git: modifiche, commit e push della cartella…", "side.git-changes"));
        } else if let Some(e) = g.entry(path) {
            match e.mark() {
                Mark::Untracked => v.push(("Git: aggiungi al repository (add)", "side.git-add")),
                Mark::Conflict => {
                    v.push(("Git: risolvi il conflitto…", "side.git-resolve"));
                    v.push(("Git: segna il conflitto come risolto (add)", "side.git-add"));
                }
                _ => {
                    if e.y != b' ' {
                        v.push(("Git: metti in stage le modifiche (add)", "side.git-add"));
                    }
                    if e.x != b' ' {
                        v.push(("Git: rimuovi dallo stage", "side.git-unstage"));
                    }
                    if e.y != b' ' {
                        v.push(("Git: annulla le modifiche…", "side.git-restore"));
                    }
                }
            }
        }
        v
    }

    fn popup(&self, path: Option<PathBuf>, widget: &gtk::Widget, x: f64, y: f64) {
        let git = self.git_items(path.as_deref().unwrap_or(&self.root()));
        self.menu.set_menu_model(Some(&build_menu(path.is_some(), &git)));
        self.set_context(path);
        let Some(p) = widget.compute_point(&self.widget, &gtk::graphene::Point::new(x as f32, y as f32)) else { return };
        self.menu.set_pointing_to(Some(&gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1)));
        self.menu.popup();
    }

    pub fn root(&self) -> PathBuf {
        self.root.borrow().clone()
    }

    /// Shows `dir`; `workdir` tells whether it is the working folder.
    pub fn set_root(self: &Rc<Self>, dir: &Path, workdir: bool) {
        if *self.root.borrow() == dir && self.workdir.get() == workdir && self.tree.borrow().is_some() {
            return;
        }
        *self.root.borrow_mut() = dir.to_path_buf();
        self.workdir.set(workdir);
        let name = dir.file_name().map_or_else(|| dir.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        self.title.set_text(&name);
        self.title.set_tooltip_text(Some(&format!(
            "{}{}",
            dir.display(),
            if workdir { "\n(cartella di lavoro)" } else { "" }
        )));
        if workdir {
            self.title.add_css_class("workdir");
        } else {
            self.title.remove_css_class("workdir");
        }
        self.up.set_sensitive(!workdir && dir.parent().is_some());
        self.load();
    }

    /// Lists the files in `dir` (sorted, folders first).
    fn dir_model(&self, dir: &gio::File) -> gio::ListModel {
        let list = gtk::DirectoryList::new(Some(ATTRS), Some(dir));
        let show_hidden = self.show_hidden.get();
        let filter = gtk::CustomFilter::new(move |o| {
            let Some(i) = o.downcast_ref::<gio::FileInfo>() else { return false };
            show_hidden || !(i.is_hidden() || i.is_backup() || i.name().to_string_lossy().starts_with('.'))
        });
        let filtered = gtk::FilterListModel::new(Some(list), Some(filter));
        let sorter = gtk::CustomSorter::new(|a, b| {
            let (Some(a), Some(b)) = (a.downcast_ref::<gio::FileInfo>(), b.downcast_ref::<gio::FileInfo>()) else {
                return gtk::Ordering::Equal;
            };
            is_dir(b)
                .cmp(&is_dir(a))
                .then_with(|| a.display_name().to_lowercase().cmp(&b.display_name().to_lowercase()))
                .into()
        });
        gtk::SortListModel::new(Some(filtered), Some(sorter)).upcast()
    }

    /// Drops the listing of the current folder and all its watched
    /// subfolders.
    pub fn clear(&self) {
        self.selection.set_model(None::<&gio::ListModel>);
        *self.tree.borrow_mut() = None;
        *self.root.borrow_mut() = PathBuf::new();
        *self.pending_reveal.borrow_mut() = None;
        *self.context.borrow_mut() = None;
        self.workdir.set(false);
        self.title.set_text("");
        self.title.set_tooltip_text(None);
        self.title.remove_css_class("workdir");
        for r in self.rows.borrow_mut().iter_mut() {
            r.item = None;
        }
    }

    /// Lists the folder again.
    pub fn refresh(self: &Rc<Self>) {
        self.load();
    }

    fn load(self: &Rc<Self>) {
        // Keep the selected item selected in the new listing.
        if self.pending_reveal.borrow().is_none() {
            *self.pending_reveal.borrow_mut() = self.selected_path();
        }
        let root = gio::File::for_path(&*self.root.borrow());
        let weak = Rc::downgrade(self);
        let tree = gtk::TreeListModel::new(self.dir_model(&root), false, false, move |o| {
            let info = o.downcast_ref::<gio::FileInfo>()?;
            if !is_dir(info) {
                return None;
            }
            let file = info.attribute_object("standard::file").and_downcast::<gio::File>()?;
            weak.upgrade().map(|s| s.dir_model(&file))
        });
        let weak = Rc::downgrade(self);
        tree.connect_items_changed(move |_, _, _, _| {
            if let Some(s) = weak.upgrade() {
                s.schedule_reveal();
            }
        });
        self.selection.set_model(Some(&tree));
        *self.tree.borrow_mut() = Some(tree);
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selection.selected_item().and_then(|o| row_info(&o)).and_then(|i| info_path(&i))
    }

    fn activate(&self, pos: u32) {
        let Some(row) = self.selection.item(pos).and_downcast::<gtk::TreeListRow>() else { return };
        let Some(info) = row.item().and_downcast::<gio::FileInfo>() else { return };
        if is_dir(&info) {
            row.set_expanded(!row.is_expanded());
        } else if let Some(p) = info_path(&info) {
            (self.handlers.open)(&p);
        }
    }

    /// Selects `path`, expanding the folders that contain it.
    pub fn reveal(self: &Rc<Self>, path: &Path) {
        if !path.starts_with(&*self.root.borrow()) {
            return;
        }
        *self.pending_reveal.borrow_mut() = Some(path.to_path_buf());
        self.schedule_reveal();
    }

    /// The list view must see model changes before rows can be selected:
    /// revealing always runs from the main loop.
    fn schedule_reveal(self: &Rc<Self>) {
        if self.reveal_scheduled.get() || self.pending_reveal.borrow().is_none() {
            return;
        }
        self.reveal_scheduled.set(true);
        let w = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(s) = w.upgrade() {
                s.reveal_scheduled.set(false);
                s.try_reveal();
            }
        });
    }

    fn try_reveal(&self) {
        if self.revealing.get() {
            return;
        }
        let Some(target) = self.pending_reveal.borrow().clone() else { return };
        let Some(tree) = self.tree.borrow().clone() else { return };
        self.revealing.set(true);
        // Folders are listed asynchronously: if the file is not there yet,
        // this runs again when they change.
        let mut i = 0;
        while i < tree.n_items() {
            if let Some(row) = tree.row(i)
                && let Some(p) = row.item().and_downcast::<gio::FileInfo>().and_then(|f| info_path(&f))
            {
                if p == target {
                    self.view.scroll_to(i, gtk::ListScrollFlags::SELECT, None);
                    *self.pending_reveal.borrow_mut() = None;
                    break;
                }
                if target.starts_with(&p) && !row.is_expanded() && row.is_expandable() {
                    row.set_expanded(true);
                }
            }
            i += 1;
        }
        self.revealing.set(false);
    }

    // ---------------------------------------------------------------- git

    /// Shows the git status of the files (None: not in a repository).
    pub fn set_git(&self, status: Option<Rc<Status>>) {
        *self.git.borrow_mut() = status;
        for r in self.rows.borrow().iter() {
            if let (Some(row), Some(mark), Some((path, dir))) = (r.row.upgrade(), r.mark.upgrade(), &r.item) {
                self.decorate(&row, &mark, path, *dir);
            }
        }
    }

    fn row_bound(&self, row: &gtk::Box, path: PathBuf, dir: bool) {
        let mut rows = self.rows.borrow_mut();
        let Some(r) = rows.iter_mut().find(|r| r.row.upgrade().as_ref() == Some(row)) else { return };
        let Some(mark) = r.mark.upgrade() else { return };
        self.decorate(row, &mark, &path, dir);
        r.item = Some((path, dir));
    }

    fn decorate(&self, row: &gtk::Box, mark: &gtk::Label, path: &Path, dir: bool) {
        for c in git::MARK_CLASSES {
            row.remove_css_class(c);
        }
        let deco = self.git.borrow().as_ref().and_then(|g| g.decoration(path, dir));
        match deco {
            Some((m, letter)) => {
                row.add_css_class(m.css_class());
                mark.set_text(&letter.to_string());
                row.set_tooltip_text(Some(m.describe()));
            }
            None => {
                mark.set_text("");
                row.set_tooltip_text(None);
            }
        }
    }

    // ------------------------------------------------------- file actions

    /// Asks for a file name; `select` is the part of `initial` to select.
    fn ask_name(&self, title: &str, initial: &str, select: usize, done: impl Fn(String) -> bool + 'static) {
        let dlg = gtk::Window::builder().title(title).modal(true).resizable(false).default_width(360).build();
        if let Some(w) = self.widget.root().and_downcast::<gtk::Window>() {
            dlg.set_transient_for(Some(&w));
        }
        let entry = gtk::Entry::builder().text(initial).activates_default(true).hexpand(true).build();
        let cancel = gtk::Button::with_label("Annulla");
        let ok = gtk::Button::with_label("OK");
        ok.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&ok);
        let b = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .build();
        b.append(&entry);
        b.append(&buttons);
        dlg.set_child(Some(&b));
        dlg.set_default_widget(Some(&ok));

        let done = Rc::new(done);
        let (d, e) = (dlg.downgrade(), entry.downgrade());
        ok.connect_clicked(move |_| {
            let (Some(d), Some(e)) = (d.upgrade(), e.upgrade()) else { return };
            let name = e.text().trim().to_string();
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                e.error_bell();
                return;
            }
            if done(name) {
                d.close();
            }
        });
        let d = dlg.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(d) = d.upgrade() {
                d.close();
            }
        });
        let esc = gtk::EventControllerKey::new();
        let d = dlg.downgrade();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(d) = d.upgrade() {
                    d.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dlg.add_controller(esc);
        dlg.present();
        entry.grab_focus();
        entry.select_region(0, select as i32);
    }

    fn new_file(self: &Rc<Self>) {
        let dir = self.context_dir();
        let w = Rc::downgrade(self);
        self.ask_name("Nuovo file", "", 0, move |name| {
            let Some(s) = w.upgrade() else { return true };
            let path = dir.join(&name);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => {
                    s.reveal(&path);
                    (s.handlers.open)(&path);
                    true
                }
                Err(e) => {
                    (s.handlers.error)("Impossibile creare il file", &format!("{}: {e}", path.display()));
                    false
                }
            }
        });
    }

    fn new_folder(self: &Rc<Self>) {
        let dir = self.context_dir();
        let w = Rc::downgrade(self);
        self.ask_name("Nuova cartella", "", 0, move |name| {
            let Some(s) = w.upgrade() else { return true };
            let path = dir.join(&name);
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    s.reveal(&path);
                    true
                }
                Err(e) => {
                    (s.handlers.error)("Impossibile creare la cartella", &format!("{}: {e}", path.display()));
                    false
                }
            }
        });
    }

    fn rename(self: &Rc<Self>) {
        let Some(old) = self.context.borrow().clone() else { return };
        let Some(name) = old.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
        // Select the name without the extension.
        let stem = match name.rfind('.') {
            Some(i) if i > 0 && !old.is_dir() => name[..i].chars().count(),
            _ => name.chars().count(),
        };
        let w = Rc::downgrade(self);
        let initial = name.clone();
        self.ask_name("Rinomina", &initial, stem, move |new_name| {
            let Some(s) = w.upgrade() else { return true };
            if new_name == name {
                return true;
            }
            let new = old.with_file_name(&new_name);
            if new.symlink_metadata().is_ok() {
                (s.handlers.error)("Impossibile rinominare", &format!("{} esiste già", new.display()));
                return false;
            }
            match std::fs::rename(&old, &new) {
                Ok(()) => {
                    (s.handlers.renamed)(&old, &new);
                    s.reveal(&new);
                    true
                }
                Err(e) => {
                    (s.handlers.error)("Impossibile rinominare", &format!("{}: {e}", old.display()));
                    false
                }
            }
        });
    }

    fn trash(self: &Rc<Self>) {
        let Some(path) = self.context.borrow().clone() else { return };
        let name = path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Spostare «{name}» nel cestino?"))
            .buttons(["Annulla", "Sposta nel cestino"])
            .cancel_button(0)
            .default_button(1)
            .build();
        let w = Rc::downgrade(self);
        let parent = self.widget.root().and_downcast::<gtk::Window>();
        dialog.choose(parent.as_ref(), None::<&gio::Cancellable>, move |res| {
            let (Ok(1), Some(s)) = (res, w.upgrade()) else { return };
            if let Err(e) = gio::File::for_path(&path).trash(None::<&gio::Cancellable>) {
                (s.handlers.error)("Impossibile spostare nel cestino", &format!("{}: {e}", path.display()));
            }
        });
    }

    /// Runs a panel action on `path` (the root folder if None).
    #[cfg(debug_assertions)]
    pub fn debug_action(&self, action: &str, path: Option<PathBuf>) {
        if action == "menu" {
            self.popup(path, self.view.upcast_ref(), 60.0, 100.0);
            // Labels of the enabled entries.
            let mut labels = Vec::new();
            if let Some(model) = self.menu.menu_model() {
                for i in 0..model.n_items() {
                    let Some(sec) = model.item_link(i, "section") else { continue };
                    for j in 0..sec.n_items() {
                        let label = sec.item_attribute_value(j, "label", None).and_then(|v| v.get::<String>()).unwrap_or_default();
                        let action = sec.item_attribute_value(j, "action", None).and_then(|v| v.get::<String>()).unwrap_or_default();
                        let name = action.trim_start_matches("side.");
                        if self.actions.is_action_enabled(name) {
                            labels.push(label);
                        }
                    }
                }
            }
            eprintln!("MENU {labels:?}");
            return;
        }
        self.set_context(path);
        gtk::prelude::ActionGroupExt::activate_action(&self.actions, action, None);
    }
}
