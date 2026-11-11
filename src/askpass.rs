//! A password a command asks for, asked for in the TUI.
//!
//! Commands run without a terminal (in a session of their own), so nothing
//! they start can read from the one docent draws on: a `sudo` that did was
//! stopped by the kernel for reading from a background process group, and
//! showed as running for ever. Instead, `sudo` in a command is a wrapper
//! that runs the real one with `-A`, and its askpass helper is docent
//! itself (`docent __askpass <prompt>`), which asks the TUI over a socket
//! in a folder only the user can open. The TUI shows the prompt, masked,
//! and the answer goes back to `sudo` and nowhere else; Esc gives none,
//! and `sudo` fails as it would on a wrong password.

/// The hidden subcommand `sudo` runs as its askpass helper.
pub const SUBCOMMAND: &str = "__askpass";

/// The variable that tells the helper where the TUI listens.
const SOCKET_VAR: &str = "DOCENT_ASKPASS";

/// One password asked for: the prompt the program gave, and where the
/// answer goes, `None` when it's declined.
pub struct Ask {
    pub prompt: String,
    pub reply: std::sync::mpsc::Sender<Option<String>>,
}

/// `docent __askpass <prompt>`: ask the TUI that ran the command, and
/// print its answer for `sudo`. Exits 1 when declined or there's no TUI
/// to ask, as askpass helpers do.
pub fn helper(prompt: Option<&str>) -> i32 {
    #[cfg(unix)]
    {
        use std::io::{BufRead, Write};
        let Ok(path) = std::env::var(SOCKET_VAR) else { return 1 };
        let Ok(mut socket) = std::os::unix::net::UnixStream::connect(path) else { return 1 };
        let prompt = prompt.unwrap_or("Password:").replace('\n', " ");
        if writeln!(socket, "{}", prompt).is_err() {
            return 1;
        }
        let mut answer = String::new();
        match std::io::BufReader::new(socket).read_line(&mut answer) {
            Ok(n) if n > 0 && answer.ends_with('\n') => {
                print!("{}", answer);
                0
            }
            _ => 1,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = prompt;
        1
    }
}

/// The folder a TUI's commands get their `sudo` and askpass helper from,
/// with the socket it listens on: removed when it's dropped.
pub struct Askpass {
    dir: std::path::PathBuf,
    env: Vec<(String, String)>,
}

impl Askpass {
    /// Set up the folder and listen, sending each password asked for to
    /// `ask`. `None` where it can't be (not unix, or no `sudo` to wrap):
    /// commands then run as before, and a `sudo` in them fails, not hangs.
    #[cfg(unix)]
    pub fn start(ask: impl Fn(Ask) + Send + 'static) -> Option<Askpass> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let sudo = find_in_path("sudo")?;
        let exe = std::env::current_exe().ok()?;
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = base.join(format!("docent-askpass-{}-{:x}", std::process::id(), nonce));
        // Only the user can reach the socket: the folder is theirs alone.
        std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
        let mut askpass = Askpass { env: Vec::new(), dir };
        let script = |name: &str, body: String| -> Option<std::path::PathBuf> {
            let path = askpass.dir.join(name);
            std::fs::write(&path, body).ok()?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).ok()?;
            Some(path)
        };
        let helper = script("askpass", format!("#!/bin/sh\nexec {} {} \"$@\"\n", quoted(&exe), SUBCOMMAND))?;
        script("sudo", format!("#!/bin/sh\nexec {} -A \"$@\"\n", quoted(&sudo)))?;
        let socket = askpass.dir.join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).ok()?;
        std::thread::spawn(move || serve(listener, ask));
        let path = std::env::var("PATH").unwrap_or_default();
        askpass.env = vec![
            ("PATH".to_string(), format!("{}:{}", askpass.dir.display(), path)),
            ("SUDO_ASKPASS".to_string(), helper.display().to_string()),
            (SOCKET_VAR.to_string(), socket.display().to_string()),
        ];
        Some(askpass)
    }

    #[cfg(not(unix))]
    pub fn start(_ask: impl Fn(Ask) + Send + 'static) -> Option<Askpass> {
        None
    }

    /// What a command's environment gets so its `sudo` asks here.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Answer each helper that connects: its prompt goes to `ask`, and the
/// answer back, or nothing when declined.
#[cfg(unix)]
fn serve(listener: std::os::unix::net::UnixListener, ask: impl Fn(Ask) + Send + 'static) {
    use std::io::{BufRead, Write};
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut prompt = String::new();
        let Ok(reader) = stream.try_clone() else { continue };
        if std::io::BufReader::new(reader).read_line(&mut prompt).is_err() {
            continue;
        }
        let (reply, answer) = std::sync::mpsc::channel();
        ask(Ask { prompt: prompt.trim().to_string(), reply });
        // Each on a thread of its own, so a prompt left open doesn't hold
        // up the next.
        std::thread::spawn(move || {
            if let Ok(Some(password)) = answer.recv() {
                let _ = writeln!(stream, "{}", password);
            }
        });
    }
}

/// `name`'s path, as the shell would find it.
#[cfg(unix)]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// `path` quoted for `sh`.
#[cfg(unix)]
fn quoted(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The helper asks over the socket and prints the answer; declined, it
    /// prints nothing and fails.
    #[test]
    fn a_password_goes_from_the_prompt_to_the_helper() {
        let Some(askpass) = Askpass::start(|ask: Ask| {
            let answer = (ask.prompt == "[sudo] password for me:").then(|| "hunter2".to_string());
            let _ = ask.reply.send(answer);
        }) else {
            return; // No sudo on this machine.
        };
        let env: std::collections::HashMap<_, _> = askpass.env().iter().cloned().collect();
        let dir = askpass.dir.clone();
        assert!(env["PATH"].starts_with(&dir.display().to_string()));
        assert!(dir.join("sudo").is_file() && dir.join("askpass").is_file());
        let mode = std::os::unix::fs::PermissionsExt::mode(&dir.metadata().unwrap().permissions());
        assert_eq!(mode & 0o777, 0o700);

        // The helper's side, as `docent __askpass` runs it.
        let ask = |prompt: &str| {
            use std::io::{BufRead, Write};
            let mut socket = std::os::unix::net::UnixStream::connect(&env[SOCKET_VAR]).unwrap();
            writeln!(socket, "{}", prompt).unwrap();
            let mut answer = String::new();
            std::io::BufReader::new(socket).read_line(&mut answer).unwrap();
            answer
        };
        assert_eq!(ask("[sudo] password for me:"), "hunter2\n");
        assert_eq!(ask("something else"), "");

        drop(askpass);
        assert!(!dir.exists(), "the folder goes with it");
    }
}
