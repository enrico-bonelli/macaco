//! Persistent state under `~/.macaco/`:
//!
//! * `config/settings.conf`  user preferences (font, size, ...)
//! * `recent`                recently opened files, most recent first
//! * `session`               tabs open at exit, working folder
//! * `cache/<id>.txt|.meta`  content of unsaved tabs
//! * `git-excluded`          files hidden from the list of git changes
//! * `indexed`               folders indexed for the quick file search
//! * `index/<hash>.paths`    their list of files
//! * `lsp-folders`           folders where language servers are enabled
//! * `bookmarks`             favourite files and folders

use crate::encoding::{Encoding, Eol};
use crate::lang::Lang;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_RECENT: usize = 15;

pub fn base_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".macaco")
}

pub fn config_dir() -> PathBuf {
    base_dir().join("config")
}

pub fn cache_dir() -> PathBuf {
    base_dir().join("cache")
}

/// Write a file atomically (temp file + rename) creating parent dirs.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp~");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

/// Iterate over `key=value` lines.
fn pairs(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub font_family: String,
    pub font_size: u32,
    pub tab_width: u32,
    pub insert_spaces: bool,
    pub wrap: bool,
    pub line_numbers: bool,
    pub highlight_line: bool,
    pub dark: bool,
    /// Audible bell (terminal, invalid key presses).
    pub bell: bool,
    pub cursor_blink: bool,
    /// Completion list while typing (Ctrl+Space always works).
    pub autocomplete: bool,
    /// Git over HTTPS: remember user names and passwords in the system
    /// keyring (when no credential helper of the user is configured).
    pub git_keyring: bool,
    pub sidebar: bool,
    pub sidebar_width: i32,
    pub width: i32,
    pub height: i32,
    /// GSK renderer: "cairo" (lightest, default), "gl", "vulkan" or "auto".
    pub renderer: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font_family: "Monospace".into(),
            font_size: 11,
            tab_width: 4,
            insert_spaces: true,
            wrap: false,
            line_numbers: true,
            highlight_line: true,
            dark: false,
            bell: true,
            cursor_blink: false,
            autocomplete: true,
            git_keyring: true,
            sidebar: false,
            sidebar_width: 240,
            width: 1000,
            height: 700,
            renderer: "cairo".into(),
        }
    }
}

pub const MIN_FONT: u32 = 6;
pub const MAX_FONT: u32 = 72;

impl Settings {
    fn path() -> PathBuf {
        config_dir().join("settings.conf")
    }

    pub fn exists() -> bool {
        Self::path().exists()
    }

    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Ok(text) = fs::read_to_string(Self::path()) else { return s };
        let flag = |v: &str| v == "true";
        for (k, v) in pairs(&text) {
            match k {
                "font_family" if !v.is_empty() => s.font_family = v.to_string(),
                "font_size" => s.font_size = v.parse().unwrap_or(s.font_size).clamp(MIN_FONT, MAX_FONT),
                "tab_width" => s.tab_width = v.parse().unwrap_or(s.tab_width).clamp(1, 16),
                "insert_spaces" => s.insert_spaces = flag(v),
                "wrap" => s.wrap = flag(v),
                "line_numbers" => s.line_numbers = flag(v),
                "highlight_line" => s.highlight_line = flag(v),
                "dark" => s.dark = flag(v),
                "bell" => s.bell = flag(v),
                "cursor_blink" => s.cursor_blink = flag(v),
                "autocomplete" => s.autocomplete = flag(v),
                "git_keyring" => s.git_keyring = flag(v),
                "sidebar" => s.sidebar = flag(v),
                "sidebar_width" => s.sidebar_width = v.parse().unwrap_or(s.sidebar_width).max(120),
                "width" => s.width = v.parse().unwrap_or(s.width).max(300),
                "height" => s.height = v.parse().unwrap_or(s.height).max(200),
                "renderer" if !v.is_empty() => s.renderer = v.to_string(),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let text = format!(
            "# macaco settings\nfont_family={}\nfont_size={}\ntab_width={}\ninsert_spaces={}\nwrap={}\nline_numbers={}\nhighlight_line={}\ndark={}\nbell={}\ncursor_blink={}\nautocomplete={}\ngit_keyring={}\nsidebar={}\nsidebar_width={}\nwidth={}\nheight={}\n# renderer: cairo (lightest) | gl | vulkan | auto\nrenderer={}\n",
            self.font_family,
            self.font_size,
            self.tab_width,
            self.insert_spaces,
            self.wrap,
            self.line_numbers,
            self.highlight_line,
            self.dark,
            self.bell,
            self.cursor_blink,
            self.autocomplete,
            self.git_keyring,
            self.sidebar,
            self.sidebar_width,
            self.width,
            self.height,
            self.renderer
        );
        if let Err(e) = write_atomic(&Self::path(), text.as_bytes()) {
            eprintln!("macaco: impossibile salvare le impostazioni: {e}");
        }
    }
}

// ---------------------------------------------------------------- bookmarks

pub fn load_bookmarks() -> Vec<PathBuf> {
    fs::read_to_string(base_dir().join("bookmarks"))
        .map(|t| t.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

pub fn save_bookmarks(list: &[PathBuf]) {
    let text: String = list.iter().map(|p| format!("{}\n", p.to_string_lossy())).collect();
    let _ = write_atomic(&base_dir().join("bookmarks"), text.as_bytes());
}

// ------------------------------------------------------------- recent files

fn recent_path() -> PathBuf {
    base_dir().join("recent")
}

pub fn load_recent() -> Vec<PathBuf> {
    fs::read_to_string(recent_path())
        .map(|t| t.lines().filter(|l| !l.is_empty()).map(PathBuf::from).take(MAX_RECENT).collect())
        .unwrap_or_default()
}

pub fn save_recent(list: &[PathBuf]) {
    let mut text = String::new();
    for p in list {
        text.push_str(&p.to_string_lossy());
        text.push('\n');
    }
    let _ = write_atomic(&recent_path(), text.as_bytes());
}

/// Move `path` to the front of the list.
pub fn push_recent(list: &mut Vec<PathBuf>, path: &Path) {
    list.retain(|p| p != path);
    list.insert(0, path.to_path_buf());
    list.truncate(MAX_RECENT);
}

// ---------------------------------------------------------- indexed folders

fn indexed_path() -> PathBuf {
    base_dir().join("indexed")
}

/// Folders indexed for the quick file search.
pub fn load_indexed() -> Vec<PathBuf> {
    fs::read_to_string(indexed_path())
        .map(|t| t.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

pub fn save_indexed(list: &[PathBuf]) {
    let mut text = String::new();
    for p in list {
        text.push_str(&p.to_string_lossy());
        text.push('\n');
    }
    let _ = write_atomic(&indexed_path(), text.as_bytes());
}

/// Folders where the language servers are enabled.
pub fn load_lsp_folders() -> Vec<PathBuf> {
    fs::read_to_string(base_dir().join("lsp-folders"))
        .map(|t| t.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

pub fn save_lsp_folders(list: &[PathBuf]) {
    let mut text = String::new();
    for p in list {
        text.push_str(&p.to_string_lossy());
        text.push('\n');
    }
    let _ = write_atomic(&base_dir().join("lsp-folders"), text.as_bytes());
}

// ------------------------------------------------------------ git excluded

fn git_excluded_path() -> PathBuf {
    base_dir().join("git-excluded")
}

/// Lines `repository<TAB>rule`.
fn read_git_excluded() -> Vec<(String, String)> {
    fs::read_to_string(git_excluded_path())
        .map(|t| {
            t.lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn load_git_excluded(repo: &Path) -> crate::git::Excluded {
    let repo = repo.to_string_lossy();
    let mut ex = crate::git::Excluded::default();
    for (r, rule) in read_git_excluded() {
        if r == repo {
            ex.add(rule);
        }
    }
    ex
}

pub fn save_git_excluded(repo: &Path, ex: &crate::git::Excluded) {
    let repo = repo.to_string_lossy();
    let mut text = String::new();
    for (r, rule) in read_git_excluded() {
        if r != repo {
            text.push_str(&format!("{r}\t{rule}\n"));
        }
    }
    for rule in &ex.rules {
        text.push_str(&format!("{repo}\t{rule}\n"));
    }
    let _ = write_atomic(&git_excluded_path(), text.as_bytes());
}

// ------------------------------------------------------------------ session

#[derive(Clone, Debug, PartialEq)]
pub enum TabRef {
    File(PathBuf),
    Cache(String),
}

#[derive(Clone, Debug, Default)]
pub struct Session {
    /// (pane, tab)
    pub tabs: Vec<(usize, TabRef)>,
    pub active: [usize; 2],
    pub split: bool,
    /// Working folder shown in the side panel.
    pub workdir: Option<PathBuf>,
}

fn session_path() -> PathBuf {
    base_dir().join("session")
}

impl Session {
    pub fn load() -> Session {
        let mut s = Session::default();
        let Ok(text) = fs::read_to_string(session_path()) else { return s };
        for line in text.lines() {
            let mut f = line.splitn(3, '\t');
            match (f.next(), f.next(), f.next()) {
                (Some("active"), Some(a), Some(b)) => {
                    s.active = [a.parse().unwrap_or(0), b.parse().unwrap_or(0)];
                }
                (Some("split"), Some(v), _) => s.split = v == "true",
                (Some("workdir"), Some(p), _) if !p.is_empty() => s.workdir = Some(PathBuf::from(p)),
                (Some(pane), Some("file"), Some(p)) => {
                    s.tabs.push((pane.parse().unwrap_or(0).min(1), TabRef::File(PathBuf::from(p))))
                }
                (Some(pane), Some("cache"), Some(id)) => {
                    s.tabs.push((pane.parse().unwrap_or(0).min(1), TabRef::Cache(id.to_string())))
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let mut text = format!("split\t{}\nactive\t{}\t{}\n", self.split, self.active[0], self.active[1]);
        if let Some(w) = &self.workdir {
            text.push_str(&format!("workdir\t{}\n", w.to_string_lossy()));
        }
        for (pane, t) in &self.tabs {
            match t {
                TabRef::File(p) => text.push_str(&format!("{pane}\tfile\t{}\n", p.to_string_lossy())),
                TabRef::Cache(id) => text.push_str(&format!("{pane}\tcache\t{id}\n")),
            }
        }
        let _ = write_atomic(&session_path(), text.as_bytes());
    }
}

// -------------------------------------------------------------------- cache

#[derive(Clone, Debug)]
pub struct CacheMeta {
    pub path: Option<PathBuf>,
    pub encoding: Encoding,
    pub eol: Eol,
    pub lang: Lang,
    pub cursor: i32,
}

fn cache_files(id: &str) -> (PathBuf, PathBuf) {
    let d = cache_dir();
    (d.join(format!("{id}.txt")), d.join(format!("{id}.meta")))
}

pub fn write_cache(id: &str, text: &str, meta: &CacheMeta) -> io::Result<()> {
    let (txt, mf) = cache_files(id);
    write_atomic(&txt, text.as_bytes())?;
    let mut m = format!(
        "encoding={}\neol={}\nlang={}\ncursor={}\n",
        meta.encoding.id(),
        meta.eol.id(),
        meta.lang.id(),
        meta.cursor
    );
    if let Some(p) = &meta.path {
        m.push_str(&format!("path={}\n", p.to_string_lossy()));
    }
    write_atomic(&mf, m.as_bytes())
}

pub fn read_cache_meta(id: &str) -> Option<CacheMeta> {
    let (_, mf) = cache_files(id);
    let text = fs::read_to_string(mf).ok()?;
    let mut m = CacheMeta { path: None, encoding: Encoding::Utf8, eol: Eol::Lf, lang: Lang::Plain, cursor: 0 };
    for (k, v) in pairs(&text) {
        match k {
            "path" if !v.is_empty() => m.path = Some(PathBuf::from(v)),
            "encoding" => m.encoding = Encoding::from_id(v).unwrap_or(Encoding::Utf8),
            "eol" => m.eol = Eol::from_id(v).unwrap_or(Eol::Lf),
            "lang" => m.lang = Lang::from_id(v).unwrap_or(Lang::Plain),
            "cursor" => m.cursor = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(m)
}

pub fn read_cache_text(id: &str) -> io::Result<String> {
    fs::read_to_string(cache_files(id).0)
}

pub fn delete_cache(id: &str) {
    let (txt, mf) = cache_files(id);
    let _ = fs::remove_file(txt);
    let _ = fs::remove_file(mf);
}

/// Ids of all cached documents, oldest first.
pub fn cached_ids() -> Vec<String> {
    let Ok(rd) = fs::read_dir(cache_dir()) else { return Vec::new() };
    let mut ids: Vec<(std::time::SystemTime, String)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".meta")?.to_string();
            let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            Some((t, id))
        })
        .collect();
    ids.sort();
    ids.into_iter().map(|(_, id)| id).collect()
}

/// A new unique cache id.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{:x}-{:x}-{}", t.as_secs(), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}
