//! GitHub, through the `gh` CLI: watching release workflows and publishing releases.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use serde::Deserialize;

use crate::proc::command;

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct WorkflowRun {
    pub status: String,
    #[serde(default)]
    pub conclusion: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub is_draft: bool,
    #[serde(default)]
    pub url: String,
}

pub trait Forge: Send + Sync {
    /// Fails with a readable reason if `gh` is missing or not logged in.
    fn ready(&self, dir: &Path) -> Result<(), String>;
    fn workflow_run(&self, dir: &Path, workflow: &str, sha: &str) -> Result<Option<WorkflowRun>, String>;
    fn release(&self, dir: &Path, tag: &str) -> Result<Option<Release>, String>;
    fn create_draft(&self, dir: &Path, tag: &str, title: &str, sha: &str, notes: &str) -> Result<(), String>;
    fn set_notes(&self, dir: &Path, tag: &str, notes: &str) -> Result<(), String>;
    fn publish(&self, dir: &Path, tag: &str) -> Result<(), String>;
}

pub struct GhCli {
    pub program: PathBuf,
}

/// Finds `gh` on PATH or, since a fresh install doesn't reach a running app's PATH, where it installs.
pub fn locate_gh() -> PathBuf {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .flat_map(|dir| [dir.join("gh.exe"), dir.join("gh")])
            .find(|candidate| candidate.is_file())
    });
    let installed = PathBuf::from(r"C:\Program Files\GitHub CLI\gh.exe");
    on_path.unwrap_or_else(|| if installed.is_file() { installed } else { PathBuf::from("gh") })
}

impl GhCli {
    fn cmd(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = command(&self.program);
        cmd.current_dir(dir).args(args).env("GH_PROMPT_DISABLED", "1");
        cmd
    }

    fn run(&self, dir: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
        let mut cmd = self.cmd(dir, args);
        cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("could not run gh: {e}"))?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            let text = text.to_string();
            thread::spawn(move || {
                let _ = stdin.write_all(text.as_bytes());
            });
        }
        let output = child.wait_with_output().map_err(|e| format!("could not run gh: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("gh {}: {}", args.join(" "), stderr.trim()))
        }
    }
}

impl Forge for GhCli {
    fn ready(&self, dir: &Path) -> Result<(), String> {
        self.run(dir, &["auth", "status"], None).map(|_| ())
    }

    fn workflow_run(&self, dir: &Path, workflow: &str, sha: &str) -> Result<Option<WorkflowRun>, String> {
        let args = [
            "run", "list", "--workflow", workflow, "--commit", sha, "--limit", "1", "--json", "status,conclusion,url",
        ];
        parse_runs(&self.run(dir, &args, None)?)
    }

    fn release(&self, dir: &Path, tag: &str) -> Result<Option<Release>, String> {
        match self.run(dir, &["release", "view", tag, "--json", "isDraft,url"], None) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| e.to_string()),
            Err(error) if error.contains("release not found") => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn create_draft(&self, dir: &Path, tag: &str, title: &str, sha: &str, notes: &str) -> Result<(), String> {
        let args = ["release", "create", tag, "--draft", "--target", sha, "--title", title, "--notes-file", "-"];
        self.run(dir, &args, Some(notes)).map(|_| ())
    }

    fn set_notes(&self, dir: &Path, tag: &str, notes: &str) -> Result<(), String> {
        self.run(dir, &["release", "edit", tag, "--notes-file", "-"], Some(notes)).map(|_| ())
    }

    fn publish(&self, dir: &Path, tag: &str) -> Result<(), String> {
        self.run(dir, &["release", "edit", tag, "--draft=false"], None).map(|_| ())
    }
}

fn parse_runs(text: &str) -> Result<Option<WorkflowRun>, String> {
    let runs: Vec<WorkflowRun> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(runs.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workflow_runs() {
        assert_eq!(parse_runs("[]").unwrap(), None);
        let run = parse_runs(r#"[{"status":"completed","conclusion":"success","url":"u"}]"#).unwrap().unwrap();
        assert_eq!((run.status.as_str(), run.conclusion.as_str()), ("completed", "success"));
        let queued = parse_runs(r#"[{"status":"queued","conclusion":"","url":"u"}]"#).unwrap().unwrap();
        assert_eq!(queued.conclusion, "");
    }

    #[test]
    fn parses_release_json() {
        let release: Release = serde_json::from_str(r#"{"isDraft":true,"url":"u"}"#).unwrap();
        assert!(release.is_draft);
    }
}
