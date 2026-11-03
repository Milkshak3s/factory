//! The guaranteed part of the factory. The agent is one stage here: everything around it
//! (test gate, commit, push, tag) is run by this code, whatever the agent did or said.

use std::fs::{self, File};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, AgentRequest, AgentResult};
use crate::config::{Paths, Project, PushTarget};
use crate::git::{git, git_cmd, origin_url};
use crate::proc::{Cancel, Exit, run_streaming, shell};

const FEEDBACK_TAIL_LINES: usize = 150;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Stage {
    Prepare,
    Setup,
    Agent,
    Test,
    Commit,
    Push,
    Tag,
    Release,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Prepare => "Prepare",
            Stage::Setup => "Setup",
            Stage::Agent => "Agent",
            Stage::Test => "Test",
            Stage::Commit => "Commit",
            Stage::Push => "Push",
            Stage::Tag => "Tag",
            Stage::Release => "Release",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum StageStatus {
    Running,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageRecord {
    pub stage: Stage,
    pub attempt: u32,
    pub status: StageStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// The app closed while this run was in progress.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RunKind {
    Build { task: String },
    Release { tag: String },
}

impl RunKind {
    pub fn title(&self) -> String {
        match self {
            RunKind::Build { task } => subject_line(task),
            RunKind::Release { tag } => format!("Release {tag}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub project: String,
    pub kind: RunKind,
    pub status: RunStatus,
    pub plan: Vec<Stage>,
    pub stages: Vec<StageRecord>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub commit: Option<String>,
    /// Where the work landed, e.g. `origin/factory/<id>` or a tag name.
    pub pushed_to: Option<String>,
    /// Kept after a failed run so it can be inspected; `None` once removed.
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
}

pub struct RunState {
    pub record: RunRecord,
    pub log: Vec<String>,
}

pub struct RunHandle {
    pub state: Arc<Mutex<RunState>>,
    pub cancel: Cancel,
    pub thread: JoinHandle<()>,
}

#[derive(Clone)]
pub struct Env {
    pub paths: Paths,
    pub agent: Arc<dyn Agent>,
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

enum Stop {
    Cancelled,
    Failed(String),
    /// Carries the tail of the test output, to feed back to the agent.
    TestsFailed(String),
}

impl From<String> for Stop {
    fn from(message: String) -> Self {
        Stop::Failed(message)
    }
}

struct Run {
    project: Project,
    env: Env,
    cancel: Cancel,
    state: Arc<Mutex<RunState>>,
    log_file: Mutex<Option<File>>,
    id: String,
    worktree: PathBuf,
    branch: String,
}

pub fn plan_for(project: &Project, kind: &RunKind) -> Vec<Stage> {
    let mut plan = vec![Stage::Prepare];
    if !project.setup_command.trim().is_empty() {
        plan.push(Stage::Setup);
    }
    match kind {
        RunKind::Build { .. } => plan.extend([Stage::Agent, Stage::Test, Stage::Commit, Stage::Push]),
        RunKind::Release { .. } => {
            plan.extend([Stage::Test, Stage::Tag, Stage::Push]);
            if !project.release_command.trim().is_empty() {
                plan.push(Stage::Release);
            }
        }
    }
    plan
}

fn new_run_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = Local::now();
    format!("{}-{:04x}", now.format("%Y%m%d-%H%M%S"), (now.timestamp_subsec_micros() + n) & 0xffff)
}

pub fn start(project: Project, kind: RunKind, env: Env) -> RunHandle {
    let id = new_run_id();
    let worktree = env.paths.data_dir.join("worktrees").join(project.slug()).join(&id);
    let branch = format!("factory/{id}");
    let record = RunRecord {
        id: id.clone(),
        project: project.name.clone(),
        plan: plan_for(&project, &kind),
        kind: kind.clone(),
        status: RunStatus::Running,
        stages: Vec::new(),
        started_at: Local::now().to_rfc3339(),
        finished_at: None,
        error: None,
        commit: None,
        pushed_to: None,
        worktree: None,
        branch: None,
    };
    let dir = env.paths.runs_dir().join(&id);
    let _ = fs::create_dir_all(&dir);
    let log_file = File::create(dir.join("log.txt")).ok();
    let state = Arc::new(Mutex::new(RunState { record, log: Vec::new() }));
    let cancel = Cancel::default();
    let run = Run {
        project,
        env,
        cancel: cancel.clone(),
        state: state.clone(),
        log_file: Mutex::new(log_file),
        id,
        worktree,
        branch,
    };
    let thread = thread::spawn(move || run.execute(kind));
    RunHandle { state, cancel, thread }
}

impl Run {
    fn execute(self, kind: RunKind) {
        self.persist();
        let outcome = catch_unwind(AssertUnwindSafe(|| match &kind {
            RunKind::Build { task } => self.build(task),
            RunKind::Release { tag } => self.release(tag),
        }))
        .unwrap_or_else(|_| Err(Stop::Failed("the pipeline panicked".to_string())));

        let (status, error) = match outcome {
            Ok(()) => (RunStatus::Succeeded, None),
            Err(Stop::Cancelled) => (RunStatus::Cancelled, None),
            Err(Stop::Failed(message)) => (RunStatus::Failed, Some(message)),
            Err(Stop::TestsFailed(_)) => (RunStatus::Failed, Some("Tests failed".to_string())),
        };
        if status == RunStatus::Succeeded {
            self.clean_up();
        }
        if let Some(message) = &error {
            self.log(&format!("FAILED: {message}"));
        }
        self.update(|record| {
            record.status = status;
            record.error = error;
            record.finished_at = Some(Local::now().to_rfc3339());
        });
    }

    fn update(&self, change: impl FnOnce(&mut RunRecord)) {
        change(&mut self.state.lock().unwrap().record);
        self.persist();
        (self.env.notify)();
    }

    fn persist(&self) {
        let _ = save_record(&self.env.paths, &self.state.lock().unwrap().record);
    }

    fn log(&self, line: &str) {
        if let Some(file) = self.log_file.lock().unwrap().as_mut() {
            let _ = writeln!(file, "{line}");
        }
        self.state.lock().unwrap().log.push(line.to_string());
        (self.env.notify)();
    }

    fn stage<T>(&self, stage: Stage, attempt: u32, body: impl FnOnce() -> Result<T, Stop>) -> Result<T, Stop> {
        if self.cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        self.log(&format!("=== {} #{attempt} ===", stage.label()));
        self.update(|record| {
            record.stages.push(StageRecord { stage, attempt, status: StageStatus::Running })
        });
        let result = body();
        let status = match &result {
            Ok(_) => StageStatus::Passed,
            Err(Stop::Cancelled) => StageStatus::Cancelled,
            Err(_) => StageStatus::Failed,
        };
        self.update(|record| {
            if let Some(last) = record.stages.last_mut() {
                last.status = status;
            }
        });
        result
    }

    /// Runs a streamed command. `Ok(false)` means it exited non-zero.
    fn stream(
        &self,
        cmd: std::process::Command,
        input: Option<String>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<bool, Stop> {
        match run_streaming(cmd, input, &self.cancel, on_line) {
            Ok(Exit::Code(code)) => Ok(code == 0),
            Ok(Exit::Cancelled) => Err(Stop::Cancelled),
            Err(e) => Err(Stop::Failed(format!("could not start command: {e}"))),
        }
    }

    fn git_stream(&self, dir: &Path, args: &[&str]) -> Result<(), Stop> {
        self.log(&format!("$ git {}", args.join(" ")));
        if self.stream(git_cmd(dir, args), None, &mut |line| self.log(line))? {
            Ok(())
        } else {
            Err(Stop::Failed(format!("git {} failed", args.join(" "))))
        }
    }

    /// Runs a configured shell command, returning whether it passed and its output tail.
    fn shell_stage(&self, line: &str, envs: &[(String, String)]) -> Result<(bool, String), Stop> {
        self.log(&format!("$ {line}"));
        let mut cmd = shell(line);
        cmd.current_dir(&self.worktree).envs(envs.iter().map(|(k, v)| (k, v)));
        let mut output = Vec::new();
        let passed = self.stream(cmd, None, &mut |text| {
            self.log(text);
            output.push(text.to_string());
        })?;
        let start = output.len().saturating_sub(FEEDBACK_TAIL_LINES);
        Ok((passed, output[start..].join("\n")))
    }

    /// Build tools get a per-project cache, since each run's worktree starts empty.
    fn build_env(&self) -> Vec<(String, String)> {
        if self.worktree.join("Cargo.toml").exists() {
            let dir = self.env.paths.data_dir.join("target").join(self.project.slug());
            vec![("CARGO_TARGET_DIR".to_string(), dir.to_string_lossy().into_owned())]
        } else {
            Vec::new()
        }
    }

    fn prepare(&self, detached: bool) -> Result<String, Stop> {
        if self.project.test_command.trim().is_empty() {
            return Err(Stop::Failed("No test command is configured, so there is no gate to run".into()));
        }
        let repo = &self.project.path;
        if origin_url(repo).is_none() {
            return Err(Stop::Failed("This repo has no `origin` remote to push to".into()));
        }
        let default = &self.project.default_branch;
        self.git_stream(repo, &["fetch", "--tags", "origin", default])?;
        let base = format!("origin/{default}");
        let base_sha = git(repo, &["rev-parse", "--verify", &base])?;

        if let Some(parent) = self.worktree.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let _ = git(repo, &["worktree", "prune"]);
        let path = self.worktree.to_string_lossy().into_owned();
        if detached {
            self.git_stream(repo, &["worktree", "add", "--detach", &path, &base_sha])?;
        } else {
            self.git_stream(repo, &["worktree", "add", "-b", &self.branch, &path, &base_sha])?;
        }
        self.update(|record| {
            record.worktree = Some(self.worktree.clone());
            if !detached {
                record.branch = Some(self.branch.clone());
            }
        });
        Ok(base_sha)
    }

    fn setup(&self) -> Result<(), Stop> {
        let command = self.project.setup_command.trim();
        if command.is_empty() {
            return Ok(());
        }
        self.stage(Stage::Setup, 1, || {
            let (passed, _) = self.shell_stage(command, &self.build_env())?;
            if passed { Ok(()) } else { Err(Stop::Failed("Setup command failed".into())) }
        })
    }

    fn run_tests(&self) -> Result<(), Stop> {
        let (passed, tail) = self.shell_stage(self.project.test_command.trim(), &self.build_env())?;
        if passed { Ok(()) } else { Err(Stop::TestsFailed(tail)) }
    }

    fn run_agent(&self, prompt: String, resume: Option<String>) -> Result<AgentResult, Stop> {
        let wt = &self.worktree;
        let head_before = git(wt, &["rev-parse", "HEAD"])?;
        let tags_before = git(wt, &["tag", "--list"])?;

        let mut env = self.build_env();
        env.extend(push_guard_env(origin_url(&self.project.path).as_deref()));
        let request = AgentRequest {
            dir: wt,
            prompt,
            resume,
            model: &self.project.model,
            permission_mode: &self.project.permission_mode,
            allowed_tools: &self.project.allowed_tools,
            env,
        };
        let result = self
            .env
            .agent
            .run(request, &self.cancel, &mut |line| self.log(line))
            .map_err(Stop::Failed)?
            .ok_or(Stop::Cancelled)?;

        // The denylist asks; these checks are what make the commit stage the only way in.
        let head_after = git(wt, &["rev-parse", "HEAD"])?;
        if head_after != head_before {
            return Err(Stop::Failed(format!(
                "The agent moved HEAD ({head_before} -> {head_after}), which only the pipeline may do"
            )));
        }
        let branch = git(wt, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
        if branch != self.branch {
            return Err(Stop::Failed(format!("The agent left branch {}", self.branch)));
        }
        if git(wt, &["tag", "--list"])? != tags_before {
            self.log("WARNING: the tag list changed during the agent stage");
        }
        if git(wt, &["status", "--porcelain"])?.is_empty() {
            return Err(Stop::Failed("The agent finished without changing anything".into()));
        }
        Ok(result)
    }

    fn build(&self, task: &str) -> Result<(), Stop> {
        let base = self.stage(Stage::Prepare, 1, || self.prepare(false))?;
        self.setup()?;

        let max_attempts = self.project.max_attempts.max(1);
        let mut attempt = 1;
        let mut prompt = task.to_string();
        let mut session = None;
        let summary = loop {
            let result = self.stage(Stage::Agent, attempt, || self.run_agent(prompt.clone(), session.clone()))?;
            session = result.session_id.or(session);
            match self.stage(Stage::Test, attempt, || self.run_tests()) {
                Ok(()) => break result.summary,
                Err(Stop::TestsFailed(tail)) if attempt < max_attempts => {
                    prompt = feedback_prompt(task, &self.project.test_command, &tail);
                    attempt += 1;
                }
                Err(Stop::TestsFailed(_)) => {
                    return Err(Stop::Failed(format!("Tests still failing after {attempt} attempt(s)")));
                }
                Err(stop) => return Err(stop),
            }
        };

        self.stage(Stage::Commit, 1, || self.commit(task, &summary))?;
        self.push(&base, attempt)
    }

    fn commit(&self, task: &str, summary: &str) -> Result<(), Stop> {
        let wt = &self.worktree;
        self.git_stream(wt, &["add", "--all"])?;
        for file in git(wt, &["diff", "--cached", "--name-status"])?.lines() {
            self.log(file);
        }
        let message = commit_message(task, summary, &self.id);
        let mut cmd = git_cmd(wt, &["commit", "--file", "-"]);
        cmd.env("GIT_EDITOR", "true");
        if !self.stream(cmd, Some(message), &mut |line| self.log(line))? {
            return Err(Stop::Failed("git commit failed".into()));
        }
        let sha = git(wt, &["rev-parse", "HEAD"])?;
        self.update(|record| record.commit = Some(sha));
        Ok(())
    }

    fn push(&self, base: &str, tested_attempt: u32) -> Result<(), Stop> {
        let wt = &self.worktree;
        let default = &self.project.default_branch;
        match self.project.push_target {
            PushTarget::Branch => {
                self.stage(Stage::Push, 1, || {
                    self.git_stream(wt, &["push", "origin", &format!("HEAD:refs/heads/{}", self.branch)])
                })?;
                self.update(|record| record.pushed_to = Some(format!("origin/{}", self.branch)));
            }
            PushTarget::Default => {
                let target = format!("HEAD:refs/heads/{default}");
                let moved = self.stage(Stage::Push, 1, || {
                    self.git_stream(wt, &["fetch", "origin", default])?;
                    let tip = git(wt, &["rev-parse", &format!("origin/{default}")])?;
                    if tip == base {
                        self.git_stream(wt, &["push", "origin", &target])?;
                        return Ok(false);
                    }
                    self.log("The default branch moved; rebasing onto it");
                    if let Err(stop) = self.git_stream(wt, &["rebase", &format!("origin/{default}")]) {
                        let _ = git(wt, &["rebase", "--abort"]);
                        return Err(stop);
                    }
                    Ok(true)
                })?;
                if moved {
                    // What got tested is no longer what would ship, so test again before pushing.
                    self.stage(Stage::Test, tested_attempt + 1, || self.run_tests()).map_err(|stop| match stop {
                        Stop::TestsFailed(_) => Stop::Failed("Tests failed after rebasing".into()),
                        other => other,
                    })?;
                    self.stage(Stage::Push, 2, || self.git_stream(wt, &["push", "origin", &target]))?;
                    let sha = git(wt, &["rev-parse", "HEAD"])?;
                    self.update(|record| record.commit = Some(sha));
                }
                self.update(|record| record.pushed_to = Some(format!("origin/{default}")));
            }
        }
        Ok(())
    }

    fn release(&self, tag: &str) -> Result<(), Stop> {
        let repo = &self.project.path;
        let reference = format!("refs/tags/{tag}");
        self.stage(Stage::Prepare, 1, || {
            let valid = !tag.starts_with('-') && git(repo, &["check-ref-format", &reference]).is_ok();
            if !valid {
                return Err(Stop::Failed(format!("`{tag}` is not a valid tag name")));
            }
            let sha = self.prepare(true)?;
            self.update(|record| record.commit = Some(sha));
            let exists_locally = git(repo, &["rev-parse", "-q", "--verify", &reference]).is_ok();
            let exists_remotely = !git(repo, &["ls-remote", "--tags", "origin", &reference])?.is_empty();
            if exists_locally || exists_remotely {
                return Err(Stop::Failed(format!("Tag {tag} already exists")));
            }
            Ok(())
        })?;
        self.setup()?;
        self.stage(Stage::Test, 1, || self.run_tests())?;

        let wt = &self.worktree;
        self.stage(Stage::Tag, 1, || {
            self.git_stream(wt, &["tag", "--annotate", tag, "--message", &format!("Release {tag}")])
        })?;
        self.stage(Stage::Push, 1, || {
            let pushed = self.git_stream(wt, &["push", "origin", &reference]);
            if pushed.is_err() {
                // Nothing left the machine, so don't leave a tag that blocks the retry.
                let _ = git(wt, &["tag", "--delete", tag]);
            }
            pushed
        })?;
        self.update(|record| record.pushed_to = Some(tag.to_string()));

        let command = self.project.release_command.trim();
        if !command.is_empty() {
            self.stage(Stage::Release, 1, || {
                let env = [("FACTORY_TAG".to_string(), tag.to_string())];
                let (passed, _) = self.shell_stage(command, &env)?;
                if passed { Ok(()) } else { Err(Stop::Failed(format!("Release command failed; {tag} is already pushed"))) }
            })?;
        }
        Ok(())
    }

    fn clean_up(&self) {
        let mut record = self.state.lock().unwrap().record.clone();
        match discard(&self.project.path, &mut record) {
            Ok(()) => self.update(|current| {
                current.worktree = None;
                current.branch = None;
            }),
            Err(error) => self.log(&format!("WARNING: could not clean up worktree: {error}")),
        }
    }
}

/// Removes a run's worktree and local branch. Pushed work on the remote is untouched.
pub fn discard(repo: &Path, record: &mut RunRecord) -> Result<(), String> {
    if let Some(worktree) = &record.worktree {
        git(repo, &["worktree", "remove", "--force", &worktree.to_string_lossy()])?;
    }
    record.worktree = None;
    if let Some(branch) = record.branch.take() {
        git(repo, &["branch", "-D", &branch])?;
    }
    Ok(())
}

/// Makes `git push` fail inside the agent's process tree by rewriting push URLs to nowhere.
fn push_guard_env(origin: Option<&str>) -> Vec<(String, String)> {
    let mut prefixes = vec!["https://", "http://", "ssh://", "git@"];
    prefixes.extend(origin);
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), prefixes.len().to_string())];
    for (i, prefix) in prefixes.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), "url.factory-push-blocked://agent/.pushInsteadOf".to_string()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), (*prefix).to_string()));
    }
    env
}

fn feedback_prompt(task: &str, test_command: &str, tail: &str) -> String {
    format!(
        "The factory ran `{test_command}` on your changes and it failed. Fix the code so it passes.\n\n\
         Original task:\n{task}\n\nEnd of the test output:\n```\n{tail}\n```"
    )
}

pub fn subject_line(task: &str) -> String {
    let first = task.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("Factory change");
    if first.chars().count() > 72 {
        format!("{}...", first.chars().take(69).collect::<String>())
    } else {
        first.to_string()
    }
}

fn commit_message(task: &str, summary: &str, run_id: &str) -> String {
    let mut message = subject_line(task);
    message.push_str("\n\n");
    let summary = summary.trim();
    if !summary.is_empty() {
        message.extend(summary.chars().take(2000));
        message.push_str("\n\n");
    }
    message.push_str(&format!("Factory-Run: {run_id}\nCo-Authored-By: Claude <noreply@anthropic.com>\n"));
    message
}

fn save_record(paths: &Paths, record: &RunRecord) -> Result<(), String> {
    let dir = paths.runs_dir().join(&record.id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    fs::write(dir.join("run.json"), text).map_err(|e| e.to_string())
}

/// Loads past runs, newest first. Anything still marked running died with the last session.
pub fn load_records(paths: &Paths) -> Vec<RunRecord> {
    let Ok(entries) = fs::read_dir(paths.runs_dir()) else { return Vec::new() };
    let mut records: Vec<RunRecord> = entries
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("run.json")).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    for record in &mut records {
        if record.status == RunStatus::Running {
            record.status = RunStatus::Interrupted;
            let _ = save_record(paths, record);
        }
    }
    records.sort_by(|a, b| b.id.cmp(&a.id));
    records
}

pub fn save(paths: &Paths, record: &RunRecord) -> Result<(), String> {
    save_record(paths, record)
}

pub fn read_log(paths: &Paths, id: &str) -> Vec<String> {
    fs::read_to_string(paths.runs_dir().join(id).join("log.txt"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    type Script = Box<dyn Fn(&AgentRequest, usize) -> Result<(), String> + Send + Sync>;

    /// Stands in for Claude: runs a closure in the worktree and records its prompts.
    struct FakeAgent {
        calls: Mutex<Vec<(String, Option<String>)>>,
        script: Script,
    }

    impl Agent for FakeAgent {
        fn run(
            &self,
            request: AgentRequest,
            _cancel: &Cancel,
            _log: &mut dyn FnMut(&str),
        ) -> Result<Option<AgentResult>, String> {
            let attempt = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((request.prompt.clone(), request.resume.clone()));
                calls.len()
            };
            (self.script)(&request, attempt)?;
            Ok(Some(AgentResult { session_id: Some("session-1".into()), summary: "Did the thing.".into() }))
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        remote: PathBuf,
        paths: Paths,
        project: Project,
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap_or_else(|e| panic!("{e}"))
    }

    fn configure(repo: &Path) {
        sh(repo, &["config", "user.name", "Test"]);
        sh(repo, &["config", "user.email", "test@example.com"]);
        sh(repo, &["config", "commit.gpgsign", "false"]);
        sh(repo, &["config", "tag.gpgsign", "false"]);
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&remote).unwrap();
        sh(&remote, &["init", "--bare", "-b", "main"]);
        sh(&repo, &["init", "-b", "main"]);
        configure(&repo);
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-m", "initial"]);
        sh(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
        sh(&repo, &["push", "-u", "origin", "main"]);
        let paths = Paths { config_file: dir.path().join("config.toml"), data_dir: dir.path().join("data") };
        let project = Project {
            name: "demo".into(),
            path: repo.clone(),
            test_command: require_file("fixed.txt"),
            ..Default::default()
        };
        Fixture { _dir: dir, repo, remote, paths, project }
    }

    /// A test command that passes only once `file` exists.
    fn require_file(file: &str) -> String {
        if cfg!(windows) { format!("type {file}") } else { format!("cat {file}") }
    }

    fn write_fixed(request: &AgentRequest, _attempt: usize) -> Result<(), String> {
        fs::write(request.dir.join("fixed.txt"), "ok\n").map_err(|e| e.to_string())
    }

    fn run(fx: &Fixture, kind: RunKind, script: Script) -> (RunRecord, Arc<FakeAgent>) {
        let agent = Arc::new(FakeAgent { calls: Mutex::new(Vec::new()), script });
        let env = Env { paths: fx.paths.clone(), agent: agent.clone(), notify: Arc::new(|| {}) };
        let handle = start(fx.project.clone(), kind, env);
        handle.thread.join().unwrap();
        let record = handle.state.lock().unwrap().record.clone();
        (record, agent)
    }

    fn build(task: &str) -> RunKind {
        RunKind::Build { task: task.into() }
    }

    fn remote_refs(fx: &Fixture) -> String {
        sh(&fx.remote, &["for-each-ref", "--format=%(refname)"])
    }

    fn stage_names(record: &RunRecord) -> Vec<&'static str> {
        record.stages.iter().map(|s| s.stage.label()).collect()
    }

    #[test]
    fn happy_path_tests_commits_and_pushes_a_branch() {
        let fx = fixture();
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let (record, _) = run(&fx, build("Fix the bug\n\nmore detail"), Box::new(write_fixed));

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Agent", "Test", "Commit", "Push"]);
        let branch = format!("factory/{}", record.id);
        assert_eq!(record.pushed_to, Some(format!("origin/{branch}")));
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before, "main must not move");
        assert_eq!(sh(&fx.remote, &["show", &format!("{branch}:fixed.txt")]).trim(), "ok");

        let message = sh(&fx.remote, &["log", "-1", "--format=%B", &branch]);
        assert!(message.starts_with("Fix the bug\n\nDid the thing."), "{message}");
        assert!(message.contains(&format!("Factory-Run: {}", record.id)));

        assert_eq!(record.worktree, None);
        assert!(git(&fx.repo, &["rev-parse", "--verify", &branch]).is_err(), "local branch is cleaned up");
    }

    #[test]
    fn default_target_pushes_straight_to_main() {
        let mut fx = fixture();
        fx.project.push_target = PushTarget::Default;
        let (record, _) = run(&fx, build("Fix"), Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(record.pushed_to.as_deref(), Some("origin/main"));
        assert_eq!(sh(&fx.remote, &["show", "main:fixed.txt"]).trim(), "ok");
        assert_eq!(record.commit.as_deref(), Some(sh(&fx.remote, &["rev-parse", "main"]).as_str()));
    }

    #[test]
    fn failing_tests_go_back_to_the_agent_with_the_output() {
        let fx = fixture();
        let script: Script = Box::new(|request, attempt| {
            let file = if attempt == 1 { "wrong.txt" } else { "fixed.txt" };
            fs::write(request.dir.join(file), "x\n").map_err(|e| e.to_string())
        });
        let (record, agent) = run(&fx, build("Fix"), script);

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Agent", "Test", "Agent", "Test", "Commit", "Push"]);
        let calls = agent.calls.lock().unwrap();
        assert_eq!(calls[0], ("Fix".to_string(), None));
        assert!(calls[1].0.contains("it failed"), "{}", calls[1].0);
        assert!(calls[1].0.contains("Original task:\nFix"));
        assert_eq!(calls[1].1.as_deref(), Some("session-1"), "the retry resumes the session");
    }

    #[test]
    fn nothing_is_committed_or_pushed_when_tests_never_pass() {
        let fx = fixture();
        let refs_before = remote_refs(&fx);
        let script: Script = Box::new(|request, _| {
            fs::write(request.dir.join("wrong.txt"), "x\n").map_err(|e| e.to_string())
        });
        let (mut record, agent) = run(&fx, build("Fix"), script);

        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(agent.calls.lock().unwrap().len(), 3, "max_attempts");
        assert!(!stage_names(&record).contains(&"Commit"));
        assert_eq!(remote_refs(&fx), refs_before);

        // The failed work stays around to inspect until it's discarded.
        let worktree = record.worktree.clone().unwrap();
        assert!(worktree.join("wrong.txt").exists());
        discard(&fx.repo, &mut record).unwrap();
        assert!(!worktree.exists());
        assert_eq!(sh(&fx.repo, &["branch", "--list", "factory/*"]), "");
    }

    #[test]
    fn an_agent_that_commits_fails_the_run() {
        let fx = fixture();
        let refs_before = remote_refs(&fx);
        let script: Script = Box::new(|request, attempt| {
            write_fixed(request, attempt)?;
            git(request.dir, &["add", "."])?;
            git(request.dir, &["commit", "-m", "sneaky"])?;
            Ok(())
        });
        let (record, _) = run(&fx, build("Fix"), script);
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("moved HEAD"));
        assert_eq!(remote_refs(&fx), refs_before);
    }

    #[test]
    fn the_agents_environment_blocks_git_push() {
        let fx = fixture();
        let script: Script = Box::new(|request, attempt| {
            write_fixed(request, attempt)?;
            let mut cmd = git_cmd(request.dir, &["push", "origin", "HEAD:refs/heads/sneaky"]);
            cmd.envs(request.env.iter().map(|(k, v)| (k, v)));
            let status = cmd.output().map_err(|e| e.to_string())?.status;
            if status.success() { Err("push got through".into()) } else { Ok(()) }
        });
        let (record, _) = run(&fx, build("Fix"), script);
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert!(!remote_refs(&fx).contains("sneaky"));
    }

    #[test]
    fn an_agent_that_changes_nothing_fails_the_run() {
        let fx = fixture();
        let (record, _) = run(&fx, build("Fix"), Box::new(|_, _| Ok(())));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("without changing"));
    }

    #[test]
    fn no_test_command_means_no_run() {
        let mut fx = fixture();
        fx.project.test_command = "  ".into();
        let (record, agent) = run(&fx, build("Fix"), Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(agent.calls.lock().unwrap().is_empty(), "the agent must not start");
    }

    #[test]
    fn no_origin_fails_before_spending_on_the_agent() {
        let fx = fixture();
        sh(&fx.repo, &["remote", "remove", "origin"]);
        let (record, agent) = run(&fx, build("Fix"), Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("origin"));
        assert!(agent.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn moved_default_branch_is_rebased_and_retested_before_pushing() {
        let mut fx = fixture();
        fx.project.push_target = PushTarget::Default;
        let remote = fx.remote.clone();
        let other = fx._dir.path().join("other");
        let script: Script = Box::new(move |request, attempt| {
            sh(request.dir.parent().unwrap(), &["clone", &remote.to_string_lossy(), &other.to_string_lossy()]);
            configure(&other);
            fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
            sh(&other, &["add", "."]);
            sh(&other, &["commit", "-m", "someone else's work"]);
            sh(&other, &["push", "origin", "main"]);
            write_fixed(request, attempt)
        });
        let (record, _) = run(&fx, build("Fix"), script);

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Agent", "Test", "Commit", "Push", "Test", "Push"]);
        assert_eq!(sh(&fx.remote, &["show", "main:theirs.txt"]).trim(), "theirs");
        assert_eq!(sh(&fx.remote, &["show", "main:fixed.txt"]).trim(), "ok");
        assert_eq!(record.commit.as_deref(), Some(sh(&fx.remote, &["rev-parse", "main"]).as_str()));
    }

    #[test]
    fn release_tests_tags_pushes_and_runs_the_release_command() {
        let mut fx = fixture();
        fx.project.test_command = require_file("README.md");
        let marker = fx._dir.path().join("released.txt");
        fx.project.release_command = if cfg!(windows) {
            format!("echo %FACTORY_TAG%> \"{}\"", marker.display())
        } else {
            format!("echo $FACTORY_TAG > '{}'", marker.display())
        };
        let (record, agent) = run(&fx, RunKind::Release { tag: "v1.2.3".into() }, Box::new(write_fixed));

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Test", "Tag", "Push", "Release"]);
        assert!(agent.calls.lock().unwrap().is_empty(), "releases don't involve the agent");
        assert_eq!(sh(&fx.remote, &["rev-parse", "v1.2.3^{commit}"]), sh(&fx.remote, &["rev-parse", "main"]));
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "v1.2.3");
    }

    #[test]
    fn release_stops_at_failing_tests_without_tagging() {
        let fx = fixture();
        let (record, _) = run(&fx, RunKind::Release { tag: "v1.0.0".into() }, Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(!remote_refs(&fx).contains("v1.0.0"));
        assert_eq!(sh(&fx.repo, &["tag", "--list"]), "");
    }

    #[test]
    fn release_refuses_existing_and_invalid_tags() {
        let mut fx = fixture();
        fx.project.test_command = require_file("README.md");
        sh(&fx.repo, &["tag", "v1.0.0"]);
        sh(&fx.repo, &["push", "origin", "v1.0.0"]);
        let (record, _) = run(&fx, RunKind::Release { tag: "v1.0.0".into() }, Box::new(write_fixed));
        assert!(record.error.unwrap().contains("already exists"));

        let (record, _) = run(&fx, RunKind::Release { tag: "--bad tag".into() }, Box::new(write_fixed));
        assert!(record.error.unwrap().contains("not a valid tag"));
    }

    #[test]
    fn cancelling_kills_a_running_test_command() {
        let mut fx = fixture();
        fx.project.test_command =
            if cfg!(windows) { "ping -n 60 127.0.0.1".into() } else { "sleep 60".into() };
        let agent = Arc::new(FakeAgent { calls: Mutex::new(Vec::new()), script: Box::new(write_fixed) });
        let env = Env { paths: fx.paths.clone(), agent, notify: Arc::new(|| {}) };
        let handle = start(fx.project.clone(), build("Fix"), env);

        let started = Instant::now();
        while !handle.state.lock().unwrap().log.iter().any(|line| line.contains("Test #1")) {
            assert!(started.elapsed() < Duration::from_secs(30), "never reached the test stage");
            thread::sleep(Duration::from_millis(20));
        }
        handle.cancel.cancel();
        handle.thread.join().unwrap();

        assert!(started.elapsed() < Duration::from_secs(45));
        let state = handle.state.lock().unwrap();
        assert_eq!(state.record.status, RunStatus::Cancelled);
        assert_eq!(state.record.stages.last().unwrap().status, StageStatus::Cancelled);
    }

    /// Spends real tokens, so it only runs with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_claude_runs_the_whole_pipeline() {
        let mut fx = fixture();
        fx.project.model = "haiku".into();
        let program = crate::agent::locate_claude(None).expect("claude binary");
        let env = Env {
            paths: fx.paths.clone(),
            agent: Arc::new(crate::agent::ClaudeCli { program }),
            notify: Arc::new(|| {}),
        };
        let task = "Create a file named fixed.txt in the current directory containing the word ok.";
        let handle = start(fx.project.clone(), build(task), env);
        handle.thread.join().unwrap();

        let state = handle.state.lock().unwrap();
        let record = &state.record;
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}\n{}", record.error, state.log.join("\n"));
        let branch = format!("factory/{}", record.id);
        assert!(sh(&fx.remote, &["show", &format!("{branch}:fixed.txt")]).contains("ok"));
        println!("{}", sh(&fx.remote, &["log", "-1", "--format=%B", &branch]));
        println!("{}", state.log.join("\n"));
    }

    #[test]
    fn runs_are_saved_and_stale_running_ones_load_as_interrupted() {
        let fx = fixture();
        let (record, _) = run(&fx, build("Fix"), Box::new(write_fixed));
        let mut stale = record.clone();
        stale.id = "99999999-000000-0000".into();
        stale.status = RunStatus::Running;
        save_record(&fx.paths, &stale).unwrap();

        let loaded = load_records(&fx.paths);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, stale.id, "newest first");
        assert_eq!(loaded[0].status, RunStatus::Interrupted);
        assert_eq!(loaded[1].status, RunStatus::Succeeded);
        assert!(read_log(&fx.paths, &record.id).iter().any(|line| line.contains("Commit #1")));
    }
}
