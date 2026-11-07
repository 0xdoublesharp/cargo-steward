//! Runs external commands with a time limit, so one stuck tool (a hung
//! sccache server, a slow network mount) can't hang `doctor`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long any one command may take.
pub const LIMIT: Duration = Duration::from_secs(20);

/// What a command printed, and whether it succeeded.
#[derive(Debug, Clone, Default)]
pub struct Ran {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `command` with no input. `None` if it can't be started, or doesn't
/// finish within `limit` (it is killed then).
pub fn run(command: &mut Command, limit: Duration) -> Option<Ran> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    let (tx, rx) = mpsc::channel();
    // Read both pipes while the command runs, so it never blocks on a full
    // one. The readers are detached: a daemon the command started (such as
    // the sccache server) may hold the pipe open after the command exits.
    read_detached(child.stdout.take(), 0, tx.clone());
    read_detached(child.stderr.take(), 1, tx);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut ran = Ran {
        success: status.success(),
        ..Ran::default()
    };
    for _ in 0..2 {
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(200));
        match rx.recv_timeout(wait) {
            Ok((0, bytes)) => ran.stdout = String::from_utf8_lossy(&bytes).into_owned(),
            Ok((_, bytes)) => ran.stderr = String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => break,
        }
    }
    Some(ran)
}

/// Reads `pipe` to its end on its own thread, then sends `(stream, bytes)`.
fn read_detached<R: Read + Send + 'static>(
    pipe: Option<R>,
    stream: u8,
    tx: mpsc::Sender<(u8, Vec<u8>)>,
) {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = tx.send((stream, bytes));
    });
}

/// [`run`] with the default limit, returning stdout of a successful run.
pub fn stdout(command: &mut Command) -> Option<String> {
    run(command, LIMIT).filter(|r| r.success).map(|r| r.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captures_output_and_gives_up_on_slow_commands() {
        let ran = run(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            LIMIT,
        )
        .unwrap();
        assert!(!ran.success);
        assert_eq!(ran.stdout.trim(), "out");
        assert_eq!(ran.stderr.trim(), "err");

        let started = Instant::now();
        assert!(run(Command::new("sleep").arg("5"), Duration::from_millis(100)).is_none());
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(stdout(&mut Command::new("/nonexistent/program")).is_none());
    }
}
