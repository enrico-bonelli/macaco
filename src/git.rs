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
    run_with(dir, args, &[], None, done);
}

/// Like `run` with extra environment variables (the askpass helper of
/// network commands) and an optional standard input.
pub fn run_with(dir: &Path, args: &[&OsStr], env: &[(String, String)], input: Option<String>, done: impl FnOnce(Output) + 'static) {
    let mut flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE;
    if input.is_some() {
        flags |= gio::SubprocessFlags::STDIN_PIPE;
    }
    let launcher = gio::SubprocessLauncher::new(flags);
    launcher.set_cwd(dir);
    for (k, v) in env {
        launcher.setenv(k, v, true);
    }
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
        let stdin = input.map(|s| glib::Bytes::from_owned(s.into_bytes()));
        let res = proc.communicate_future(stdin.as_ref()).await;
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

// ------------------------------------------------------------ remotes

/// How a remote is reached, from its URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteKind {
    /// `https://host/…`: user name and password (or token).
    Https { host: String },
    /// `ssh://[user@]host[:port]/…` or `user@host:path`: SSH keys.
    Ssh { user: Option<String>, host: String, port: Option<u16> },
    /// A folder or `file://`: no authentication.
    Local,
    /// `git://` and anything else.
    Other,
}

pub fn remote_kind(url: &str) -> RemoteKind {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) {
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or(authority);
        return RemoteKind::Https { host: host.to_string() };
    }
    let ssh = |authority: &str| {
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, authority),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p.parse().ok()),
            _ => (hostport, None),
        };
        RemoteKind::Ssh { user, host: host.to_string(), port }
    };
    if let Some(rest) = u.strip_prefix("ssh://").or_else(|| u.strip_prefix("git+ssh://")) {
        return ssh(rest.split('/').next().unwrap_or(""));
    }
    if u.starts_with("file://") || u.starts_with('/') || u.starts_with('.') || u.starts_with('~') {
        return RemoteKind::Local;
    }
    if u.contains("://") {
        return RemoteKind::Other;
    }
    // scp-like syntax: [user@]host:path (a colon before any slash).
    match u.split_once(':') {
        Some((authority, _)) if !authority.contains('/') && !authority.is_empty() => ssh(authority),
        _ => RemoteKind::Local,
    }
}

/// `name url` pairs of `git remote -v` (fetch URLs, without duplicates).
pub fn parse_remotes(out: &str) -> Vec<(String, String)> {
    let mut list: Vec<(String, String)> = Vec::new();
    for l in out.lines() {
        let mut f = l.split_whitespace();
        let (Some(name), Some(url)) = (f.next(), f.next()) else { continue };
        if !list.iter().any(|(n, _)| n == name) {
            list.push((name.to_string(), url.to_string()));
        }
    }
    list
}

/// The key passed with `-i` in a `core.sshCommand`.
pub fn ssh_command_key(cmd: &str) -> Option<String> {
    let mut words = shell_words(cmd).into_iter();
    while let Some(w) = words.next() {
        if w == "-i" {
            return words.next();
        }
        if let Some(k) = w.strip_prefix("-i") {
            return Some(k.to_string());
        }
    }
    None
}

/// `core.sshCommand` using only `key` (quoted for the shell git runs it with).
pub fn ssh_command_for(key: &str) -> String {
    format!("ssh -i '{}' -o IdentitiesOnly=yes", key.replace('\'', "'\\''"))
}

/// Split a command line on blanks, honouring single and double quotes.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (_, c) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------- history

/// Arguments of `git log` for the history of one file (renames followed),
/// in the format read by `parse_log`.
pub fn log_args(rel: &str, max: usize) -> Vec<String> {
    vec![
        "log".into(),
        "--follow".into(),
        format!("-n{max}"),
        "--date=format:%d/%m/%Y %H:%M".into(),
        "--format=%x1e%H%x1f%h%x1f%an%x1f%ad%x1f%s".into(),
        "--name-only".into(),
        "--".into(),
        rel.into(),
    ]
}

/// A commit in the history of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    /// Path of the file (relative to the repository) in that commit: it
    /// changes across renames.
    pub path: String,
}

/// Parse the output of `git log` run with `log_args`. `rel` is the current
/// path, used for commits that list no file (merges).
pub fn parse_log(out: &str, rel: &str) -> Vec<LogEntry> {
    let mut list: Vec<LogEntry> = Vec::new();
    for rec in out.split('\x1e').filter(|r| !r.trim().is_empty()) {
        let mut lines = rec.lines();
        let head = lines.next().unwrap_or("");
        let f: Vec<&str> = head.split('\x1f').collect();
        if f.len() < 5 {
            continue;
        }
        let path = lines
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| list.last().map_or_else(|| rel.to_string(), |e| e.path.clone()));
        list.push(LogEntry {
            hash: f[0].into(),
            short: f[1].into(),
            author: f[2].into(),
            date: f[3].into(),
            subject: f[4..].join("\x1f"),
            path,
        });
    }
    list
}

/// Lines (from 0) of the new file added or changed by a unified diff.
pub fn added_lines(patch: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut line = None;
    for l in patch.lines() {
        if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            line = h
                .split_whitespace()
                .find_map(|p| p.strip_prefix('+'))
                .and_then(|p| p.split(',').next()?.parse::<usize>().ok())
                .map(|n| n.saturating_sub(1));
            continue;
        }
        let Some(n) = line.as_mut() else { continue };
        match l.as_bytes().first() {
            Some(b'+') => {
                out.push(*n);
                *n += 1;
            }
            Some(b'-') | Some(b'\\') => {}
            _ => *n += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes() {
        assert_eq!(remote_kind("https://github.com/enrico/macaco.git"), RemoteKind::Https { host: "github.com".into() });
        assert_eq!(remote_kind("https://tok@git.example.it/x"), RemoteKind::Https { host: "git.example.it".into() });
        assert_eq!(
            remote_kind("git@github.com:enrico/macaco.git"),
            RemoteKind::Ssh { user: Some("git".into()), host: "github.com".into(), port: None }
        );
        assert_eq!(
            remote_kind("ssh://gitea@192.168.1.10:2222/team/app.git"),
            RemoteKind::Ssh { user: Some("gitea".into()), host: "192.168.1.10".into(), port: Some(2222) }
        );
        assert_eq!(remote_kind("server:/srv/git/app.git"), RemoteKind::Ssh { user: None, host: "server".into(), port: None });
        assert_eq!(remote_kind("/srv/git/app.git"), RemoteKind::Local);
        assert_eq!(remote_kind("../altro"), RemoteKind::Local);
        assert_eq!(remote_kind("git://example.org/x"), RemoteKind::Other);
        let r = parse_remotes("origin\tgit@github.com:a/b.git (fetch)\norigin\tgit@github.com:a/b.git (push)\nlan\t/srv/x (fetch)\n");
        assert_eq!(r, vec![("origin".into(), "git@github.com:a/b.git".into()), ("lan".into(), "/srv/x".into())]);
        assert_eq!(ssh_command_key(&ssh_command_for("/home/e/my keys/id_ed25519")).as_deref(), Some("/home/e/my keys/id_ed25519"));
        assert_eq!(ssh_command_key("ssh -o X=y -i~/.ssh/k").as_deref(), Some("~/.ssh/k"));
        assert_eq!(ssh_command_key("ssh -v"), None);
    }

    #[test]
    fn history() {
        let out = "\x1eaaa111\x1faaa\x1fAnna\x1f28/10/2026 10:55\x1fCorregge il menu\n\nsrc/new.rs\n\
                   \x1ebbb222\x1fbbb\x1fLuca\x1f20/10/2026 09:00\x1fMerge branch 'x'\n\
                   \x1eccc333\x1fccc\x1fLuca\x1f01/10/2026 08:00\x1fPrima versione\n\nsrc/old.rs\n";
        let l = parse_log(out, "src/new.rs");
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].short.as_str(), l[0].author.as_str(), l[0].path.as_str()), ("aaa", "Anna", "src/new.rs"));
        assert_eq!(l[1].path, "src/new.rs");
        assert_eq!((l[2].subject.as_str(), l[2].path.as_str()), ("Prima versione", "src/old.rs"));
        let patch = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,4 @@\n uno\n-due\n+DUE\n+tre\n quattro\n@@ -10,2 +11,2 @@\n a\n+b\n";
        assert_eq!(added_lines(patch), vec![1, 2, 11]);
    }

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
