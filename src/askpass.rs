//! Questions of git and ssh (user names, passwords, key passphrases, new
//! host keys) answered in the editor.
//!
//! Network git commands run with `GIT_ASKPASS` and `SSH_ASKPASS` pointing
//! to this same program. Started that way, it does not open a window: it
//! sends the question to the running editor over a private socket (in the
//! user's runtime folder, with a random token), prints the answer and
//! exits. Nothing is stored here: remembered passwords go to the system
//! keyring through git's own credential helper.

use gtk::prelude::*;
use gtk::{gio, glib};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;

const ENV_SOCKET: &str = "MACACO_ASKPASS";
const ENV_TOKEN: &str = "MACACO_ASKPASS_TOKEN";

/// Askpass mode: when git or ssh run this program, ask the editor and
/// return the exit code (`None`: a normal start).
pub fn client() -> Option<i32> {
    let socket = std::env::var_os(ENV_SOCKET)?;
    let token = std::env::var(ENV_TOKEN).unwrap_or_default();
    let prompt = std::env::args().nth(1).unwrap_or_default();
    // ssh: "confirm" (yes/no) or "none" (only a notice).
    let kind = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    let ask = || -> std::io::Result<Option<String>> {
        let mut s = UnixStream::connect(socket)?;
        s.write_all(format!("{token}\n{kind}\n{prompt}").as_bytes())?;
        s.shutdown(std::net::Shutdown::Write)?;
        let mut reply = String::new();
        s.read_to_string(&mut reply)?;
        Ok(reply.strip_prefix("OK\n").map(str::to_string))
    };
    Some(match ask() {
        Ok(Some(answer)) => {
            println!("{answer}");
            0
        }
        _ => 1,
    })
}

/// A question from git or ssh.
pub struct Request {
    /// `SSH_ASKPASS_PROMPT`: "confirm", "none" or empty (a text answer).
    pub kind: String,
    pub prompt: String,
}

/// Called with each request and the function that sends the answer
/// (`None`: cancelled).
pub type Handler = Rc<dyn Fn(Request, Box<dyn FnOnce(Option<String>)>)>;

/// The socket the askpass helpers connect to.
pub struct Server {
    path: PathBuf,
    /// The folder was created for the socket (removed with it).
    own_dir: bool,
    token: String,
    exe: PathBuf,
    service: gio::SocketService,
}

/// A secret shared with the helpers; None if no random bytes are
/// available (then nothing is asked in the editor).
fn random_token() -> Option<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Folder of the socket: the user's runtime folder (private), else a new
/// private folder in the temporary one, so that the socket is never
/// reachable by others, not even before its permissions are set.
fn socket_dir() -> Result<(PathBuf, bool), String> {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()) {
        return Ok((d, false));
    }
    use std::os::unix::fs::DirBuilderExt;
    let d = std::env::temp_dir().join(format!("macaco-askpass-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::DirBuilder::new().mode(0o700).create(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok((d, true))
}

/// This program, also after an upgrade replaced the file on disk.
fn own_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if exe.exists() {
        return Some(exe);
    }
    let s = exe.to_string_lossy();
    let p = PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s));
    p.exists().then_some(p)
}

impl Server {
    pub fn start(handler: Handler) -> Result<Server, String> {
        let exe = own_exe().ok_or("percorso del programma non disponibile")?;
        let token = random_token().ok_or("nessuna sorgente di numeri casuali")?;
        let (dir, own_dir) = socket_dir()?;
        let path = dir.join(format!("macaco-askpass-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let service = gio::SocketService::new();
        let addr = gio::UnixSocketAddress::new(&path);
        service
            .add_address(&addr, gio::SocketType::Stream, gio::SocketProtocol::Default, None::<&glib::Object>)
            .map_err(|e| e.to_string())?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let expected = token.clone();
        service.connect_incoming(move |_, conn, _| {
            let (conn, handler, expected) = (conn.clone(), handler.clone(), expected.clone());
            glib::MainContext::default().spawn_local(async move {
                let input = conn.input_stream();
                let mut data = Vec::new();
                while let Ok(chunk) = input.read_bytes_future(4096, glib::Priority::DEFAULT).await {
                    if chunk.is_empty() || data.len() > 64 * 1024 {
                        break;
                    }
                    data.extend_from_slice(&chunk);
                }
                let text = String::from_utf8_lossy(&data).into_owned();
                let mut parts = text.splitn(3, '\n');
                let (token, kind, prompt) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                if token != expected {
                    let _ = conn.close_future(glib::Priority::DEFAULT).await;
                    return;
                }
                let reply = Box::new(move |answer: Option<String>| {
                    let msg = match answer {
                        Some(a) => {
                            let mut a = a.into_bytes();
                            let mut msg = b"OK\n".to_vec();
                            msg.extend_from_slice(&a);
                            a.fill(0);
                            msg
                        }
                        None => b"NO".to_vec(),
                    };
                    glib::MainContext::default().spawn_local(async move {
                        let out = conn.output_stream();
                        // The answer may be a password: not left in memory.
                        let mut sent = match out.write_all_future(msg, glib::Priority::DEFAULT).await {
                            Ok((b, ..)) | Err((b, _)) => b,
                        };
                        sent.fill(0);
                        let _ = conn.close_future(glib::Priority::DEFAULT).await;
                    });
                });
                handler(Request { kind: kind.to_string(), prompt: prompt.to_string() }, reply);
            });
            true
        });
        service.start();
        Ok(Server { path, own_dir, token, exe, service })
    }

    /// Environment of a network git command: its questions come here.
    pub fn env(&self) -> Vec<(String, String)> {
        let exe = self.exe.to_string_lossy().into_owned();
        vec![
            ("GIT_ASKPASS".into(), exe.clone()),
            ("SSH_ASKPASS".into(), exe),
            // ssh asks through SSH_ASKPASS even without a display.
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (ENV_SOCKET.into(), self.path.to_string_lossy().into_owned()),
            (ENV_TOKEN.into(), self.token.clone()),
        ]
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.service.stop();
        let _ = std::fs::remove_file(&self.path);
        if self.own_dir
            && let Some(d) = self.path.parent()
        {
            let _ = std::fs::remove_dir(d);
        }
    }
}
