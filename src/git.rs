//! Git support: runs the `git` command asynchronously and parses
//! `git status`.

use gtk::{gio, glib};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// How a file differs from the last commit, as shown in the file panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Untracked,
    /// Changes added to the index only.
    Staged,
    /// Changes not added to the index.
    Modified,
    Conflict,
}

impl Mark {
    pub fn css_class(self) -> &'static str {
        match self {
            Mark::Untracked => "git-untracked",
            Mark::Staged => "git-staged",
            Mark::Modified => "git-modified",
            Mark::Conflict => "git-conflict",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Mark::Untracked => "non tracciato",
            Mark::Staged => "nell'area di stage",
            Mark::Modified => "modificato",
            Mark::Conflict => "in conflitto",
        }
    }
}

pub const MARK_CLASSES: [&str; 4] = ["git-untracked", "git-staged", "git-modified", "git-conflict"];

/// Status of a file: the two `XY` letters of `git status --porcelain`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub x: u8,
    pub y: u8,
}

impl Entry {
    pub fn mark(self) -> Mark {
        match (self.x, self.y) {
            (b'?', b'?') => Mark::Untracked,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => Mark::Conflict,
            (_, b' ') => Mark::Staged,
            _ => Mark::Modified,
        }
    }

    /// One letter shown next to the file name.
    pub fn letter(self) -> char {
        match self.mark() {
            Mark::Untracked => 'U',
            Mark::Conflict => '!',
            Mark::Staged => self.x as char,
            Mark::Modified => self.y as char,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Top folder of the working tree.
    pub root: PathBuf,
    pub git_dir: PathBuf,
    /// Current branch (None when detached).
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: HashMap<PathBuf, Entry>,
    /// Untracked folders (all their content is untracked).
    pub untracked_dirs: HashSet<PathBuf>,
    /// Most relevant mark of the files inside each folder.
    pub dirs: HashMap<PathBuf, Mark>,
}

impl Status {
    /// Parses `git status --porcelain=v1 -z --branch`.
    pub fn parse(root: &Path, git_dir: &Path, out: &[u8]) -> Status {
        let mut st = Status { root: root.to_path_buf(), git_dir: git_dir.to_path_buf(), ..Default::default() };
        let mut recs = out.split(|&c| c == 0).filter(|r| !r.is_empty());
        while let Some(rec) = recs.next() {
            if let Some(head) = rec.strip_prefix(b"## ") {
                st.parse_branch(&String::from_utf8_lossy(head));
                continue;
            }
            if rec.len() < 4 {
                continue;
            }
            let e = Entry { x: rec[0], y: rec[1] };
            let rel = &rec[3..];
            // Renames and copies are followed by the original path.
            if matches!(e.x, b'R' | b'C') || matches!(e.y, b'R' | b'C') {
                recs.next();
            }
            if e.mark() == Mark::Untracked && rel.ends_with(b"/") {
                let p = root.join(OsStr::from_bytes(&rel[..rel.len() - 1]));
                st.add_to_dirs(&p, Mark::Untracked);
                st.untracked_dirs.insert(p);
                continue;
            }
            let p = root.join(OsStr::from_bytes(rel));
            if let Some(dir) = p.parent() {
                let dir = dir.to_path_buf();
                st.add_to_dirs(&dir, e.mark());
            }
            st.files.insert(p, e);
        }
        st
    }

    fn parse_branch(&mut self, head: &str) {
        let head = head.trim();
        if let Some(b) = head.strip_prefix("No commits yet on ").or_else(|| head.strip_prefix("Initial commit on ")) {
            self.branch = Some(b.to_string());
            return;
        }
        if head.starts_with("HEAD (no branch)") {
            return;
        }
        let (refs, counts) = match head.split_once(" [") {
            Some((r, c)) => (r, c.trim_end_matches(']')),
            None => (head, ""),
        };
        match refs.split_once("...") {
            Some((b, u)) => {
                self.branch = Some(b.to_string());
                self.upstream = Some(u.to_string());
            }
            None => self.branch = Some(refs.to_string()),
        }
        for part in counts.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                self.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                self.behind = n.parse().unwrap_or(0);
            }
        }
    }

    /// Records `mark` for `dir` and its parents up to the root.
    fn add_to_dirs(&mut self, dir: &Path, mark: Mark) {
        for d in dir.ancestors() {
            if !d.starts_with(&self.root) || d == self.root {
                break;
            }
            let m = self.dirs.entry(d.to_path_buf()).or_insert(mark);
            if *m < mark {
                *m = mark;
            }
        }
    }

    /// Status of a file (None: unchanged or outside the repository).
    pub fn entry(&self, path: &Path) -> Option<Entry> {
        if let Some(e) = self.files.get(path) {
            return Some(*e);
        }
        path.ancestors()
            .skip(1)
            .take_while(|d| d.starts_with(&self.root) && *d != self.root)
            .any(|d| self.untracked_dirs.contains(d))
            .then_some(Entry { x: b'?', y: b'?' })
    }

    /// Mark and letter to show for a file or folder.
    pub fn decoration(&self, path: &Path, is_dir: bool) -> Option<(Mark, char)> {
        if is_dir {
            if self.untracked_dirs.contains(path) || self.entry(path).is_some_and(|e| e.mark() == Mark::Untracked) {
                return Some((Mark::Untracked, 'U'));
            }
            return self.dirs.get(path).map(|m| (*m, '•'));
        }
        self.entry(path).map(|e| (e.mark(), e.letter()))
    }

    pub fn count(&self, mark: Mark) -> usize {
        self.files.values().filter(|e| e.mark() == mark).count()
            + if mark == Mark::Untracked { self.untracked_dirs.len() } else { 0 }
    }

    /// Files with changes in the index.
    pub fn staged(&self) -> usize {
        self.files.values().filter(|e| e.x != b' ' && e.x != b'?' && e.mark() != Mark::Conflict).count()
    }

    /// Branch name with ahead/behind counts, for the status bar.
    pub fn branch_label(&self) -> String {
        let mut s = self.branch.clone().unwrap_or_else(|| "(HEAD staccato)".into());
        if self.ahead > 0 {
            s.push_str(&format!(" ↑{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!(" ↓{}", self.behind));
        }
        s
    }
}

/// Files and folders the user does not want to see in the list of changes.
/// Rules are paths relative to the repository; folders end with `/`.
#[derive(Clone, Debug, Default)]
pub struct Excluded {
    pub rules: Vec<String>,
    /// Rules for single files, for a quick lookup.
    files: HashSet<String>,
}

impl Excluded {
    /// The rule hiding `rel` (a path relative to the repository), if any.
    pub fn rule_for(&self, rel: &str) -> Option<&str> {
        if let Some(r) = self.files.get(rel) {
            return Some(r);
        }
        self.rules.iter().find(|r| r.ends_with('/') && rel.starts_with(r.as_str())).map(String::as_str)
    }

    pub fn add(&mut self, rule: String) {
        if let Err(i) = self.rules.binary_search(&rule) {
            if !rule.ends_with('/') {
                self.files.insert(rule.clone());
            }
            self.rules.insert(i, rule);
        }
    }

    /// Removes every rule that hides `rel`.
    pub fn restore(&mut self, rel: &str) {
        while let Some(r) = self.rule_for(rel).map(str::to_string) {
            self.rules.retain(|x| *x != r);
            self.files.remove(&r);
        }
    }
}

/// Whether `rel` matches the filter typed by the user: words separated by
/// spaces are alternatives; a word with `*` or `?` is a pattern for the
/// file name (or the whole path if it contains `/`), otherwise any part of
/// the path matches. Case is ignored.
pub fn filter_matches(rel: &str, filter: &str) -> bool {
    let rel = rel.to_lowercase();
    let name = rel.rsplit('/').next().unwrap_or(&rel);
    let mut words = filter.split_whitespace().peekable();
    if words.peek().is_none() {
        return true;
    }
    words.any(|w| {
        let w = w.to_lowercase();
        if w.contains(['*', '?']) {
            glob(w.as_bytes(), if w.contains('/') { rel.as_bytes() } else { name.as_bytes() })
        } else {
            rel.contains(&w)
        }
    })
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob(&p[1..], s) || (!s.is_empty() && glob(p, &s[1..])),
        (Some(b'?'), Some(_)) => glob(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob(&p[1..], &s[1..]),
        _ => false,
    }
}

/// Output of a finished command.
pub struct Output {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
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

/// Runs `git args` in `dir` and calls `done` on the main loop.
///
/// Git must never wait for input: password prompts are disabled and the
/// command runs without a controlling terminal (SSH then uses the agent
/// or `SSH_ASKPASS`).
pub fn run(dir: &Path, args: &[&OsStr], done: impl FnOnce(Output) + 'static) {
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE);
    launcher.set_cwd(dir);
    launcher.setenv("GIT_TERMINAL_PROMPT", "0", true);
    // Merges (also by `git pull`) never open an editor for the message.
    launcher.setenv("GIT_MERGE_AUTOEDIT", "no", true);
    // `git status` must not rewrite the index: that would wake the
    // .git folder monitor again.
    launcher.setenv("GIT_OPTIONAL_LOCKS", "0", true);
    launcher.set_child_setup(|| {
        // SAFETY: setsid is async-signal-safe.
        unsafe { libc::setsid() };
    });
    let mut argv: Vec<&OsStr> = vec![OsStr::new("git")];
    argv.extend_from_slice(args);
    let proc = match launcher.spawn(&argv) {
        Ok(p) => p,
        Err(e) => {
            done(Output { ok: false, stdout: Vec::new(), stderr: format!("impossibile eseguire git: {e}") });
            return;
        }
    };
    glib::MainContext::default().spawn_local(async move {
        let res = proc.communicate_future(None).await;
        let out = match res {
            Ok((stdout, stderr)) => Output {
                ok: proc.is_successful(),
                stdout: stdout.map(|b| b.to_vec()).unwrap_or_default(),
                stderr: stderr.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            },
            Err(e) => Output { ok: false, stdout: Vec::new(), stderr: e.to_string() },
        };
        done(out);
    });
}

/// Like `run` with string arguments.
pub fn run_str(dir: &Path, args: &[&str], done: impl FnOnce(Output) + 'static) {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    run(dir, &args, done);
}

/// Finds the repository containing `dir` and reads its status.
pub fn status(dir: &Path, done: impl FnOnce(Option<Status>) + 'static) {
    status_of(dir, None, done);
}

/// Like `status`, limited to the files under `only` and listing every
/// untracked file (instead of their folders).
pub fn status_of(dir: &Path, only: Option<&Path>, done: impl FnOnce(Option<Status>) + 'static) {
    let only = only.map(Path::to_path_buf);
    run_str(dir, &["rev-parse", "--show-toplevel", "--absolute-git-dir"], move |out| {
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut lines = text.lines();
        let (Some(root), Some(git_dir), true) = (lines.next(), lines.next(), out.ok) else {
            done(None);
            return;
        };
        let (root, git_dir) = (PathBuf::from(root), PathBuf::from(git_dir));
        let mut args: Vec<&OsStr> = ["status", "--porcelain=v1", "-z", "--branch"].map(OsStr::new).to_vec();
        if let Some(p) = &only {
            args.extend([OsStr::new("--untracked-files=all"), OsStr::new("--"), p.as_os_str()]);
        }
        run(&root.clone(), &args, move |out| {
            done(out.ok.then(|| Status::parse(&root, &git_dir, &out.stdout)));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status() {
        let out = b"## main...origin/main [ahead 2, behind 1]\0M  src/a.rs\0 M src/b/c.rs\0?? new/\0?? x.txt\0R  d.rs\0old.rs\0UU e.rs\0";
        let st = Status::parse(Path::new("/r"), Path::new("/r/.git"), out);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.decoration(Path::new("/r/src/a.rs"), false), Some((Mark::Staged, 'M')));
        assert_eq!(st.decoration(Path::new("/r/src/b/c.rs"), false), Some((Mark::Modified, 'M')));
        assert_eq!(st.decoration(Path::new("/r/src"), true), Some((Mark::Modified, '•')));
        assert_eq!(st.decoration(Path::new("/r/new/deep/f"), false), Some((Mark::Untracked, 'U')));
        assert_eq!(st.decoration(Path::new("/r/new"), true), Some((Mark::Untracked, 'U')));
        assert_eq!(st.decoration(Path::new("/r/d.rs"), false), Some((Mark::Staged, 'R')));
        assert_eq!(st.decoration(Path::new("/r/old.rs"), false), None);
        assert_eq!(st.decoration(Path::new("/r/e.rs"), false), Some((Mark::Conflict, '!')));
        assert_eq!(st.staged(), 2);
        let mut ex = Excluded::default();
        ex.add("build/".into());
        ex.add("notes.txt".into());
        assert_eq!(ex.rule_for("build/x/y.o"), Some("build/"));
        assert_eq!(ex.rule_for("notes.txt"), Some("notes.txt"));
        assert_eq!(ex.rule_for("notes.txt.bak"), None);
        assert_eq!(ex.rule_for("buildx"), None);
        ex.restore("build/a");
        assert_eq!(ex.rules, vec!["notes.txt".to_string()]);
        assert!(filter_matches("doc/Guida.PDF", ".pdf"));
        assert!(filter_matches("doc/guida.pdf", "*.pdf"));
        assert!(!filter_matches("doc/guida.pdf.txt", "*.pdf"));
        assert!(filter_matches("a/x.png", ".pdf .png"));
        assert!(filter_matches("src/a.rs", "src/"));
        assert!(filter_matches("src/a.rs", "src/*.rs"));
        assert!(!filter_matches("lib/a.rs", "src/*.rs"));
        assert!(filter_matches("x", "  "));
        let st = Status::parse(Path::new("/r"), Path::new("/r/.git"), b"## No commits yet on dev\0");
        assert_eq!(st.branch.as_deref(), Some("dev"));
        assert!(st.upstream.is_none());
    }
}
