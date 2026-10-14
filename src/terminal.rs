//! Optional terminal panel docked at the bottom of the window.
//!
//! libvte (GTK 4 build) is loaded with `dlopen` the first time the panel is
//! opened: when the terminal is never used it costs nothing, and macaco does
//! not need VTE to build or to run.

use gtk::glib::translate::{FromGlibPtrNone, IntoGlib, ToGlibPtr};
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

const LIB: &str = "libvte-2.91-gtk4.so.0";
const SCROLLBACK: c_long = 10_000;
/// G_SPAWN_SEARCH_PATH
const SPAWN_SEARCH_PATH: c_int = 1 << 2;
/// VTE_FORMAT_TEXT
const FORMAT_TEXT: c_int = 1;

type Ptr = *mut c_void;
type SpawnCallback = unsafe extern "C" fn(Ptr, c_int, *mut glib::ffi::GError, Ptr);

/// The few libvte entry points macaco uses.
struct Vte {
    new: unsafe extern "C" fn() -> *mut gtk::ffi::GtkWidget,
    #[allow(clippy::type_complexity)]
    spawn_async: unsafe extern "C" fn(
        Ptr,                       // terminal
        c_int,                     // VtePtyFlags
        *const c_char,             // working directory
        *const *const c_char,      // argv
        *const *const c_char,      // envv (NULL: inherit)
        c_int,                     // GSpawnFlags
        Ptr,                       // child setup
        Ptr,                       // child setup data
        Ptr,                       // child setup data destroy
        c_int,                     // timeout
        Ptr,                       // GCancellable
        Option<SpawnCallback>,     // callback
        Ptr,                       // user data
    ),
    set_font: unsafe extern "C" fn(Ptr, *const pango::ffi::PangoFontDescription),
    set_colors: unsafe extern "C" fn(Ptr, *const gdk::ffi::GdkRGBA, *const gdk::ffi::GdkRGBA, *const gdk::ffi::GdkRGBA, usize),
    set_scrollback_lines: unsafe extern "C" fn(Ptr, c_long),
    set_audible_bell: unsafe extern "C" fn(Ptr, glib::ffi::gboolean),
    feed_child: unsafe extern "C" fn(Ptr, *const c_char, isize),
    copy_clipboard_format: unsafe extern "C" fn(Ptr, c_int),
    paste_clipboard: unsafe extern "C" fn(Ptr),
    get_pty: unsafe extern "C" fn(Ptr) -> Ptr,
    pty_get_fd: unsafe extern "C" fn(Ptr) -> c_int,
}

fn dl_error() -> String {
    // SAFETY: dlerror returns NULL or a valid C string.
    unsafe {
        let e = libc::dlerror();
        if e.is_null() { String::new() } else { CStr::from_ptr(e).to_string_lossy().into_owned() }
    }
}

#[allow(clippy::missing_transmute_annotations)]
fn load() -> Result<Vte, String> {
    let name = CString::new(LIB).unwrap();
    // The library is never closed: it registers GTypes.
    // SAFETY: plain dlopen/dlsym; every symbol is cast to its C prototype.
    unsafe {
        let h = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return Err(dl_error());
        }
        macro_rules! sym {
            ($n:literal) => {{
                let p = libc::dlsym(h, concat!($n, "\0").as_ptr().cast());
                if p.is_null() {
                    return Err(format!("simbolo {} mancante", $n));
                }
                std::mem::transmute::<*mut c_void, _>(p)
            }};
        }
        Ok(Vte {
            new: sym!("vte_terminal_new"),
            spawn_async: sym!("vte_terminal_spawn_async"),
            set_font: sym!("vte_terminal_set_font"),
            set_colors: sym!("vte_terminal_set_colors"),
            set_scrollback_lines: sym!("vte_terminal_set_scrollback_lines"),
            set_audible_bell: sym!("vte_terminal_set_audible_bell"),
            feed_child: sym!("vte_terminal_feed_child"),
            copy_clipboard_format: sym!("vte_terminal_copy_clipboard_format"),
            paste_clipboard: sym!("vte_terminal_paste_clipboard"),
            get_pty: sym!("vte_terminal_get_pty"),
            pty_get_fd: sym!("vte_pty_get_fd"),
        })
    }
}

fn vte() -> Result<&'static Vte, String> {
    static VTE: OnceLock<Result<Vte, String>> = OnceLock::new();
    VTE.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Terminal palettes matching the editor colors: background, foreground and
/// the 16 ANSI colors.
const DARK: (&str, &str, [&str; 16]) = (
    "#1e2127",
    "#abb2bf",
    [
        "#3f4451", "#e06c75", "#98c379", "#d19a66", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
        "#5c6370", "#ef7f88", "#a9d18e", "#e5c07b", "#74bdf8", "#d68ce8", "#67cbd6", "#e6e6e6",
    ],
);
const LIGHT: (&str, &str, [&str; 16]) = (
    "#ffffff",
    "#24292e",
    [
        "#24292e", "#d73a49", "#22863a", "#b08800", "#0366d6", "#6f42c1", "#1b7c83", "#6a737d",
        "#586069", "#cb2431", "#28a745", "#dbab09", "#2188ff", "#8a63d2", "#3192aa", "#d1d5da",
    ],
);

fn rgba(s: &str) -> gdk::ffi::GdkRGBA {
    let c = gdk::RGBA::parse(s).unwrap_or(gdk::RGBA::BLACK);
    // SAFETY: the pointer refers to `c`, alive for the copy.
    unsafe { *ToGlibPtr::<*const gdk::ffi::GdkRGBA>::to_glib_none(&c).0 }
}

/// Quotes a path for POSIX shells (and fish).
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

pub struct Terminal {
    /// Terminal and its scrollbar: the widget to put in the window.
    pub widget: gtk::Box,
    term: gtk::Widget,
    vte: &'static Vte,
    /// Shell process id, 0 until spawned.
    pid: Rc<Cell<i32>>,
}

impl Terminal {
    /// Creates the terminal and starts the user's shell in `dir`.
    /// `on_exit` is called when the shell terminates; `on_error` if it
    /// could not be started.
    pub fn new(dir: &Path, on_exit: impl Fn() + 'static, on_error: impl FnOnce(String) + 'static) -> Result<Terminal, String> {
        let vte = vte()?;
        // SAFETY: vte_terminal_new returns a floating GtkWidget reference,
        // which from_glib_none sinks.
        let term: gtk::Widget = unsafe { gtk::Widget::from_glib_none((vte.new)()) };
        term.set_hexpand(true);
        term.set_vexpand(true);
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        widget.add_css_class("terminal-panel");
        widget.append(&term);
        let vadj = term.property::<gtk::Adjustment>("vadjustment");
        widget.append(&gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&vadj)));

        let t = Terminal { widget, term, vte, pid: Rc::new(Cell::new(0)) };
        // SAFETY: valid terminal pointer.
        unsafe { (vte.set_scrollback_lines)(t.ptr(), SCROLLBACK) };

        // Ctrl+Shift+C / Ctrl+Shift+V: copy and paste, as in other terminals.
        let keys = gtk::EventControllerKey::new();
        let w = t.term.downgrade();
        keys.connect_key_pressed(move |_, key, _, state| {
            let mods = state & (gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::ALT_MASK);
            if mods != gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK {
                return glib::Propagation::Proceed;
            }
            let Some(term) = w.upgrade() else { return glib::Propagation::Proceed };
            let p = term.as_ptr() as Ptr;
            // SAFETY: valid terminal pointer.
            match key.to_lower() {
                gdk::Key::c => unsafe { (vte.copy_clipboard_format)(p, FORMAT_TEXT) },
                gdk::Key::v => unsafe { (vte.paste_clipboard)(p) },
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        t.term.add_controller(keys);

        t.term.connect_local("child-exited", false, move |_| {
            on_exit();
            None
        });
        t.spawn(dir, on_error);
        Ok(t)
    }

    fn ptr(&self) -> Ptr {
        self.term.as_ptr() as Ptr
    }

    fn spawn(&self, dir: &Path, on_error: impl FnOnce(String) + 'static) {
        let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        let shell = CString::new(shell.as_bytes()).unwrap_or_else(|_| c"/bin/sh".to_owned());
        let argv = [shell.as_ptr(), std::ptr::null()];
        let cwd = CString::new(dir.as_os_str().as_bytes()).ok();

        type Done = Box<dyn FnOnce(Result<i32, String>)>;
        unsafe extern "C" fn spawned(_: Ptr, pid: c_int, err: *mut glib::ffi::GError, data: Ptr) {
            // SAFETY: `data` is the Box leaked below, used exactly once.
            let done = unsafe { Box::from_raw(data as *mut Done) };
            if err.is_null() {
                done(Ok(pid));
            } else {
                // SAFETY: VTE passes a valid GError.
                let msg = unsafe { CStr::from_ptr((*err).message).to_string_lossy().into_owned() };
                done(Err(msg));
            }
        }
        let pid = self.pid.clone();
        let done: Done = Box::new(move |r| match r {
            Ok(p) => pid.set(p),
            Err(e) => on_error(e),
        });
        // SAFETY: argv/cwd are copied by VTE before returning.
        unsafe {
            (self.vte.spawn_async)(
                self.ptr(),
                0,
                cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                argv.as_ptr(),
                std::ptr::null(),
                SPAWN_SEARCH_PATH,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                -1,
                std::ptr::null_mut(),
                Some(spawned),
                Box::into_raw(Box::new(done)) as Ptr,
            );
        }
    }

    pub fn apply_style(&self, font: &pango::FontDescription, dark: bool, bell: bool) {
        let (bg, fg, pal) = if dark { DARK } else { LIGHT };
        let palette: Vec<gdk::ffi::GdkRGBA> = pal.iter().map(|c| rgba(c)).collect();
        let (bg, fg) = (rgba(bg), rgba(fg));
        // SAFETY: valid terminal pointer; VTE copies font and colors.
        unsafe {
            (self.vte.set_font)(self.ptr(), font.to_glib_none().0);
            (self.vte.set_colors)(self.ptr(), &fg, &bg, palette.as_ptr(), palette.len());
            (self.vte.set_audible_bell)(self.ptr(), bell.into_glib());
        }
    }

    pub fn grab_focus(&self) {
        self.term.grab_focus();
    }

    pub fn has_focus(&self) -> bool {
        self.term.has_focus()
    }

    /// Calls `f(true)` when the terminal gains the keyboard focus, `f(false)`
    /// when it loses it.
    pub fn connect_focus(&self, f: impl Fn(bool) + 'static) {
        let c = gtk::EventControllerFocus::new();
        let f = Rc::new(f);
        let g = f.clone();
        c.connect_enter(move |_| f(true));
        c.connect_leave(move |_| g(false));
        self.term.add_controller(c);
    }

    /// Current directory of the shell.
    fn cwd(&self) -> Option<PathBuf> {
        let pid = self.pid.get();
        (pid > 0).then(|| std::fs::read_link(format!("/proc/{pid}/cwd")).ok()).flatten()
    }

    /// Whether the shell is waiting at its prompt (no command running in
    /// the foreground).
    fn shell_idle(&self) -> bool {
        let pid = self.pid.get();
        if pid <= 0 {
            return false;
        }
        // SAFETY: valid terminal pointer; the pty belongs to the terminal.
        unsafe {
            let pty = (self.vte.get_pty)(self.ptr());
            !pty.is_null() && libc::tcgetpgrp((self.vte.pty_get_fd)(pty)) == pid
        }
    }

    /// Moves the shell to `dir`. Fails if a command is running.
    pub fn cd(&self, dir: &Path) -> Result<(), String> {
        if self.cwd().as_deref() == Some(dir) {
            return Ok(());
        }
        if !self.shell_idle() {
            return Err("Il terminale sta eseguendo un comando: cartella non cambiata".into());
        }
        // Ctrl+E Ctrl+U clear what is typed at the prompt; the leading space
        // keeps the command out of the history (HISTCONTROL=ignorespace).
        let cmd = format!("\x05\x15 cd -- {}\r", shell_quote(dir));
        // SAFETY: valid terminal pointer and buffer.
        unsafe { (self.vte.feed_child)(self.ptr(), cmd.as_ptr().cast(), cmd.len() as isize) };
        Ok(())
    }

    #[cfg(debug_assertions)]
    pub fn debug_feed(&self, text: &str) {
        // SAFETY: valid terminal pointer and buffer.
        unsafe { (self.vte.feed_child)(self.ptr(), text.as_ptr().cast(), text.len() as isize) };
    }
}
