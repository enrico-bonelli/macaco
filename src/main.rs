//! macaco — a lightweight GTK 4 text editor.

mod config;
mod diff;
mod doc;
mod encoding;
mod finder;
mod format;
mod git;
mod lang;
mod lexer;
mod search;
mod sidebar;
mod terminal;
mod window;

use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

fn main() -> glib::ExitCode {
    // One malloc arena: memory used by background work (file scans, text
    // search) goes back to the system when freed instead of staying in the
    // arena of a worker thread.
    #[cfg(target_env = "gnu")]
    // SAFETY: called before any other thread exists.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
    // A text editor does not need GPU rendering: the cairo renderer avoids
    // loading GL/Vulkan drivers and saves a lot of memory.
    if std::env::var_os("GSK_RENDERER").is_none() {
        let r = config::Settings::load().renderer;
        if r != "auto" {
            // SAFETY: no other thread exists yet.
            unsafe { std::env::set_var("GSK_RENDERER", r) };
        }
    }
    #[allow(unused_mut)]
    let mut flags = gio::ApplicationFlags::HANDLES_OPEN;
    // Scripted test runs must not be forwarded to a running editor.
    #[cfg(debug_assertions)]
    if std::env::var_os("MACACO_DEBUG").is_some() {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = gtk::Application::builder()
        .application_id(window::APP_ID)
        .flags(flags)
        .build();
    let editor: Rc<RefCell<Option<Rc<window::Editor>>>> = Rc::default();

    let e = editor.clone();
    app.connect_startup(move |app| {
        // Running from the source tree: make the bundled icon available.
        #[cfg(debug_assertions)]
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display).add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data"));
        }
        gtk::Window::set_default_icon_name(window::APP_ID);
        let ed = window::Editor::new(app);
        #[cfg(debug_assertions)]
        ed.debug_script();
        *e.borrow_mut() = Some(ed);
    });
    let e = editor.clone();
    app.connect_activate(move |_| {
        if let Some(ed) = e.borrow().as_ref() {
            ed.present();
        }
    });
    let e = editor.clone();
    app.connect_open(move |_, files, _| {
        if let Some(ed) = e.borrow().as_ref() {
            for f in files {
                if let Some(p) = f.path() {
                    ed.open_path(&p);
                }
            }
            ed.present();
        }
    });
    app.run()
}
