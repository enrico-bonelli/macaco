//! External commands run without blocking the interface: the process is
//! read asynchronously on the main loop and `done` is called there.

use gtk::{gio, glib};
use std::cell::Cell;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Output of a finished command.
pub struct Output {
    pub ok: bool,
    /// Kept as the bytes read from the pipe (no copy).
    pub stdout: glib::Bytes,
    pub stderr: String,
}

impl Output {
    pub fn failed(why: String) -> Output {
        Output { ok: false, stdout: glib::Bytes::from_static(b""), stderr: why }
    }

    /// Text to show to the user (errors first).
    pub fn text(&self) -> String {
        let out = String::from_utf8_lossy(&self.stdout);
        let all = format!("{}\n{}", self.stderr.trim(), out.trim());
        all.trim().to_string()
    }

    /// Last meaningful line, for the status bar.
    pub fn summary(&self) -> String {
        self.text().lines().rev().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("hint:")).unwrap_or("").to_string()
    }
}

/// How to run a command.
#[derive(Default)]
pub struct Options<'a> {
    pub cwd: Option<&'a Path>,
    pub env: &'a [(&'a str, &'a str)],
    pub input: Option<Vec<u8>>,
    /// The process is killed after this long.
    pub timeout: Option<Duration>,
    /// Without a controlling terminal (nothing can ask for a password on
    /// the terminal the editor was started from).
    pub detach: bool,
}

/// Runs `argv` and calls `done` with its output on the main loop.
pub fn run(argv: &[&OsStr], opts: Options, done: impl FnOnce(Output) + 'static) {
    let mut flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE;
    if opts.input.is_some() {
        flags |= gio::SubprocessFlags::STDIN_PIPE;
    }
    let launcher = gio::SubprocessLauncher::new(flags);
    if let Some(dir) = opts.cwd {
        launcher.set_cwd(dir);
    }
    for (k, v) in opts.env {
        launcher.setenv(k, v, true);
    }
    if opts.detach {
        launcher.set_child_setup(|| {
            // SAFETY: setsid is async-signal-safe.
            unsafe { libc::setsid() };
        });
    }
    let proc = match launcher.spawn(argv) {
        Ok(p) => p,
        Err(e) => {
            let name = argv.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
            done(Output::failed(format!("impossibile eseguire {name}: {e}")));
            return;
        }
    };
    let timed_out = Rc::new(Cell::new(false));
    let timer = opts.timeout.map(|t| {
        let (proc, timed_out) = (proc.clone(), timed_out.clone());
        let id = Rc::new(Cell::new(None));
        let id2 = id.clone();
        id.set(Some(glib::timeout_add_local_once(t, move || {
            id2.set(None);
            timed_out.set(true);
            proc.force_exit();
        })));
        id
    });
    glib::MainContext::default().spawn_local(async move {
        let stdin = opts.input.map(glib::Bytes::from_owned);
        let res = proc.communicate_future(stdin.as_ref()).await;
        if let Some(id) = timer.and_then(|t| t.take()) {
            id.remove();
        }
        let out = match res {
            _ if timed_out.get() => Output::failed("tempo scaduto".to_string()),
            Ok((stdout, stderr)) => Output {
                ok: proc.is_successful(),
                stdout: stdout.unwrap_or_else(|| glib::Bytes::from_static(b"")),
                stderr: stderr.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            },
            Err(e) => Output::failed(e.to_string()),
        };
        done(out);
    });
}

/// `cmd` in the folders of `PATH`.
pub fn find_in_path(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(cmd)).find(|p| p.is_file()))
}

/// Resident memory (kB) of process `pid` (`"self"` for this one).
pub fn rss_kb(pid: &str) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0)
}
