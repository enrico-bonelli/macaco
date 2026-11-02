//! Small pieces of interface shared by the windows and dialogs.

use gtk::prelude::*;
use gtk::{gdk, glib};

/// Escape closes `win`.
pub fn close_on_escape(win: &impl IsA<gtk::Window>) {
    let esc = gtk::EventControllerKey::new();
    let w = win.as_ref().downgrade();
    esc.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = w.upgrade() {
                w.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    win.as_ref().add_controller(esc);
}

/// A modal dialog over `parent`, closed by Escape.
pub fn dialog_window(parent: &impl IsA<gtk::Window>, title: &str, width: i32) -> gtk::Window {
    let w = gtk::Window::builder().title(title).transient_for(parent).modal(true).default_width(width).build();
    close_on_escape(&w);
    w
}

/// The content box of a dialog: vertical, with the usual margins.
pub fn dialog_box(spacing: i32) -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(spacing)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .build()
}

/// A window to pick an item: a search entry, `list` in a frame and a
/// dim line of text below. Returns the window, the entry and the line.
pub fn picker_window(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    size: (i32, i32),
    entry: gtk::SearchEntry,
    list: &impl IsA<gtk::Widget>,
) -> (gtk::Window, gtk::SearchEntry, gtk::Label) {
    let win = gtk::Window::builder().title(title).transient_for(parent).modal(true).default_width(size.0).default_height(size.1).build();
    let scroll = gtk::ScrolledWindow::builder().child(list).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
    scroll.add_css_class("frame");
    let line = gtk::Label::new(None);
    line.add_css_class("dim-label");
    line.set_xalign(0.0);
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build();
    b.append(&entry);
    b.append(&scroll);
    b.append(&line);
    win.set_child(Some(&b));
    (win, entry, line)
}

/// A menu section with one entry per file ("name  —  ~/folder") running
/// `action` with the path, or `empty` (disabled) when there are none.
pub fn path_menu(paths: &[std::path::PathBuf], action: &str, empty: &str) -> gtk::gio::Menu {
    let files = gtk::gio::Menu::new();
    for p in paths {
        let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        let dir = p.parent().map(crate::config::tilde).unwrap_or_default();
        // '_' would be taken as a mnemonic marker.
        let label = format!("{name}  —  {dir}").replace('_', "__");
        let item = gtk::gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some(action), Some(&p.to_string_lossy().to_variant()));
        files.append_item(&item);
    }
    if paths.is_empty() {
        // `win.none` does not exist: the entry is shown disabled.
        files.append(Some(empty), Some("win.none"));
    }
    files
}

/// The window of the application that has the focus, if any.
pub fn active_window() -> Option<gtk::Window> {
    gtk::Window::list_toplevels().into_iter().filter_map(|w| w.downcast::<gtk::Window>().ok()).find(|w| w.is_active())
}

/// Asks to confirm an action ("Annulla" or `ok`, the default one when
/// `ok_default`); `then` gets whether it was confirmed.
pub fn confirm(parent: Option<&gtk::Window>, message: &str, detail: &str, ok: &str, ok_default: bool, then: impl FnOnce(bool) + 'static) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .buttons(["Annulla", ok])
        .cancel_button(0)
        .default_button(i32::from(ok_default))
        .build();
    if !detail.is_empty() {
        dialog.set_detail(detail);
    }
    dialog.choose(parent, None::<&gtk::gio::Cancellable>, move |res| then(res == Ok(1)));
}
