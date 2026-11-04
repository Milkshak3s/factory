//! Projects on a WSL distro's Linux filesystem. Every tool for them runs inside the distro through
//! `wsl.exe`, so this module turns commands, paths and distro state into that form. The decisions
//! are pure functions and compile on every OS, because only a real Windows path can ever match.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::proc::{self, command};

const WSL_EXE: &str = "wsl.exe";
const CANONICAL_HOST: &str = "wsl.localhost";
const ENV_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const TOOLS_TIMEOUT: Duration = Duration::from_secs(60);
const KILL_TIMEOUT: Duration = Duration::from_secs(15);
/// Used when the distro can't tell us its own, plus what `proc::merge_path` knows about.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const REQUIRED_TOOLS: [&str; 4] = ["git", "make", "claude", "gh"];

pub const WINDOWS_DRIVE_NOTICE: &str =
    "Only repos on a distro's Linux filesystem are supported, and this one is on a Windows drive (/mnt/...).";

/// A path inside a distro.
#[derive(Clone, Debug, PartialEq)]
pub struct WslPath {
    pub distro: String,
    /// Always absolute, with `/` separators.
    pub linux: String,
}

impl WslPath {
    pub fn is_on_windows_drive(&self) -> bool {
        self.linux == "/mnt" || self.linux.starts_with("/mnt/")
    }
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// Reads `\\wsl.localhost\<distro>\...` or `\\wsl$\<distro>\...`, also in their `\\?\UNC\` forms.
/// Anything else isn't a WSL path. That includes a Unix path, so native projects never match.
pub fn parse(path: &Path) -> Option<WslPath> {
    let text = path.to_str()?;
    let rest = strip_prefix_ignoring_case(text, r"\\?\UNC\").or_else(|| text.strip_prefix(r"\\"))?;
    let (host, rest) = rest.split_once('\\')?;
    if !host.eq_ignore_ascii_case(CANONICAL_HOST) && !host.eq_ignore_ascii_case("wsl$") {
        return None;
    }
    let mut parts = rest.split(['\\', '/']);
    let distro = parts.next().filter(|distro| !distro.is_empty())?;
    let mut linux: Vec<&str> = Vec::new();
    for part in parts {
        match part {
            "" | "." => {}
            ".." => {
                linux.pop();
            }
            part => linux.push(part),
        }
    }
    Some(WslPath { distro: distro.to_string(), linux: format!("/{}", linux.join("/")) })
}

/// The distro and Linux path that a project or working directory is in, if any. It only reads the
/// path, so it never wakes the distro.
pub fn target(path: &Path) -> Option<WslPath> {
    parse(path)
}

/// The reverse of `parse`, always under `\\wsl.localhost`.
pub fn to_unc(distro: &str, linux: &str) -> PathBuf {
    PathBuf::from(format!(r"\\{CANONICAL_HOST}\{distro}{}", linux.replace('/', r"\")))
}

/// One spelling for each WSL path, so a repo can't be added twice. Other paths are left alone.
pub fn normalise(path: &Path) -> PathBuf {
    match parse(path) {
        Some(wsl) => to_unc(&wsl.distro, &wsl.linux),
        None => path.to_path_buf(),
    }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a.distro.eq_ignore_ascii_case(&b.distro) && a.linux == b.linux,
        _ => a == b,
    }
}

pub fn is_on_windows_drive(path: &Path) -> bool {
    parse(path).is_some_and(|wsl| wsl.is_on_windows_drive())
}

/// How to name `path` to a tool: its Linux path if it's in a distro, otherwise as it is.
pub fn tool_path(path: &Path) -> String {
    match parse(path) {
        Some(wsl) => wsl.linux,
        None => path.to_string_lossy().into_owned(),
    }
}

/// `$HOME/.local/share/factory/worktrees/<slug>/<id>`
pub fn worktree_dir(home: &str, slug: &str, id: &str) -> String {
    format!("{}/.local/share/factory/worktrees/{slug}/{id}", home.trim_end_matches('/'))
}

/// `$HOME/.local/share/factory/target/<slug>`
pub fn cargo_target_dir(home: &str, slug: &str) -> String {
    format!("{}/.local/share/factory/target/{slug}", home.trim_end_matches('/'))
}

/// The program to start for a tool: its bare name in a distro, otherwise where it was located.
pub fn program(dir: &Path, name: &str, located: &Path) -> PathBuf {
    if target(dir).is_some() { PathBuf::from(name) } else { located.to_path_buf() }
}

/// `wsl.exe -d <distro> --cd <dir>`, ready for `--exec`. `proc::command` keeps its console hidden,
/// and `WSL_UTF8` keeps wsl.exe's own messages from being UTF-16. It gets no working directory of
/// its own: `--cd` does that, and a UNC directory there would fail before wsl.exe even started.
fn base_command(distro: &str, dir: &str) -> Command {
    let mut cmd = command(WSL_EXE);
    cmd.args(["-d", distro, "--cd", dir]).env("WSL_UTF8", "1");
    cmd
}

fn pair(key: &OsStr, value: &OsStr) -> OsString {
    let mut text = key.to_os_string();
    text.push("=");
    text.push(value);
    text
}

/// `cmd` as `wsl.exe -d <distro> --cd <dir> --exec env PATH=<path> K=V ... <program> <args...>`.
/// No shell parses the arguments on the way, and `--exec` starts a process with no environment
/// from Windows, so env has to go in explicitly. Removed variables were never forwarded anyway.
pub fn translate(cmd: &Command, target: &WslPath, path: &str) -> Command {
    let mut routed = base_command(&target.distro, &target.linux);
    routed.args(["--exec", "env"]).arg(format!("PATH={path}"));
    for (key, value) in cmd.get_envs() {
        if let Some(value) = value {
            routed.arg(pair(key, value));
        }
    }
    routed.arg(cmd.get_program()).args(cmd.get_args());
    routed
}

/// Sends `cmd` through `wsl.exe` if its working directory is in a distro. Set up its program, args
/// and env first: env added afterwards wouldn't reach the Linux process. Other commands are
/// returned as they are. The first call for a distro starts it, to read its PATH.
pub fn route(cmd: Command) -> Command {
    let Some(target) = cmd.get_current_dir().and_then(parse) else { return cmd };
    let path = env_for(&target.distro).path;
    translate(&cmd, &target, &path)
}

// --- Finding the distro's PATH and home ---

#[derive(Clone, Debug, PartialEq)]
pub struct DistroEnv {
    pub path: String,
    pub home: Option<String>,
}

type EnvSlot = Arc<Mutex<Option<DistroEnv>>>;

/// Each distro has its own lock, so concurrent status threads wait for one read of its PATH.
fn env_slot(distro: &str) -> EnvSlot {
    static SLOTS: OnceLock<Mutex<HashMap<String, EnvSlot>>> = OnceLock::new();
    let mut slots = SLOTS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    slots.entry(distro.to_ascii_lowercase()).or_default().clone()
}

/// The distro's env, reading it the first time. Can start the distro.
pub fn env_for(distro: &str) -> DistroEnv {
    let slot = env_slot(distro);
    let mut env = slot.lock().unwrap_or_else(|e| e.into_inner());
    env.get_or_insert_with(|| read_env(distro)).clone()
}

/// Reads the env again, so a newly installed tool is picked up. Can start the distro.
pub fn refresh_env(distro: &str) -> DistroEnv {
    let slot = env_slot(distro);
    let mut env = slot.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = read_env(distro);
    *env = Some(fresh.clone());
    fresh
}

#[cfg(test)]
pub fn seed_env(distro: &str, path: &str, home: &str) {
    *env_slot(distro).lock().unwrap() = Some(DistroEnv { path: path.to_string(), home: Some(home.to_string()) });
}

#[derive(Debug, PartialEq)]
struct Identity {
    inherited_path: String,
    home: Option<String>,
    shell: String,
}

/// `$SHELL` may be unset under `--exec`, so the user's shell comes from the password database.
const IDENTITY_SCRIPT: &str = r#"printf '%s\n%s\n' "$PATH" "$HOME"; getent passwd "$(id -u)""#;

/// Reads what `IDENTITY_SCRIPT` prints. An entry's fields are `name:pw:uid:gid:gecos:home:shell`.
fn parse_identity(output: &str) -> Identity {
    let mut lines = output.lines();
    let inherited_path = lines.next().unwrap_or_default().trim().to_string();
    let env_home = lines.next().unwrap_or_default().trim().to_string();
    let entry: Vec<&str> = lines.next().unwrap_or_default().trim().split(':').collect();
    let field = |index: usize| entry.get(index).map(|value| value.trim()).filter(|value| !value.is_empty());
    let home = field(5).map(str::to_string).or(Some(env_home).filter(|home| home.starts_with('/')));
    Identity { inherited_path, home, shell: field(6).unwrap_or("/bin/sh").to_string() }
}

fn exec_in(distro: &str, program: impl AsRef<OsStr>, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Command {
    let mut cmd = base_command(distro, "/");
    cmd.arg("--exec").arg(program).args(args);
    cmd
}

/// Reads PATH from the user's interactive login shell, as `proc.rs` does on Unix. If that fails, it
/// falls back to the distro's default PATH plus the known directories.
fn read_env(distro: &str) -> DistroEnv {
    let started = Instant::now();
    let identity = proc::output_timeout(exec_in(distro, "sh", ["-c", IDENTITY_SCRIPT]), ENV_TIMEOUT)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_identity(&clean_text(&output.stdout)));
    let Some(identity) = identity else {
        return DistroEnv { path: proc::merge_path(None, DEFAULT_PATH, ""), home: None };
    };
    let login = proc::login_shell_command(&identity.shell);
    let args: Vec<&OsStr> = login.get_args().collect();
    let remaining = ENV_TIMEOUT.saturating_sub(started.elapsed());
    let login_path = proc::read_marked_path(exec_in(distro, &identity.shell, args), remaining);
    let inherited = if identity.inherited_path.is_empty() { DEFAULT_PATH } else { identity.inherited_path.as_str() };
    let home = identity.home.as_deref().unwrap_or_default();
    DistroEnv { path: proc::merge_path(login_path.as_deref(), inherited, home), home: identity.home.clone() }
}

// --- Cancellation ---

/// A program started in a session of its own inside a distro, which can be killed from outside.
#[derive(Debug)]
pub struct Session {
    distro: String,
    pidfile: String,
}

/// Records the session leader's PID, runs the program and cleans up after it. It doesn't `exec`, so
/// that the file gets removed. `$0` is the file and `"$@"` the untouched program and arguments.
const SESSION_SCRIPT: &str = r#"echo $$ > "$0"; "$@"; code=$?; rm -f "$0"; exit $code"#;

/// Waits briefly in case the program is still starting, then kills its process group and, for
/// anything that left the group, what's left of the session. `pkill` may not be installed. `-s KILL`
/// is the form that every `sh` builtin accepts along with `--`.
const KILL_SCRIPT: &str = r#"i=0; while [ ! -s "$0" ] && [ "$i" -lt 5 ]; do sleep 1; i=$((i+1)); done; pid=$(cat "$0" 2>/dev/null); case "$pid" in ''|*[!0-9]*) ;; *) kill -s KILL -- "-$pid" 2>/dev/null || kill -KILL "-$pid" 2>/dev/null; pkill -KILL -s "$pid" 2>/dev/null ;; esac; rm -f "$0""#;

fn new_pidfile() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_nanos());
    format!("/tmp/factory-{}-{count}-{nanos}.pid", std::process::id())
}

/// `args` with the session wrapper put in front of the program. `None` if there's no `--exec`.
fn session_args(args: &[OsString], pidfile: &str) -> Option<Vec<OsString>> {
    let exec = args.iter().position(|arg| arg == "--exec")?;
    let mut program = exec + 1;
    if args.get(program).is_some_and(|arg| arg == "env") {
        program += 1;
        while args.get(program).is_some_and(|arg| arg.to_string_lossy().contains('=')) {
            program += 1;
        }
    }
    let wrapper = ["setsid", "--wait", "sh", "-c", SESSION_SCRIPT, pidfile].map(OsString::from);
    let mut wrapped = args[..program].to_vec();
    wrapped.extend(wrapper);
    wrapped.extend_from_slice(&args[program..]);
    Some(wrapped)
}

/// If `cmd` runs inside a distro, starts its program in a session that `kill_session` can reach,
/// since killing `wsl.exe` can leave the Linux processes running. Other commands are unchanged.
pub fn into_session(cmd: Command) -> (Command, Option<Session>) {
    if cmd.get_program() != WSL_EXE {
        return (cmd, None);
    }
    let args: Vec<OsString> = cmd.get_args().map(OsStr::to_os_string).collect();
    let distro = args.iter().position(|arg| arg == "-d").and_then(|at| args.get(at + 1));
    let distro = distro.map(|name| name.to_string_lossy().into_owned());
    let pidfile = new_pidfile();
    let (Some(distro), Some(wrapped)) = (distro, session_args(&args, &pidfile)) else { return (cmd, None) };
    let mut session_cmd = command(WSL_EXE);
    session_cmd.args(wrapped);
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => session_cmd.env(key, value),
            None => session_cmd.env_remove(key),
        };
    }
    (session_cmd, Some(Session { distro, pidfile }))
}

/// Kills the session's processes and removes its temp file, through a second `wsl.exe`.
pub fn kill_session(session: &Session) {
    let cmd = exec_in(&session.distro, "sh", ["-c", KILL_SCRIPT, session.pidfile.as_str()]);
    let _ = proc::output_timeout(cmd, KILL_TIMEOUT);
}

// --- Output from wsl.exe ---

/// Text from `wsl.exe`, without the NUL bytes or byte order mark left by UTF-16.
pub fn clean_text(bytes: &[u8]) -> String {
    let bytes: Vec<u8> = bytes.iter().copied().filter(|byte| *byte != 0).collect();
    String::from_utf8_lossy(&bytes).replace(['\u{feff}', '\u{fffd}'], "")
}

/// Why a command failed. wsl.exe prints its own errors to stdout, so through it that's the fallback
/// when stderr has nothing. Only stderr is cleaned: stdout can carry NULs that a tool put there.
pub fn failure_detail(routed: bool, stdout: &[u8], stderr: &[u8]) -> String {
    if !routed {
        return String::from_utf8_lossy(stderr).trim().to_string();
    }
    let detail = clean_text(stderr);
    if detail.trim().is_empty() { clean_text(stdout).trim().to_string() } else { detail.trim().to_string() }
}

// --- Which distros are running ---

fn looks_like_a_distro_name(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Reads `wsl.exe --list --running --quiet`. It exits non-zero when nothing's running, which isn't
/// the same as WSL being unavailable: that one is an `Err`, and its messages say so.
pub fn parse_running(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<Vec<String>, String> {
    let out = clean_text(stdout);
    if success {
        // Names have no spaces, so a message on stdout can't pass for a distro.
        return Ok(out.lines().map(str::trim).filter(|line| looks_like_a_distro_name(line)).map(str::to_string).collect());
    }
    let err = clean_text(stderr);
    let text = format!("{out}\n{err}");
    let lowered = text.to_lowercase();
    let unavailable = ["not installed", "no installed distributions", "wsl/", "error code", "not enabled", "virtual machine"];
    if unavailable.iter().any(|sign| lowered.contains(*sign)) {
        return Err(text.trim().lines().next().unwrap_or("wsl.exe failed").trim().to_string());
    }
    Ok(Vec::new())
}

/// Never starts a distro.
pub fn list_running() -> Result<Vec<String>, String> {
    let mut cmd = command(WSL_EXE);
    cmd.args(["--list", "--running", "--quiet"]).env("WSL_UTF8", "1");
    match proc::output_timeout(cmd, LIST_TIMEOUT) {
        Ok(output) => parse_running(output.status.success(), &output.stdout, &output.stderr),
        Err(error) => Err(format!("could not run {WSL_EXE}: {error}")),
    }
}

/// What can be done with a distro's projects right now. Kept apart from their `RepoStatus`, so that
/// a project whose distro is stopped still shows the last status it had.
#[derive(Clone, Debug, PartialEq)]
pub enum DistroState {
    Available,
    Stopped,
    Unavailable(String),
}

impl DistroState {
    /// A short line for the project list.
    pub fn label(&self, distro: &str) -> Option<String> {
        match self {
            DistroState::Available => None,
            DistroState::Stopped => Some(format!("{distro} is stopped")),
            DistroState::Unavailable(reason) => Some(format!("WSL isn't available: {reason}")),
        }
    }
}

/// An automatic refresh must not wake a stopped distro, or Factory would keep its VM alive by
/// merely being open. A click on *Refresh* or *Add…* can.
pub fn refresh_decision(manual: bool, listing: &Result<Vec<String>, String>, distro: &str) -> DistroState {
    match listing {
        Err(reason) => DistroState::Unavailable(reason.clone()),
        Ok(running) if manual || running.iter().any(|name| name.eq_ignore_ascii_case(distro)) => DistroState::Available,
        Ok(_) => DistroState::Stopped,
    }
}

// --- Whether the distro has what a run needs ---

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCheck {
    /// In `REQUIRED_TOOLS` order.
    pub missing: Vec<String>,
    pub gh_logged_in: bool,
}

const TOOLS_SCRIPT: &str = r#"for t in git make claude gh; do printf '%s=%s\n' "$t" "$(command -v "$t")"; done; if command -v gh >/dev/null 2>&1; then gh auth status >/dev/null 2>&1; printf 'gh-auth=%s\n' "$?"; fi"#;

/// Reads what `TOOLS_SCRIPT` printed. A copy under `/mnt/` is the Windows one, reached through the
/// PATH that WSL appends to, and it can't work on Linux paths, so it doesn't count.
fn parse_tool_check(output: &str) -> Result<ToolCheck, String> {
    let lookup = |key: &str| {
        output.lines().find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('=').map(str::to_string))
    };
    if lookup("git").is_none() {
        let reason = output.trim();
        return Err(if reason.is_empty() { "no output from the distro".to_string() } else { reason.to_string() });
    }
    let missing = REQUIRED_TOOLS
        .iter()
        .filter(|tool| lookup(**tool).is_none_or(|found| found.is_empty() || found.starts_with("/mnt/")))
        .map(|tool| (*tool).to_string())
        .collect();
    Ok(ToolCheck { missing, gh_logged_in: lookup("gh-auth").as_deref() == Some("0") })
}

impl ToolCheck {
    /// What stands in the way, if anything. `gh` only matters to a `full` run.
    pub fn problem(&self, distro: &str, full: bool) -> Option<String> {
        let missing: Vec<&str> = self.missing.iter().map(String::as_str).filter(|tool| full || *tool != "gh").collect();
        let mut problems = Vec::new();
        if !missing.is_empty() {
            problems.push(format!(
                "{distro} is missing {}. Install them inside the distro, on your login shell's PATH.",
                missing.join(", ")
            ));
        }
        if full && !self.gh_logged_in && !missing.contains(&"gh") {
            problems.push(format!("The GitHub CLI isn't logged in inside {distro}. Run `gh auth login` there."));
        }
        (!problems.is_empty()).then(|| problems.join(" "))
    }
}

/// Can start the distro.
pub fn check_tools(distro: &str) -> Result<ToolCheck, String> {
    let mut cmd = command("sh");
    cmd.args(["-c", TOOLS_SCRIPT]).env("GH_PROMPT_DISABLED", "1");
    let target = WslPath { distro: distro.to_string(), linux: "/".to_string() };
    let routed = translate(&cmd, &target, &env_for(distro).path);
    let output = proc::output_timeout(routed, TOOLS_TIMEOUT).map_err(|error| format!("could not run {WSL_EXE}: {error}"))?;
    parse_tool_check(&clean_text(&output.stdout))
}

/// The check at the start of a run, which fails it before any agent time is spent.
pub fn require_tools(target: &WslPath, full: bool) -> Result<(), String> {
    if target.is_on_windows_drive() {
        return Err(WINDOWS_DRIVE_NOTICE.to_string());
    }
    let check = check_tools(&target.distro).map_err(|error| could_not_check(&target.distro, &error))?;
    match check.problem(&target.distro, full) {
        Some(problem) => Err(problem),
        None => Ok(()),
    }
}

pub fn could_not_check(distro: &str, error: &str) -> String {
    format!("Could not check the tools in {distro}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wsl(distro: &str, linux: &str) -> Option<WslPath> {
        Some(WslPath { distro: distro.to_string(), linux: linux.to_string() })
    }

    fn parsed(path: &str) -> Option<WslPath> {
        parse(Path::new(path))
    }

    fn strings(args: Vec<&OsStr>) -> Vec<String> {
        args.into_iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn both_hosts_and_the_verbatim_form_are_recognised_in_any_case() {
        let expected = wsl("Ubuntu", "/home/jo/repo");
        assert_eq!(parsed(r"\\wsl.localhost\Ubuntu\home\jo\repo"), expected);
        assert_eq!(parsed(r"\\wsl$\Ubuntu\home\jo\repo"), expected);
        assert_eq!(parsed(r"\\WSL.LocalHost\Ubuntu\home\jo\repo"), expected);
        assert_eq!(parsed(r"\\WSL$\Ubuntu\home\jo\repo"), expected);
        assert_eq!(parsed(r"\\?\UNC\wsl.localhost\Ubuntu\home\jo\repo"), expected);
        assert_eq!(parsed(r"\\?\unc\WSL$\Ubuntu\home\jo\repo"), expected);
    }

    #[test]
    fn distro_names_and_paths_keep_their_dots_hyphens_and_spaces() {
        assert_eq!(parsed(r"\\wsl.localhost\Ubuntu-22.04\home\jo"), wsl("Ubuntu-22.04", "/home/jo"));
        assert_eq!(parsed(r"\\wsl$\my.distro-x\srv\my repo\a b"), wsl("my.distro-x", "/srv/my repo/a b"));
    }

    #[test]
    fn a_distro_root_and_untidy_paths_come_out_clean() {
        assert_eq!(parsed(r"\\wsl.localhost\Ubuntu"), wsl("Ubuntu", "/"));
        assert_eq!(parsed(r"\\wsl.localhost\Ubuntu\"), wsl("Ubuntu", "/"));
        assert_eq!(parsed(r"\\wsl.localhost\Ubuntu\home\jo\"), wsl("Ubuntu", "/home/jo"));
        assert_eq!(parsed(r"\\wsl.localhost\Ubuntu\home/jo\.\x\..\y"), wsl("Ubuntu", "/home/jo/y"));
    }

    #[test]
    fn other_paths_are_not_wsl_paths() {
        for path in [
            r"C:\Users\jo\repo",
            r"\\server\share\repo",
            r"\\?\C:\repo",
            r"\\?\UNC\server\share\repo",
            r"\\wsl.localhost",
            r"\\wsl.localhost\",
            r"\\wsl.localhostish\Ubuntu\repo",
            "/home/jo/repo",
            "//wsl.localhost/Ubuntu/repo",
            "relative",
            "",
        ] {
            assert_eq!(parsed(path), None, "{path}");
        }
    }

    #[test]
    fn the_reverse_is_always_under_the_one_host() {
        assert_eq!(to_unc("Ubuntu", "/home/jo/a b"), PathBuf::from(r"\\wsl.localhost\Ubuntu\home\jo\a b"));
        assert_eq!(to_unc("Ubuntu", "/"), PathBuf::from(r"\\wsl.localhost\Ubuntu\"));
        for path in [r"\\wsl$\Ubuntu-22.04\home\jo\a b", r"\\?\UNC\wsl.localhost\Ubuntu\srv"] {
            let original = parsed(path).unwrap();
            assert_eq!(parse(&to_unc(&original.distro, &original.linux)).unwrap(), original);
        }
    }

    #[test]
    fn normalising_stops_the_same_repo_being_added_under_another_host() {
        let canonical = PathBuf::from(r"\\wsl.localhost\Ubuntu\home\jo\repo");
        assert_eq!(normalise(Path::new(r"\\wsl$\Ubuntu\home\jo\repo")), canonical);
        assert_eq!(normalise(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\jo\repo\")), canonical);
        assert_eq!(normalise(Path::new(r"C:\repo")), PathBuf::from(r"C:\repo"));

        assert!(same_path(Path::new(r"\\wsl$\Ubuntu\home\jo\repo"), &canonical));
        assert!(same_path(Path::new(r"\\wsl$\UBUNTU\home\jo\repo"), &canonical));
        assert!(!same_path(Path::new(r"\\wsl$\Debian\home\jo\repo"), &canonical));
        assert!(!same_path(Path::new(r"\\wsl$\Ubuntu\home\jo\REPO"), &canonical), "Linux paths are case-sensitive");
        assert!(same_path(Path::new(r"C:\a"), Path::new(r"C:\a")));
        assert!(!same_path(Path::new(r"C:\a"), &canonical));
    }

    #[test]
    fn repos_on_a_windows_drive_are_rejected() {
        assert!(is_on_windows_drive(Path::new(r"\\wsl.localhost\Ubuntu\mnt\c\repo")));
        assert!(is_on_windows_drive(Path::new(r"\\wsl$\Ubuntu\mnt")));
        assert!(is_on_windows_drive(Path::new(r"\\wsl$\Ubuntu\home\..\mnt\c")), "dots can't hide it");
        assert!(!is_on_windows_drive(Path::new(r"\\wsl$\Ubuntu\mnt2\c")));
        assert!(!is_on_windows_drive(Path::new(r"\\wsl$\Ubuntu\home\mnt\c")));
        assert!(!is_on_windows_drive(Path::new(r"C:\mnt\c")));
    }

    #[test]
    fn worktrees_and_the_cargo_cache_live_under_the_distro_home() {
        assert_eq!(worktree_dir("/home/jo", "demo", "2026-1"), "/home/jo/.local/share/factory/worktrees/demo/2026-1");
        assert_eq!(cargo_target_dir("/home/jo/", "demo"), "/home/jo/.local/share/factory/target/demo");

        let unc = to_unc("Ubuntu", &worktree_dir("/home/jo", "demo", "2026-1"));
        assert_eq!(unc, PathBuf::from(r"\\wsl.localhost\Ubuntu\home\jo\.local\share\factory\worktrees\demo\2026-1"));
        assert_eq!(tool_path(&unc), "/home/jo/.local/share/factory/worktrees/demo/2026-1");
        assert_eq!(tool_path(Path::new(r"C:\wt\1")), r"C:\wt\1", "an old record's path is passed on as it was");
    }

    #[test]
    fn a_tool_is_named_in_a_distro_and_located_elsewhere() {
        let located = Path::new(r"C:\Tools\gh.exe");
        assert_eq!(program(Path::new(r"\\wsl$\Ubuntu\repo"), "gh", located), PathBuf::from("gh"));
        assert_eq!(program(Path::new(r"C:\repo"), "gh", located), located);
    }

    fn sample_command() -> Command {
        let mut cmd = command("claude");
        cmd.current_dir(r"\\wsl.localhost\Ubuntu\home\jo\my repo")
            .args(["-p", "--tools", "", "--json-schema", r#"{"type":"object","a b":["c"]}"#, "many words here"])
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "url.factory-push-blocked://agent/.pushInsteadOf")
            .env("GIT_CONFIG_VALUE_0", "https://")
            .env("EMPTY", "")
            .env_remove("CLAUDECODE");
        cmd
    }

    #[test]
    fn a_command_becomes_wsl_exe_with_its_distro_directory_env_and_arguments_intact() {
        let cmd = sample_command();
        let target = parse(cmd.get_current_dir().unwrap()).unwrap();
        let routed = translate(&cmd, &target, "/usr/bin:/a b");

        assert_eq!(routed.get_program(), "wsl.exe");
        assert_eq!(routed.get_current_dir(), None, "a UNC directory would break wsl.exe itself");
        let envs: Vec<_> = routed.get_envs().collect();
        assert_eq!(envs, [(OsStr::new("WSL_UTF8"), Some(OsStr::new("1")))]);

        let args = strings(routed.get_args().collect());
        let expected_start = ["-d", "Ubuntu", "--cd", "/home/jo/my repo", "--exec", "env", "PATH=/usr/bin:/a b"];
        assert_eq!(args[..7], expected_start);
        for pair in [
            "GIT_CONFIG_COUNT=1",
            "GIT_CONFIG_KEY_0=url.factory-push-blocked://agent/.pushInsteadOf",
            "GIT_CONFIG_VALUE_0=https://",
            "EMPTY=",
        ] {
            assert!(args[7..11].contains(&pair.to_string()), "{pair} in {args:?}");
        }
        assert_eq!(
            args[11..],
            ["claude", "-p", "--tools", "", "--json-schema", r#"{"type":"object","a b":["c"]}"#, "many words here"]
        );
        assert!(!args.iter().any(|arg| arg.contains("CLAUDECODE")));
    }

    #[test]
    fn a_native_command_is_not_rerouted() {
        let mut cmd = command("git");
        cmd.current_dir("some-dir").arg("status").env("GIT_TERMINAL_PROMPT", "0");
        let routed = route(cmd);
        assert_eq!(routed.get_program(), "git");
        assert_eq!(routed.get_current_dir(), Some(Path::new("some-dir")));
        assert_eq!(strings(routed.get_args().collect()), ["status"]);

        let (session_cmd, session) = into_session(routed);
        assert!(session.is_none());
        assert_eq!(session_cmd.get_program(), "git");
        assert_eq!(session_cmd.get_current_dir(), Some(Path::new("some-dir")));
    }

    #[test]
    fn route_uses_the_distros_cached_path() {
        seed_env("RouteDistro", "/seeded/bin", "/home/jo");
        let mut cmd = command("git");
        cmd.current_dir(r"\\wsl$\routedistro\home\jo").arg("status");
        let args = strings(route(cmd).get_args().collect());
        assert!(args.contains(&"PATH=/seeded/bin".to_string()), "{args:?}");
    }

    #[test]
    fn the_session_wrapper_goes_after_the_env_and_keeps_every_argument() {
        let cmd = sample_command();
        let target = parse(cmd.get_current_dir().unwrap()).unwrap();
        let routed = translate(&cmd, &target, "/usr/bin");
        let original = strings(routed.get_args().collect());

        let (wrapped, session) = into_session(routed);
        let session = session.unwrap();
        assert_eq!(session.distro, "Ubuntu");
        assert!(session.pidfile.starts_with("/tmp/factory-") && session.pidfile.ends_with(".pid"));
        assert_eq!(wrapped.get_program(), "wsl.exe");
        assert_eq!(wrapped.get_envs().collect::<Vec<_>>(), [(OsStr::new("WSL_UTF8"), Some(OsStr::new("1")))]);

        let args = strings(wrapped.get_args().collect());
        let at = args.iter().position(|arg| arg == "setsid").unwrap();
        assert_eq!(args[..at], original[..at]);
        assert_eq!(at, 11, "after every env assignment");
        assert_eq!(args[at..at + 5], ["setsid", "--wait", "sh", "-c", SESSION_SCRIPT]);
        assert_eq!(args[at + 5], session.pidfile);
        assert_eq!(args[at + 6..], original[at..], "the program and arguments are untouched");
    }

    #[test]
    fn a_command_without_env_can_still_be_wrapped_and_each_gets_its_own_file() {
        let (first, session) = into_session(exec_in("Ubuntu", "/bin/bash", ["-l", "-i", "-c", "x=1"]));
        let args = strings(first.get_args().collect());
        let at = args.iter().position(|arg| arg == "--exec").unwrap();
        assert_eq!(args[at + 1], "setsid");
        assert_eq!(args[args.len() - 5..], ["/bin/bash", "-l", "-i", "-c", "x=1"]);
        let (_, other) = into_session(exec_in("Ubuntu", "true", Vec::<&str>::new()));
        assert_ne!(session.unwrap().pidfile, other.unwrap().pidfile);
    }

    #[test]
    fn a_wsl_exe_call_with_nothing_to_exec_gets_no_session() {
        let mut cmd = command(WSL_EXE);
        cmd.args(["--list", "--running", "--quiet"]);
        let (cmd, session) = into_session(cmd);
        assert!(session.is_none());
        assert_eq!(strings(cmd.get_args().collect()), ["--list", "--running", "--quiet"]);
    }

    #[test]
    fn the_scripts_stay_on_one_line_so_they_depend_on_as_little_as_possible() {
        assert!(!SESSION_SCRIPT.contains('\n') && !KILL_SCRIPT.contains('\n') && !TOOLS_SCRIPT.contains('\n'));
    }

    fn utf16(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    }

    #[test]
    fn running_distros_are_read_through_utf16_and_nul_noise() {
        assert_eq!(parse_running(true, b"Ubuntu\nDebian\n", b"").unwrap(), ["Ubuntu", "Debian"]);
        assert_eq!(parse_running(true, &utf16("Ubuntu-22.04\r\nDebian\r\n"), b"").unwrap(), ["Ubuntu-22.04", "Debian"]);
        assert_eq!(parse_running(true, b"U\0b\0u\0n\0t\0u\0\r\0\n\0", b"").unwrap(), ["Ubuntu"]);
    }

    #[test]
    fn nothing_running_is_an_empty_list_whatever_the_exit_code() {
        assert!(parse_running(true, b"", b"").unwrap().is_empty());
        assert!(parse_running(true, b"There are no running distributions.\r\n", b"").unwrap().is_empty());
        assert!(parse_running(false, b"There are no running distributions.\r\n", b"").unwrap().is_empty());
        assert!(parse_running(false, &utf16("There are no running distributions.\r\n"), b"").unwrap().is_empty());
        assert!(parse_running(false, b"", b"").unwrap().is_empty());
    }

    #[test]
    fn wsl_being_unavailable_is_its_own_case() {
        let message = b"Windows Subsystem for Linux has no installed distributions.\r\nError code: Wsl/WSL_E_DEFAULT_DISTRO_NOT_FOUND\r\n";
        let error = parse_running(false, message, b"").unwrap_err();
        assert_eq!(error, "Windows Subsystem for Linux has no installed distributions.");
        assert!(parse_running(false, &utf16("The Windows Subsystem for Linux is not installed.\r\n"), b"").is_err());
        assert!(parse_running(false, b"", b"Error code: Wsl/Service/E_UNEXPECTED").is_err());
    }

    #[test]
    fn an_automatic_refresh_skips_a_stopped_distro_and_a_manual_one_does_not() {
        let listing = Ok(vec!["Ubuntu".to_string()]);
        assert_eq!(refresh_decision(false, &listing, "Ubuntu"), DistroState::Available);
        assert_eq!(refresh_decision(false, &listing, "ubuntu"), DistroState::Available);
        assert_eq!(refresh_decision(false, &listing, "Debian"), DistroState::Stopped);
        assert_eq!(refresh_decision(true, &listing, "Debian"), DistroState::Available);
        assert_eq!(refresh_decision(false, &Ok(Vec::new()), "Debian"), DistroState::Stopped);
    }

    #[test]
    fn with_wsl_unavailable_no_refresh_is_attempted() {
        let listing = Err("not installed".to_string());
        let unavailable = DistroState::Unavailable("not installed".to_string());
        assert_eq!(refresh_decision(false, &listing, "Ubuntu"), unavailable);
        assert_eq!(refresh_decision(true, &listing, "Ubuntu"), unavailable);
    }

    #[test]
    fn distro_labels_name_what_is_wrong() {
        assert_eq!(DistroState::Available.label("Ubuntu"), None);
        assert_eq!(DistroState::Stopped.label("Ubuntu").as_deref(), Some("Ubuntu is stopped"));
        let label = DistroState::Unavailable("not installed".to_string()).label("Ubuntu").unwrap();
        assert_eq!(label, "WSL isn't available: not installed");
    }

    #[test]
    fn tools_are_found_or_missing_and_a_windows_copy_does_not_count() {
        let all = "git=/usr/bin/git\nmake=/usr/bin/make\nclaude=/home/jo/.local/bin/claude\ngh=/usr/bin/gh\ngh-auth=0\n";
        let check = parse_tool_check(all).unwrap();
        assert_eq!(check, ToolCheck { missing: Vec::new(), gh_logged_in: true });
        assert_eq!(check.problem("Ubuntu", true), None);

        let partial = "git=/usr/bin/git\nmake=\nclaude=/mnt/c/Users/jo/AppData/Roaming/npm/claude\ngh=\n";
        let check = parse_tool_check(partial).unwrap();
        assert_eq!(check.missing, ["make", "claude", "gh"]);
        assert!(!check.gh_logged_in);
    }

    #[test]
    fn a_failed_check_is_not_reported_as_every_tool_missing() {
        assert!(parse_tool_check("").is_err());
        let error = parse_tool_check("There is no distribution with the supplied name.").unwrap_err();
        assert_eq!(error, "There is no distribution with the supplied name.");
    }

    #[test]
    fn the_blocker_names_the_distro_and_what_is_missing() {
        let check = ToolCheck { missing: vec!["make".into(), "gh".into()], gh_logged_in: false };
        assert_eq!(
            check.problem("Ubuntu", true).unwrap(),
            "Ubuntu is missing make, gh. Install them inside the distro, on your login shell's PATH."
        );

        let logged_out = ToolCheck { missing: Vec::new(), gh_logged_in: false };
        assert_eq!(
            logged_out.problem("Ubuntu", true).unwrap(),
            "The GitHub CLI isn't logged in inside Ubuntu. Run `gh auth login` there."
        );

        let both = ToolCheck { missing: vec!["claude".into()], gh_logged_in: false };
        let text = both.problem("Ubuntu", true).unwrap();
        assert!(text.contains("missing claude") && text.contains("isn't logged in"), "{text}");
    }

    #[test]
    fn gh_only_matters_to_a_full_run() {
        let no_gh = ToolCheck { missing: vec!["gh".into()], gh_logged_in: false };
        assert_eq!(no_gh.problem("Ubuntu", false), None);
        assert!(no_gh.problem("Ubuntu", true).is_some());
        let logged_out = ToolCheck::default();
        assert_eq!(logged_out.problem("Ubuntu", false), None);
        let no_make = ToolCheck { missing: vec!["make".into(), "gh".into()], gh_logged_in: false };
        assert_eq!(
            no_make.problem("Ubuntu", false).unwrap(),
            "Ubuntu is missing make. Install them inside the distro, on your login shell's PATH."
        );
    }

    #[test]
    fn a_project_on_a_windows_drive_fails_its_run_without_touching_the_distro() {
        let target = WslPath { distro: "Ubuntu".into(), linux: "/mnt/c/repo".into() };
        assert_eq!(require_tools(&target, true).unwrap_err(), WINDOWS_DRIVE_NOTICE);
    }

    #[test]
    fn identity_comes_from_the_password_database_with_fallbacks() {
        let full = parse_identity("/usr/bin:/bin\n/home/env\njo:x:1000:1000:Jo:/home/jo:/usr/bin/zsh\n");
        assert_eq!(
            full,
            Identity { inherited_path: "/usr/bin:/bin".into(), home: Some("/home/jo".into()), shell: "/usr/bin/zsh".into() }
        );

        let no_getent = parse_identity("/usr/bin\n/home/env\n");
        assert_eq!(no_getent.home.as_deref(), Some("/home/env"));
        assert_eq!(no_getent.shell, "/bin/sh");

        let empty_shell = parse_identity("/usr/bin\n/home/env\njo:x:1000:1000::/home/jo:\n");
        assert_eq!(empty_shell.shell, "/bin/sh");

        assert_eq!(parse_identity("").home, None);
    }

    #[test]
    fn stray_nul_bytes_and_byte_order_marks_are_stripped_from_wsl_text() {
        assert_eq!(clean_text(&utf16("hi there")), "hi there");
        assert_eq!(clean_text("\u{feff}plain".as_bytes()), "plain");
    }

    #[test]
    fn a_routed_failure_falls_back_to_stdout_where_wsl_exe_prints_its_errors() {
        assert_eq!(failure_detail(true, b"out", b"  err\n"), "err");
        assert_eq!(failure_detail(true, &utf16("no such distro\r\n"), b""), "no such distro");
        assert_eq!(failure_detail(true, b"", b"e\0r\0r\0"), "err");
        assert_eq!(failure_detail(false, b"out", b""), "", "native commands only ever report stderr");
        assert_eq!(failure_detail(false, b"out", b" err \n"), "err");
    }

    /// These need a real distro, named by FACTORY_TEST_WSL_DISTRO, and do nothing without one.
    #[cfg(windows)]
    mod real {
        use std::process::Stdio;
        use std::thread;

        use super::*;
        use crate::git::{git, git_cmd};
        use crate::proc::{Cancel, Exit, run_streaming};

        fn distro() -> Option<String> {
            std::env::var("FACTORY_TEST_WSL_DISTRO").ok().filter(|name| !name.is_empty())
        }

        /// Makes a new temp directory in the distro, and returns its UNC and Linux paths.
        fn temp_dir(distro: &str) -> (PathBuf, String) {
            let output = exec_in(distro, "mktemp", ["-d"]).stdin(Stdio::null()).output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let linux = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (to_unc(distro, &linux), linux)
        }

        fn sh_in(dir: &Path, script: &str, args: &[&str]) -> Command {
            let mut cmd = command("sh");
            cmd.current_dir(dir).args(["-c", script, "sh"]).args(args);
            cmd
        }

        fn remove(distro: &str, linux: &str) {
            let _ = exec_in(distro, "rm", ["-rf", linux]).stdin(Stdio::null()).output();
        }

        const TRICKY_ARGS: [&str; 8] = [
            r#"{"type":"object","properties":{"a b":{"enum":["x","y\"z"]}},"required":["a b"]}"#,
            "",
            "several words in one",
            "two\nlines",
            "$HOME `id` $(id) * ~ ; & |",
            r#"back\slash\\ and \" quote"#,
            "trailing backslash\\",
            "'single' \"double\"",
        ];

        /// Exactly what the Linux process saw, NUL-separated so no argument can blur into another.
        fn seen_by_linux(routed: Command) -> Vec<String> {
            let mut cmd = routed;
            let output = cmd.stdin(Stdio::null()).output().unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout).split_terminator('\0').map(str::to_string).collect()
        }

        #[test]
        #[ignore]
        fn arguments_and_env_arrive_intact() {
            let Some(distro) = distro() else { return };
            let (dir, linux) = temp_dir(&distro);
            let script = r#"printf '%s\0' "$PWD" "$FACTORY_ONE" "$GIT_CONFIG_VALUE_0" "$EMPTY_ONE" "$#"; for a in "$@"; do printf '%s\0' "$a"; done"#;
            let mut cmd = sh_in(&dir, script, &TRICKY_ARGS);
            cmd.env("FACTORY_ONE", "a b \"c\"")
                .env("GIT_CONFIG_VALUE_0", "https://")
                .env("EMPTY_ONE", "");
            let seen = seen_by_linux(route(cmd));
            assert_eq!(seen[0], linux, "--cd");
            assert_eq!(seen[1], "a b \"c\"");
            assert_eq!(seen[2], "https://");
            assert_eq!(seen[3], "");
            assert_eq!(seen[4], TRICKY_ARGS.len().to_string());
            assert_eq!(seen[5..], TRICKY_ARGS);
            remove(&distro, &linux);
        }

        #[test]
        #[ignore]
        fn the_session_wrapper_keeps_arguments_exit_codes_and_streams_intact() {
            let Some(distro) = distro() else { return };
            let (dir, linux) = temp_dir(&distro);
            let script = r#"for a in "$@"; do printf '[%s]\n' "$a"; done; echo to-stderr >&2; exit 7"#;
            let mut lines = Vec::new();
            let exit = run_streaming(route(sh_in(&dir, script, &TRICKY_ARGS)), None, &Cancel::default(), &mut |line| {
                lines.push(line.to_string())
            })
            .unwrap();
            assert_eq!(exit, Exit::Code(7));
            let text = lines.join("\n");
            assert!(lines.iter().any(|line| line == "to-stderr"), "{text}");
            assert!(text.contains(&format!("[{}]", TRICKY_ARGS[0])), "{text}");
            assert!(lines.iter().any(|line| line == "[]"), "the empty argument: {text}");
            assert!(text.contains("[two\nlines]"), "{text}");
            remove(&distro, &linux);
        }

        #[test]
        #[ignore]
        fn stdin_reaches_the_program() {
            let Some(distro) = distro() else { return };
            let (dir, linux) = temp_dir(&distro);
            let prompt = "héllo wörld\nsecond line \u{1F600}\n".to_string();
            let mut lines = Vec::new();
            let exit =
                run_streaming(route(sh_in(&dir, "cat", &[])), Some(prompt), &Cancel::default(), &mut |line| lines.push(line.to_string()))
                    .unwrap();
            assert_eq!(exit, Exit::Code(0));
            assert_eq!(lines, ["héllo wörld", "second line \u{1F600}"]);
            remove(&distro, &linux);
        }

        #[test]
        #[ignore]
        fn the_login_shell_path_and_home_are_read() {
            let Some(distro) = distro() else { return };
            let env = refresh_env(&distro);
            assert!(env.path.split(':').any(|dir| dir == "/usr/bin"), "{}", env.path);
            assert!(env.path.split(':').any(|dir| dir.ends_with("/.local/bin")), "{}", env.path);
            assert!(env.home.as_deref().is_some_and(|home| home.starts_with('/')), "{:?}", env.home);
        }

        #[test]
        #[ignore]
        fn a_running_distro_is_listed_and_the_tools_check_answers() {
            let Some(distro) = distro() else { return };
            let _ = refresh_env(&distro);
            let running = list_running().unwrap();
            assert!(running.iter().any(|name| name.eq_ignore_ascii_case(&distro)), "{running:?}");
            check_tools(&distro).unwrap();
        }

        #[test]
        #[ignore]
        fn cancel_kills_a_grandchild_inside_the_distro_and_removes_its_temp_file() {
            let Some(distro) = distro() else { return };
            let (dir, linux) = temp_dir(&distro);
            let cancel = Cancel::default();
            let trigger = cancel.clone();
            let mut grandchild = String::new();
            let script = "sleep 300 & echo $!; wait";
            let exit = run_streaming(route(sh_in(&dir, script, &[])), None, &cancel, &mut |line| {
                grandchild = line.to_string();
                trigger.cancel();
            })
            .unwrap();
            assert_eq!(exit, Exit::Cancelled);
            assert!(grandchild.parse::<u32>().is_ok(), "{grandchild}");

            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let alive = exec_in(&distro, "kill", ["-0", grandchild.as_str()])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                if !alive.success() {
                    break;
                }
                assert!(Instant::now() < deadline, "the grandchild {grandchild} survived the cancel");
                thread::sleep(Duration::from_millis(200));
            }
            remove(&distro, &linux);
        }

        #[test]
        #[ignore]
        fn a_worktree_can_be_added_and_removed_on_the_linux_side() {
            let Some(distro) = distro() else { return };
            let (dir, linux) = temp_dir(&distro);
            let run = |args: &[&str]| git(&dir, args).unwrap_or_else(|error| panic!("{error}"));
            run(&["init", "-b", "main"]);
            run(&["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "--allow-empty", "-m", "first"]);

            let linux_worktree = format!("{linux}/wt with space");
            let unc_worktree = to_unc(&distro, &linux_worktree);
            run(&["worktree", "add", "--detach", &linux_worktree, "HEAD"]);
            assert!(unc_worktree.join(".git").exists(), "Windows sees it through the UNC path");
            assert_eq!(git(&unc_worktree, &["rev-parse", "HEAD"]).unwrap(), run(&["rev-parse", "HEAD"]));
            let log = git(&unc_worktree, &["log", "-1", "--format=%s%x00%cr"]).unwrap();
            assert!(log.contains('\0'), "a NUL a tool printed on purpose survives: {log:?}");
            let streamed = run_streaming(git_cmd(&unc_worktree, &["status"]), None, &Cancel::default(), &mut |_| {}).unwrap();
            assert_eq!(streamed, Exit::Code(0));

            run(&["worktree", "remove", "--force", &tool_path(&unc_worktree)]);
            assert!(!unc_worktree.exists());
            remove(&distro, &linux);
        }
    }
}
