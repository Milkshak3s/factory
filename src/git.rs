//! Thin wrapper over the git CLI, plus the read-only status shown in the project list.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::proc::command;
use crate::wsl;

pub fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = command("git");
    cmd.current_dir(dir).args(args).env("GIT_TERMINAL_PROMPT", "0");
    wsl::route(cmd)
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_cmd(dir, args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
    } else {
        let detail = wsl::failure_detail(wsl::target(dir).is_some(), &output.stdout, &output.stderr);
        Err(format!("git {}: {detail}", args.join(" ")))
    }
}

pub fn is_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|s| s == "true")
}

pub fn origin_url(dir: &Path) -> Option<String> {
    git(dir, &["remote", "get-url", "origin"]).ok()
}

pub fn default_branch(dir: &Path) -> String {
    if let Ok(head) = git(dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = head.strip_prefix("origin/") {
            return name.to_string();
        }
    }
    git(dir, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|_| "main".to_string())
}

#[derive(Clone, Debug, Default)]
pub struct RepoStatus {
    pub branch: String,
    pub dirty: usize,
    pub ahead: u32,
    pub behind: u32,
    pub last_commit: String,
    pub last_commit_age: String,
    pub latest_tag: Option<String>,
    pub has_origin: bool,
    /// Whether the branch runs start from has a Makefile, which is what the gate needs.
    pub has_makefile: bool,
    pub error: Option<String>,
}

pub fn status(dir: &Path, default_branch: &str) -> RepoStatus {
    let porcelain = match git(dir, &["status", "--porcelain=v2", "--branch"]) {
        Ok(text) => text,
        Err(error) => return RepoStatus { error: Some(error), ..Default::default() },
    };
    let mut status = parse_porcelain(&porcelain);
    if let Ok(log) = git(dir, &["log", "-1", "--format=%s%x00%cr"]) {
        if let Some((subject, age)) = log.split_once('\0') {
            status.last_commit = subject.to_string();
            status.last_commit_age = age.to_string();
        }
    }
    status.latest_tag = latest_tag(dir);
    status.has_origin = origin_url(dir).is_some();
    status.has_makefile = git(dir, &["cat-file", "-e", &format!("origin/{default_branch}:Makefile")]).is_ok();
    status
}

pub fn latest_tag(dir: &Path) -> Option<String> {
    git(dir, &["tag", "--sort=-creatordate"])
        .ok()?
        .lines()
        .next()
        .map(str::to_string)
}

fn parse_porcelain(text: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    for line in text.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = head.to_string();
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut parts = ab.split_whitespace();
            status.ahead = parts.next().and_then(|p| p.strip_prefix('+')?.parse().ok()).unwrap_or(0);
            status.behind = parts.next().and_then(|p| p.strip_prefix('-')?.parse().ok()).unwrap_or(0);
        } else if !line.starts_with('#') && !line.is_empty() {
            status.dirty += 1;
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_goes_through_wsl_only_for_a_wsl_directory() {
        wsl::seed_env("GitDistro", "/seeded/bin", "/home/jo");
        let cmd = git_cmd(Path::new(r"\\wsl.localhost\GitDistro\home\jo\repo"), &["status"]);
        assert_eq!(cmd.get_program(), "wsl.exe");
        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..4], ["-d", "GitDistro", "--cd", "/home/jo/repo"]);
        assert!(args.contains(&"GIT_TERMINAL_PROMPT=0".to_string()));
        assert_eq!(args[args.len() - 2..], ["git", "status"]);

        let native = git_cmd(Path::new("."), &["status"]);
        assert_eq!(native.get_program(), "git");
        assert_eq!(native.get_current_dir(), Some(Path::new(".")));
    }

    #[test]
    fn parses_branch_counts_and_dirty_files() {
        let status = parse_porcelain(
            "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n\
             # branch.ab +2 -3\n1 .M N... 100644 100644 100644 a b src/a.rs\n? new.txt\n",
        );
        assert_eq!(status.branch, "main");
        assert_eq!((status.ahead, status.behind, status.dirty), (2, 3, 2));
    }
}
