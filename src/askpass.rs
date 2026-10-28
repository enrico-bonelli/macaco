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
    token: String,
    exe: PathBuf,
    service: gio::SocketService,
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
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
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
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
        let token = random_token();
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
                        Some(a) => format!("OK\n{a}"),
                        None => "NO".to_string(),
                    };
                    glib::MainContext::default().spawn_local(async move {
                        let out = conn.output_stream();
                        let _ = out.write_all_future(msg.into_bytes(), glib::Priority::DEFAULT).await;
                        let _ = conn.close_future(glib::Priority::DEFAULT).await;
                    });
                });
                handler(Request { kind: kind.to_string(), prompt: prompt.to_string() }, reply);
            });
            true
        });
        service.start();
        Ok(Server { path, token, exe, service })
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
    }
}
