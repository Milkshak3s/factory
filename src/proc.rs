//! Child-process plumbing: no console windows, streamed output, cancellation, and on Unix the
//! user's real PATH when launched from a desktop.

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::wsl;

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

/// Starts `cmd` in a process group of its own on Unix, so it can be signalled along with whatever
/// it spawns. Does nothing elsewhere.
pub fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

const PATH_BEGIN: &str = "__FACTORY_PATH_BEGIN__";
const PATH_END: &str = "__FACTORY_PATH_END__";
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// Launches from Finder or a desktop launcher get a minimal PATH that usually misses Homebrew and
/// `~/.local/bin`, so this replaces it with the login shell's, plus the usual install directories.
/// Does nothing on Windows. Call it before any other thread starts.
pub fn adopt_login_path() {
    if !cfg!(unix) {
        return;
    }
    let env_text = |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()).unwrap_or_default();
    let shell = Some(env_text("SHELL")).filter(|shell| !shell.is_empty()).unwrap_or_else(|| "/bin/sh".to_string());
    // A terminal launch already has this PATH, and an interactive shell in a background process
    // group would only stop itself waiting for that terminal, until the timeout.
    let login = if std::io::stdin().is_terminal() {
        None
    } else {
        read_marked_path(login_shell_command(&shell), LOGIN_SHELL_TIMEOUT)
    };
    let merged = merge_path(login.as_deref(), &env_text("PATH"), &env_text("HOME"));
    // SAFETY: main calls this first, before starting any thread that could read the environment.
    // The only other thread that can exist by now is read_marked_path's, which only reads a pipe.
    unsafe { std::env::set_var("PATH", merged) };
}

pub(crate) fn login_shell_command(shell: &str) -> Command {
    let mut cmd = command(shell);
    cmd.args(["-l", "-i", "-c"]).arg(format!("printf '%s%s%s' '{PATH_BEGIN}' \"$PATH\" '{PATH_END}'"));
    cmd
}

/// Runs `cmd` and returns the PATH it prints between the markers, or `None` on failure or timeout.
pub(crate) fn read_marked_path(cmd: Command, timeout: Duration) -> Option<String> {
    let (mut cmd, session) = wsl::into_session(cmd);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    own_process_group(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;

    // Read on a thread, since a daemon started by the shell's profile can hold the pipe open forever.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(count) = stdout.read(&mut chunk) {
            if count == 0 || tx.send(chunk[..count].to_vec()).is_err() {
                return;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut output: Vec<u8> = Vec::new();
    let found = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => {
                output.extend(chunk);
                if let Some(path) = parse_marked_path(&String::from_utf8_lossy(&output).replace('\0', "")) {
                    break path;
                }
            }
            Err(_) => {
                kill_tree(&mut child, session.as_ref());
                return None;
            }
        }
    };

    // Let it exit by itself, so that anything its profile left running in the background survives.
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            _ => return Some(found),
        }
    }
    kill_tree(&mut child, session.as_ref());
    Some(found)
}

/// Profiles can print anything, so only what's between the last pair of markers counts.
fn parse_marked_path(output: &str) -> Option<String> {
    let start = output.rfind(PATH_BEGIN)? + PATH_BEGIN.len();
    let rest = &output[start..];
    let end = rest.find(PATH_END)?;
    Some(rest[..end].to_string()).filter(|path| !path.is_empty())
}

/// The login PATH, then the inherited one, then the usual install directories, without repeats.
pub(crate) fn merge_path(login: Option<&str>, inherited: &str, home: &str) -> String {
    let mut known = vec!["/opt/homebrew/bin".to_string(), "/usr/local/bin".to_string()];
    if !home.is_empty() {
        known.extend([".local/bin", ".cargo/bin", ".claude/local"].map(|dir| format!("{home}/{dir}")));
    }
    let mut merged: Vec<&str> = Vec::new();
    let listed = login.unwrap_or_default().split(':').chain(inherited.split(':'));
    for dir in listed.chain(known.iter().map(String::as_str)) {
        if !dir.is_empty() && !merged.contains(&dir) {
            merged.push(dir);
        }
    }
    merged.join(":")
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

/// Runs a user-configured command line in `dir`: through `sh` inside the distro for a WSL path,
/// otherwise through the platform shell. Set any env on it, then pass it to `wsl::route`.
pub fn shell_in(dir: &Path, line: &str) -> Command {
    let mut cmd = if wsl::target(dir).is_some() {
        let mut cmd = command("sh");
        cmd.arg("-c").arg(line);
        cmd
    } else {
        shell(line)
    };
    cmd.current_dir(dir);
    cmd
}

/// Runs `cmd` to completion and collects its output, killing it if `timeout` passes first.
pub(crate) fn output_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    // Read on threads: a full pipe would stall the child, and a daemon can hold one open forever.
    fn collect(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
        rx
    }
    let stdout = collect(child.stdout.take().expect("stdout is piped"));
    let stderr = collect(child.stderr.take().expect("stderr is piped"));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child, None);
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let grace = Duration::from_millis(500);
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(grace).unwrap_or_default(),
        stderr: stderr.recv_timeout(grace).unwrap_or_default(),
    })
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

fn pump(pipe: impl Read + Send + 'static, tx: mpsc::Sender<String>, strip_nul: bool) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let mut line = String::from_utf8_lossy(&buf).into_owned();
                    if strip_nul {
                        line.retain(|c| c != '\0');
                    }
                    if tx.send(line.trim_end().to_string()).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// The command that kills `pid` and its descendants. On Unix that means its process group, which
/// `own_process_group` gave it. The `--` matters: some versions of `kill` otherwise read a negative
/// PID as options, and procps's then signals the wrong target.
fn tree_kill_command(windows: bool, pid: u32) -> (&'static str, Vec<String>) {
    if windows {
        ("taskkill", ["/T", "/F", "/PID"].map(str::to_string).into_iter().chain([pid.to_string()]).collect())
    } else {
        ("kill", vec!["-KILL".to_string(), "--".to_string(), format!("-{pid}")])
    }
}

/// `session` is what's running inside a distro, which killing the Windows process can leave behind.
fn kill_tree(child: &mut Child, session: Option<&wsl::Session>) {
    if let Some(session) = session {
        wsl::kill_session(session);
    }
    // Test runners and agents spawn their own children, which Child::kill would orphan. The child
    // hasn't been reaped yet, so its PID, and so its group's ID, can't have been reused.
    let (program, args) = tree_kill_command(cfg!(windows), child.id());
    let _ = command(program).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs `cmd`, handing each stdout/stderr line to `on_line` as it arrives.
pub fn run_streaming(
    cmd: Command,
    input: Option<String>,
    cancel: &Cancel,
    on_line: &mut dyn FnMut(&str),
) -> std::io::Result<Exit> {
    let (mut cmd, session) = wsl::into_session(cmd);
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut cmd);
    let mut child = cmd.spawn()?;

    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }

    let (tx, rx) = mpsc::channel();
    pump(child.stdout.take().expect("stdout is piped"), tx.clone(), session.is_some());
    pump(child.stderr.take().expect("stderr is piped"), tx, session.is_some());

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => on_line(&line),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(50)),
        }
        if cancel.is_cancelled() {
            kill_tree(&mut child, session.as_ref());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_read_from_between_the_markers_whatever_surrounds_it() {
        let marked = |path: &str| format!("{PATH_BEGIN}{path}{PATH_END}");
        assert_eq!(parse_marked_path(&marked("/a:/b")).as_deref(), Some("/a:/b"));
        let noisy = format!("Welcome!\n{}\nBye\n", marked("/a"));
        assert_eq!(parse_marked_path(&noisy).as_deref(), Some("/a"));
        let echoed = format!("{}\n{}", marked("$PATH"), marked("/real"));
        assert_eq!(parse_marked_path(&echoed).as_deref(), Some("/real"));
    }

    #[test]
    fn missing_markers_or_an_empty_path_give_nothing() {
        assert_eq!(parse_marked_path(""), None);
        assert_eq!(parse_marked_path("/usr/bin"), None);
        assert_eq!(parse_marked_path(&format!("{PATH_BEGIN}/cut/off")), None);
        assert_eq!(parse_marked_path(&format!("/a{PATH_END}")), None);
        assert_eq!(parse_marked_path(&format!("{PATH_END}/a{PATH_BEGIN}")), None);
        assert_eq!(parse_marked_path(&format!("{PATH_BEGIN}{PATH_END}")), None);
    }

    #[test]
    fn the_login_path_comes_first_then_inherited_then_known_without_repeats() {
        let merged = merge_path(Some("/login:/shared:/usr/local/bin"), "/inherited:/shared::/login", "/home/jo");
        assert_eq!(
            merged,
            "/login:/shared:/usr/local/bin:/inherited:/opt/homebrew/bin:\
             /home/jo/.local/bin:/home/jo/.cargo/bin:/home/jo/.claude/local"
        );
    }

    #[test]
    fn a_failed_login_shell_leaves_the_inherited_path_and_known_dirs() {
        assert_eq!(merge_path(None, "/usr/bin:/bin", ""), "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin");
    }

    #[test]
    fn the_login_shell_is_asked_for_its_path_with_a_login_and_interactive_start() {
        let cmd = login_shell_command("/bin/zsh");
        assert_eq!(cmd.get_program(), "/bin/zsh");
        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..3], ["-l", "-i", "-c"]);
        assert_eq!(args[3], format!("printf '%s%s%s' '{PATH_BEGIN}' \"$PATH\" '{PATH_END}'"));
    }

    #[test]
    fn a_tree_is_killed_with_taskkill_or_by_its_process_group() {
        let (program, args) = tree_kill_command(true, 42);
        assert_eq!(program, "taskkill");
        assert_eq!(args, ["/T", "/F", "/PID", "42"]);
        let (program, args) = tree_kill_command(false, 42);
        assert_eq!(program, "kill");
        assert_eq!(args, ["-KILL", "--", "-42"]);
    }

    #[test]
    fn a_wsl_directory_gets_sh_and_any_other_keeps_the_platform_shell() {
        let cmd = shell_in(Path::new(r"\\wsl.localhost\Ubuntu\home\jo\repo"), "npm ci && npm test");
        assert_eq!(cmd.get_program(), "sh");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["-c", "npm ci && npm test"]);

        let native = shell_in(Path::new("some-dir"), "make test");
        assert_eq!(native.get_program(), shell("make test").get_program());
        assert_eq!(native.get_current_dir(), Some(Path::new("some-dir")));
    }

    #[test]
    fn output_is_collected_and_a_missing_program_is_an_error() {
        let output = output_timeout(shell("echo hello"), Duration::from_secs(30)).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
        assert!(output_timeout(command("factory-no-such-program"), Duration::from_secs(30)).is_err());
    }

    #[cfg(unix)]
    fn sh_command(script: &str) -> Command {
        let mut cmd = command("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_that_prints_noise_and_its_path_is_read() {
        let script = format!("echo noise; printf '%s%s%s' '{PATH_BEGIN}' /from/shell '{PATH_END}'; echo more");
        let found = read_marked_path(sh_command(&script), Duration::from_secs(10));
        assert_eq!(found.as_deref(), Some("/from/shell"));
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_that_hangs_times_out_and_one_that_prints_nothing_gives_nothing() {
        let started = Instant::now();
        assert_eq!(read_marked_path(sh_command("sleep 30"), Duration::from_millis(200)), None);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(read_marked_path(sh_command("exit 1"), Duration::from_secs(10)), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_background_process_holding_the_pipe_does_not_block_the_read() {
        let script = format!("sleep 5 & printf '%s%s%s' '{PATH_BEGIN}' /a '{PATH_END}'");
        let started = Instant::now();
        assert_eq!(read_marked_path(sh_command(&script), Duration::from_secs(4)).as_deref(), Some("/a"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_also_kills_grandchildren() {
        let cancel = Cancel::default();
        let trigger = cancel.clone();
        let mut grandchild = String::new();
        let exit = run_streaming(sh_command("sleep 60 & echo $!; wait"), None, &cancel, &mut |line| {
            grandchild = line.to_string();
            trigger.cancel();
        })
        .unwrap();
        assert_eq!(exit, Exit::Cancelled);
        assert!(!grandchild.is_empty());

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = command("kill").args(["-0", grandchild.as_str()]).stderr(Stdio::null()).status().unwrap();
            if !status.success() {
                break;
            }
            assert!(Instant::now() < deadline, "the grandchild {grandchild} survived the cancel");
            thread::sleep(Duration::from_millis(50));
        }
    }
}
