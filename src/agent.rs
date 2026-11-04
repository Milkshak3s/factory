//! The agent stages: headless Claude Code, restricted to what each stage needs.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::proc::{Cancel, Exit, command, run_streaming};

/// Told to the agent up front. The pipeline verifies what it can rather than trusting this.
const RULES: &str = "You are the build step of a software factory. Implement the solution or task you are given by \
editing files in the current directory, and add or update tests for it, then stop. After you stop, \
the factory itself runs `make test`, commits, merges, pushes and cuts releases. Do not run git \
commit, push, tag, merge, rebase, reset, checkout or switch, and do not change branches. Do not \
edit the Makefile. Do not delete, skip or weaken tests to make them pass. Your final message becomes \
the commit message body, so make it only 1-3 sentences saying what changed and why, with no preamble.";

const GRILL_RULES: &str = "You are the requirements step of a software factory. Your job is to \
grill the user until the request has narrowed to one specific solution. Nothing has been built \
yet, and a build agent will work from your brief without being able to ask anything. Read the \
code first, and never ask what the code already answers. Then ask pointed questions about the \
behavior, scope, edge cases and tradeoffs that would change what gets built. Any question with \
discrete choices must give 2-4 short, self-contained options in `options`, and set `recommended` to \
the zero-based index of the one you'd pick; the explanation and tradeoffs go in `text`. Leave \
`options` out of open-ended questions. Don't add an 'Other' option, because the user can always type \
their own answer. Always ask at least one round of questions, however clear the \
request looks. Ask at most 5 questions per round, and keep going for as many rounds as it takes. \
Once the user has answered and no question would change the solution, return status \"ready\" with a title \
(an imperative commit subject of under 60 characters), a brief (the specific solution: what \
changes, where, how it behaves, what is out of scope, and how to test it), and a bump: patch for \
fixes, minor for new features, major for breaking changes. Otherwise return status \"questions\" \
with your questions. You can't change any files.";

const GRILL_SCHEMA: &str = r#"{"type":"object","properties":{"status":{"type":"string","enum":["questions","ready"]},"questions":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"options":{"type":"array","items":{"type":"string"}},"recommended":{"type":"integer","minimum":0}},"required":["text"]}},"title":{"type":"string"},"brief":{"type":"string"},"bump":{"type":"string","enum":["patch","minor","major"]}},"required":["status"],"if":{"properties":{"status":{"const":"ready"}}},"then":{"required":["title","brief","bump"]}}"#;

const MAKEFILE_RULES_START: &str = "You are setting up a software factory's test gate for this project. \
Read the project, including any CI workflows and scripts, to learn how it is really built and \
tested. Then write a GNU Makefile in the project root and change no other file. Its `test` target \
is the gate every future change has to pass, so it must run the project's complete test suite, \
exiting non-zero on any failure. Never make it a no-op, and never ignore failures. If CI runs more \
than one test command, run them all. Also add targets for the project's other common tasks, such as \
build and lint, and a short comment at the top listing them.";

const MAKEFILE_SHELL_WINDOWS: &str = "Recipes run on Windows, under either cmd.exe or a POSIX sh, so use \
commands that work in both.";

const MAKEFILE_SHELL_POSIX: &str = "Recipes run under a POSIX sh.";

const MAKEFILE_RULES_END: &str = "If the project has no tests at all, write \
a `test` target that prints that and exits non-zero, and return has_tests false; otherwise return \
has_tests true. The factory runs `make test` after you stop, and commits and pushes the result. You \
can't run commands yourself.";

/// The Makefile agent's rules, describing the shell that recipes run under on this host.
fn makefile_rules(windows: bool) -> String {
    let shell = if windows { MAKEFILE_SHELL_WINDOWS } else { MAKEFILE_SHELL_POSIX };
    format!("{MAKEFILE_RULES_START} {shell} {MAKEFILE_RULES_END}")
}

const MAKEFILE_SCHEMA: &str = r#"{"type":"object","properties":{"has_tests":{"type":"boolean"}},"required":["has_tests"]}"#;

const DENIED_TOOLS: &str ="Bash(git commit:*),Bash(git push:*),Bash(git tag:*),Bash(git merge:*),\
Bash(git rebase:*),Bash(git reset:*),Bash(git checkout:*),Bash(git switch:*),\
Bash(git worktree:*),Bash(gh pr:*),Bash(gh release:*)";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    /// Reads the code and questions the user. Can't change anything.
    Grill,
    /// Writes the code, under the project's permission settings.
    Build,
    /// Reads the code and writes the Makefile. Can't run commands.
    Makefile,
    /// Turns the given text into release notes. Has no tools at all.
    Summarize,
}

pub struct AgentRequest<'a> {
    pub role: Role,
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
    pub structured: Option<Value>,
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
        // --strict-mcp-config: the login also brings claude.ai connectors like Gmail and Drive,
        // and no stage has any business reaching those.
        let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config"]
            .map(str::to_string)
            .into();
        let mut add = |values: &[&str]| args.extend(values.iter().map(|v| v.to_string()));
        match request.role {
            Role::Grill => add(&[
                "--permission-mode",
                "default",
                "--tools",
                "Read,Grep,Glob",
                "--append-system-prompt",
                GRILL_RULES,
                "--json-schema",
                GRILL_SCHEMA,
            ]),
            Role::Build => {
                add(&[
                    "--permission-mode",
                    request.permission_mode,
                    "--append-system-prompt",
                    RULES,
                    "--disallowedTools",
                    DENIED_TOOLS,
                ]);
                if !request.allowed_tools.trim().is_empty() {
                    add(&["--allowedTools", request.allowed_tools.trim()]);
                }
            }
            Role::Makefile => add(&[
                "--permission-mode",
                "acceptEdits",
                "--tools",
                "Read,Grep,Glob,Write,Edit",
                "--append-system-prompt",
                makefile_rules(cfg!(windows)).as_str(),
                "--json-schema",
                MAKEFILE_SCHEMA,
            ]),
            Role::Summarize => add(&["--tools", ""]),
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
                    if event.structured.is_some() {
                        result.structured = event.structured;
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
    structured: Option<Value>,
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
                event.structured = Some(value["structured_output"].clone()).filter(|v| !v.is_null());
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

/// Finds `claude`: the configured path, then PATH, then on Windows the desktop app's bundled copy.
pub fn locate_claude(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in claude_names(cfg!(windows)) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    if !cfg!(windows) {
        return None;
    }
    let bundled = PathBuf::from(std::env::var_os("APPDATA")?).join("Claude").join("claude-code");
    newest_bundled(&bundled)
}

fn claude_names(windows: bool) -> &'static [&'static str] {
    if windows { &["claude.exe", "claude.cmd", "claude"] } else { &["claude"] }
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

    fn request(dir: &Path, role: Role) -> AgentRequest<'_> {
        AgentRequest {
            role,
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
        let args = ClaudeCli::args(&request(Path::new("."), Role::Build));
        let denied = args.iter().position(|a| a == "--disallowedTools").unwrap();
        assert!(args[denied + 1].contains("Bash(git push:*)"));
        assert!(args[denied + 1].contains("Bash(git commit:*)"));
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(args.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(args.windows(2).any(|w| w == ["--model", "haiku"]));
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn grill_and_summarize_can_only_read_or_nothing_whatever_the_project_allows() {
        let mut grill = request(Path::new("."), Role::Grill);
        grill.permission_mode = "bypassPermissions";
        grill.allowed_tools = "Bash(cargo:*)";
        let args = ClaudeCli::args(&grill);
        assert!(args.windows(2).any(|w| w == ["--tools", "Read,Grep,Glob"]));
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "default"]));
        assert!(args.contains(&"--json-schema".to_string()));
        assert!(!args.iter().any(|a| a == "bypassPermissions" || a == "Bash(cargo:*)"));

        let args = ClaudeCli::args(&request(Path::new("."), Role::Summarize));
        assert!(args.windows(2).any(|w| w == ["--tools", ""]));
    }

    #[test]
    fn the_makefile_agent_can_write_files_but_not_run_commands_whatever_the_project_allows() {
        let mut makefile = request(Path::new("."), Role::Makefile);
        makefile.permission_mode = "bypassPermissions";
        makefile.allowed_tools = "Bash(cargo:*)";
        let args = ClaudeCli::args(&makefile);
        assert!(args.windows(2).any(|w| w == ["--tools", "Read,Grep,Glob,Write,Edit"]));
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(!args.iter().any(|a| a == "bypassPermissions" || a == "Bash(cargo:*)"));
        let schema: Value = serde_json::from_str(MAKEFILE_SCHEMA).unwrap();
        assert_eq!(schema["required"][0], "has_tests");
    }

    #[test]
    fn the_makefile_rules_describe_the_hosts_shell() {
        let windows = makefile_rules(true);
        assert!(windows.contains("under either cmd.exe or a POSIX sh"));
        assert!(!windows.contains("Recipes run under a POSIX sh"));

        let unix = makefile_rules(false);
        assert!(unix.contains("listing them. Recipes run under a POSIX sh. If the project has no tests"));
        assert!(!unix.contains("cmd.exe"));
        assert!(!unix.contains("Windows"));
    }

    #[test]
    fn only_windows_looks_for_an_exe_or_a_cmd() {
        assert_eq!(claude_names(true), ["claude.exe", "claude.cmd", "claude"]);
        assert_eq!(claude_names(false), ["claude"]);
    }

    #[test]
    fn every_role_is_cut_off_from_claude_ai_connectors() {
        for role in [Role::Grill, Role::Build, Role::Makefile, Role::Summarize] {
            assert!(ClaudeCli::args(&request(Path::new("."), role)).contains(&"--strict-mcp-config".to_string()));
        }
    }

    #[test]
    fn the_grill_schema_is_valid_json() {
        let schema: Value = serde_json::from_str(GRILL_SCHEMA).unwrap();
        assert_eq!(schema["properties"]["bump"]["enum"][1], "minor");
    }

    #[test]
    fn grill_questions_require_text_and_can_offer_options_and_a_recommendation() {
        let schema: Value = serde_json::from_str(GRILL_SCHEMA).unwrap();
        let items = &schema["properties"]["questions"]["items"];
        assert_eq!(items["type"], "object");
        assert_eq!(items["required"], serde_json::json!(["text"]));
        assert_eq!(items["properties"]["text"]["type"], "string");
        assert_eq!(items["properties"]["options"]["items"]["type"], "string");
        assert_eq!(items["properties"]["recommended"]["type"], "integer");
    }

    #[test]
    fn reads_structured_output_from_the_result() {
        let event = parse_event(r#"{"type":"result","is_error":false,"result":"x","structured_output":{"status":"ready"}}"#).unwrap();
        assert_eq!(event.structured.unwrap()["status"], "ready");
        let plain = parse_event(r#"{"type":"result","is_error":false,"result":"x","structured_output":null}"#).unwrap();
        assert_eq!(plain.structured, None);
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
