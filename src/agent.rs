//! The build step: headless Claude Code, restricted to writing code.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::proc::{Cancel, Exit, command, run_streaming};

/// Told to the agent up front. The pipeline verifies what it can rather than trusting this.
const RULES: &str = "You are the build step of a software factory. Implement the task by editing \
files in the current directory, then stop. After you stop, the factory itself runs the test \
suite, commits, pushes and cuts releases. Do not run git commit, push, tag, merge, rebase, reset, \
checkout or switch, and do not change branches. Do not delete, skip or weaken tests to make them \
pass. End with a 1-3 sentence summary of the change; it becomes the commit message body.";

const DENIED_TOOLS: &str = "Bash(git commit:*),Bash(git push:*),Bash(git tag:*),Bash(git merge:*),\
Bash(git rebase:*),Bash(git reset:*),Bash(git checkout:*),Bash(git switch:*),\
Bash(git worktree:*),Bash(gh pr:*),Bash(gh release:*)";

pub struct AgentRequest<'a> {
    pub dir: &'a Path,
    pub prompt: String,
    /// Session to continue, so a retry remembers what it already tried.
    pub resume: Option<String>,
    pub model: &'a str,
    pub permission_mode: &'a str,
    pub allowed_tools: &'a str,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct AgentResult {
    pub session_id: Option<String>,
    pub summary: String,
}

pub trait Agent: Send + Sync {
    fn run(
        &self,
        request: AgentRequest,
        cancel: &Cancel,
        log: &mut dyn FnMut(&str),
    ) -> Result<Option<AgentResult>, String>;
}

pub struct ClaudeCli {
    pub program: PathBuf,
}

impl ClaudeCli {
    fn args(request: &AgentRequest) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            request.permission_mode,
            "--append-system-prompt",
            RULES,
            "--disallowedTools",
            DENIED_TOOLS,
        ]
        .map(str::to_string)
        .into();
        if !request.allowed_tools.trim().is_empty() {
            args.extend(["--allowedTools".to_string(), request.allowed_tools.trim().to_string()]);
        }
        if !request.model.trim().is_empty() {
            args.extend(["--model".to_string(), request.model.trim().to_string()]);
        }
        if let Some(session) = &request.resume {
            args.extend(["--resume".to_string(), session.clone()]);
        }
        args
    }
}

impl Agent for ClaudeCli {
    /// Returns `Ok(None)` if cancelled.
    fn run(
        &self,
        request: AgentRequest,
        cancel: &Cancel,
        log: &mut dyn FnMut(&str),
    ) -> Result<Option<AgentResult>, String> {
        let mut cmd = command(&self.program);
        cmd.current_dir(request.dir)
            .args(Self::args(&request))
            .envs(request.env.iter().map(|(k, v)| (k, v)))
            // Set when the factory itself was launched from a Claude Code session.
            .env_remove("CLAUDECODE");

        let mut result = AgentResult::default();
        let mut failure = None;
        let exit = run_streaming(cmd, Some(request.prompt), cancel, &mut |line| {
            match parse_event(line) {
                Some(event) => {
                    if let Some(id) = event.session_id {
                        result.session_id = Some(id);
                    }
                    if let Some(text) = event.log {
                        text.lines().for_each(&mut *log);
                    }
                    if let Some(summary) = event.summary {
                        result.summary = summary;
                    }
                    if event.failure.is_some() {
                        failure = event.failure;
                    }
                }
                None => log(line),
            }
        })
        .map_err(|e| format!("could not start {}: {e}", self.program.display()))?;

        match exit {
            Exit::Cancelled => Ok(None),
            Exit::Code(code) => match failure {
                Some(message) => Err(message),
                None if code != 0 => Err(format!("claude exited with code {code}")),
                None => Ok(Some(result)),
            },
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct Event {
    session_id: Option<String>,
    log: Option<String>,
    summary: Option<String>,
    failure: Option<String>,
}

/// Reads one stream-json line. `None` means it wasn't JSON, so log it as-is.
fn parse_event(line: &str) -> Option<Event> {
    let value: Value = serde_json::from_str(line).ok()?;
    let mut event = Event {
        session_id: value["session_id"].as_str().map(str::to_string),
        ..Default::default()
    };
    match value["type"].as_str() {
        Some("assistant") => {
            let blocks = value["message"]["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let lines: Vec<String> = blocks.iter().filter_map(describe_block).collect();
            if !lines.is_empty() {
                event.log = Some(lines.join("\n"));
            }
        }
        Some("result") => {
            let text = value["result"].as_str().unwrap_or_default().to_string();
            if value["is_error"].as_bool().unwrap_or(false) {
                event.failure = Some(if text.is_empty() { "agent reported an error".into() } else { text });
            } else {
                event.summary = Some(text);
            }
            if let Some(cost) = value["total_cost_usd"].as_f64() {
                event.log = Some(format!("agent finished (${cost:.2})"));
            }
        }
        _ => {}
    }
    Some(event)
}

fn describe_block(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str().map(str::to_string),
        "tool_use" => {
            let input = &block["input"];
            let detail = ["command", "file_path", "path", "pattern"]
                .iter()
                .find_map(|key| input[*key].as_str())
                .unwrap_or_default();
            Some(format!("-> {} {detail}", block["name"].as_str().unwrap_or("tool")))
        }
        _ => None,
    }
}

/// Finds `claude`: the configured path, then PATH, then the desktop app's bundled copy.
pub fn locate_claude(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(path.to_path_buf());
    }
    let names = ["claude.exe", "claude.cmd", "claude"];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let bundled = PathBuf::from(std::env::var_os("APPDATA")?).join("Claude").join("claude-code");
    newest_bundled(&bundled)
}

/// The desktop app keeps copies at `<version>/<hash>/claude.exe`; take the most recent.
fn newest_bundled(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|version| std::fs::read_dir(version.path()).ok())
        .flat_map(|hashes| hashes.flatten())
        .map(|hash| hash.path().join("claude.exe"))
        .filter_map(|exe| {
            let modified = exe.metadata().ok()?.modified().ok()?;
            Some((modified, exe))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, exe)| exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(dir: &Path) -> AgentRequest<'_> {
        AgentRequest {
            dir,
            prompt: String::new(),
            resume: Some("abc".into()),
            model: "haiku",
            permission_mode: "acceptEdits",
            allowed_tools: "",
            env: Vec::new(),
        }
    }

    #[test]
    fn args_always_deny_pipeline_commands_and_resume_when_asked() {
        let args = ClaudeCli::args(&request(Path::new(".")));
        let denied = args.iter().position(|a| a == "--disallowedTools").unwrap();
        assert!(args[denied + 1].contains("Bash(git push:*)"));
        assert!(args[denied + 1].contains("Bash(git commit:*)"));
        assert!(args.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(args.windows(2).any(|w| w == ["--model", "haiku"]));
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn parses_assistant_tool_use_and_text() {
        let event = parse_event(
            r#"{"type":"assistant","session_id":"s1","message":{"content":[
                {"type":"text","text":"Fixing"},
                {"type":"tool_use","name":"Edit","input":{"file_path":"a.rs"}}]}}"#,
        )
        .unwrap();
        assert_eq!(event.session_id.as_deref(), Some("s1"));
        assert_eq!(event.log.as_deref(), Some("Fixing\n-> Edit a.rs"));
    }

    #[test]
    fn parses_success_and_error_results() {
        let ok = parse_event(r#"{"type":"result","is_error":false,"result":"Done","total_cost_usd":0.5}"#).unwrap();
        assert_eq!(ok.summary.as_deref(), Some("Done"));
        assert_eq!(ok.log.as_deref(), Some("agent finished ($0.50)"));
        let bad = parse_event(r#"{"type":"result","is_error":true,"result":"Not logged in"}"#).unwrap();
        assert_eq!(bad.failure.as_deref(), Some("Not logged in"));
    }

    #[test]
    fn non_json_lines_are_passed_through() {
        assert_eq!(parse_event("plain stderr"), None);
    }
}
