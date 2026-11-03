//! Child-process plumbing: no console windows, streamed output, cancellation.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A `Command` that won't flash a console window when run from the GUI.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Runs a user-configured command line through the platform shell.
pub fn shell(line: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = command("cmd");
        // raw_arg: cmd.exe has its own quoting rules, so pass the line untouched.
        cmd.raw_arg("/C").raw_arg(line);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = command("sh");
        cmd.arg("-c").arg(line);
        cmd
    }
}

#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, PartialEq)]
pub enum Exit {
    Code(i32),
    Cancelled,
}

fn pump(pipe: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    if tx.send(line.trim_end().to_string()).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

fn kill_tree(child: &mut Child) {
    // Test runners and agents spawn their own children, which Child::kill would orphan.
    #[cfg(windows)]
    {
        let _ = command("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs `cmd`, handing each stdout/stderr line to `on_line` as it arrives.
pub fn run_streaming(
    mut cmd: Command,
    input: Option<String>,
    cancel: &Cancel,
    on_line: &mut dyn FnMut(&str),
) -> std::io::Result<Exit> {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }

    let (tx, rx) = mpsc::channel();
    pump(child.stdout.take().expect("stdout is piped"), tx.clone());
    pump(child.stderr.take().expect("stderr is piped"), tx);

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => on_line(&line),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(50)),
        }
        if cancel.is_cancelled() {
            kill_tree(&mut child);
            return Ok(Exit::Cancelled);
        }
        if let Some(status) = child.try_wait()? {
            // A surviving grandchild can hold the pipes open, so don't wait for EOF forever.
            let deadline = Instant::now() + Duration::from_millis(500);
            while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                on_line(&line);
            }
            return Ok(Exit::Code(status.code().unwrap_or(-1)));
        }
    }
}
