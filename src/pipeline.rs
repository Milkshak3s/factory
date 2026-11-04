//! The guaranteed part of the factory. Agents write the brief and the code; everything else
//! (test gate, commit, merge, release, publish) is run by this code, whatever an agent did or said.

use std::fs::{self, File};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, AgentRequest, AgentResult, Role};
use crate::config::{Paths, Project};
use crate::forge::Forge;
use crate::git::{git, git_cmd, latest_tag, origin_url};
use crate::proc::{Cancel, Exit, command, run_streaming, shell};
use crate::version::{self, Bump, Manifest};

/// The gate. Every project provides it, so the pipeline never has to guess how to test.
pub const TEST_COMMAND: &str = "make test";

const FEEDBACK_TAIL_LINES: usize = 150;
const RELEASE_WORKFLOW: &str = "release.yml";
const WORKFLOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WORKFLOW_RUN_TIMEOUT: Duration = Duration::from_secs(90 * 60);
const GITHUB_WAIT: &str = "Waiting for GitHub";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Stage {
    Prepare,
    Grill,
    Branch,
    Setup,
    Agent,
    Test,
    Commit,
    Merge,
    Release,
    Publish,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Prepare => "Prepare",
            Stage::Grill => "Grill",
            Stage::Branch => "Branch",
            Stage::Setup => "Setup",
            Stage::Agent => "Build",
            Stage::Test => "Test",
            Stage::Commit => "Commit",
            Stage::Merge => "Merge",
            Stage::Release => "Release",
            Stage::Publish => "Publish",
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
    Build {
        task: String,
        /// `Some(bump)` skips the grill and uses that bump. `None` runs the grill, which picks the bump.
        #[serde(default)]
        bump: Option<Bump>,
    },
    Release { bump: Bump },
    CreateMakefile,
}

impl RunKind {
    pub fn title(&self) -> String {
        match self {
            RunKind::Build { task, .. } => subject_line(task),
            RunKind::Release { bump } => format!("Release ({} bump)", bump.label()),
            RunKind::CreateMakefile => "Create Makefile".to_string(),
        }
    }
}

/// Something the grill agent wants to know, and the answers it suggests, if any.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "QuestionForm")]
pub struct Question {
    pub text: String,
    pub options: Vec<String>,
    /// Index into `options` of the one the agent would pick.
    pub recommended: Option<usize>,
}

/// Saved runs and plain replies can hold a bare string, so both forms have to keep loading.
#[derive(Deserialize)]
#[serde(untagged)]
enum QuestionForm {
    Text(String),
    Full {
        text: String,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        recommended: Option<usize>,
    },
}

impl From<QuestionForm> for Question {
    fn from(form: QuestionForm) -> Self {
        match form {
            QuestionForm::Text(text) => Question::plain(text),
            QuestionForm::Full { text, options, recommended } => Question { text, options, recommended },
        }
    }
}

impl Question {
    /// A question with nothing to pick from.
    pub fn plain(text: impl Into<String>) -> Self {
        Question { text: text.into(), options: Vec::new(), recommended: None }
    }

    /// Trims the options and drops empty ones. `recommended` stays on the option it pointed at,
    /// and becomes `None` if that option is gone or never existed.
    fn normalise(&mut self) {
        let mut options = Vec::new();
        let mut recommended = None;
        for (index, option) in self.options.iter().enumerate() {
            let option = option.trim();
            if !option.is_empty() {
                if self.recommended == Some(index) {
                    recommended = Some(options.len());
                }
                options.push(option.to_string());
            }
        }
        self.options = options;
        self.recommended = recommended;
    }
}

/// What a running run is held up on right now, for the status line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub label: String,
    /// When this wait began, in the same format as `started_at`.
    pub since: String,
    pub url: Option<String>,
}

impl Wait {
    fn new(label: impl Into<String>) -> Self {
        Wait { label: label.into(), since: Local::now().to_rfc3339(), url: None }
    }
}

/// One round of the grill: what the agent asked and what it was told.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    pub questions: Vec<String>,
    pub answer: String,
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
    /// Where the work landed, e.g. `origin/main`.
    pub pushed_to: Option<String>,
    /// Kept after a failed run so it can be inspected; `None` once removed.
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    /// Questions waiting on the user. The run is paused while this is set.
    pub questions: Option<Vec<Question>>,
    pub transcript: Vec<Exchange>,
    pub brief: Option<String>,
    pub release_tag: Option<String>,
    pub release_url: Option<String>,
    pub release_notes: Option<String>,
    /// Something a successful run still needs the user to know.
    #[serde(default)]
    pub note: Option<String>,
    /// `None` between stages and once the run is over.
    #[serde(default)]
    pub waiting_on: Option<Wait>,
}

pub struct RunState {
    pub record: RunRecord,
    pub log: Vec<String>,
}

pub enum GrillReply {
    Answer(String),
    /// The user has said enough: decide what's left and write the brief.
    Proceed,
}

pub struct RunHandle {
    pub state: Arc<Mutex<RunState>>,
    pub cancel: Cancel,
    pub replies: mpsc::Sender<GrillReply>,
    pub thread: JoinHandle<()>,
}

#[derive(Clone)]
pub struct Env {
    pub paths: Paths,
    pub agent: Arc<dyn Agent>,
    pub forge: Arc<dyn Forge>,
    pub notify: Arc<dyn Fn() + Send + Sync>,
    pub poll_every: Duration,
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
    replies: Mutex<mpsc::Receiver<GrillReply>>,
    log_file: Mutex<Option<File>>,
    id: String,
    worktree: PathBuf,
    branch: String,
}

/// What the grill settled on, ready to hand to the build agent.
struct Brief {
    title: String,
    text: String,
    bump: Bump,
}

#[derive(Debug, Default, Deserialize)]
struct GrillTurn {
    #[serde(default)]
    status: String,
    #[serde(default)]
    questions: Vec<Question>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    brief: String,
    #[serde(default)]
    bump: Bump,
}

pub fn plan_for(project: &Project, kind: &RunKind) -> Vec<Stage> {
    let setup = !project.setup_command.trim().is_empty();
    let mut plan = vec![Stage::Prepare];
    match kind {
        RunKind::Build { bump, .. } => {
            if bump.is_none() {
                plan.push(Stage::Grill);
            }
            plan.push(Stage::Branch);
            if setup {
                plan.push(Stage::Setup);
            }
            plan.extend([Stage::Agent, Stage::Test, Stage::Commit, Stage::Merge, Stage::Release, Stage::Publish]);
        }
        RunKind::Release { .. } => {
            if setup {
                plan.push(Stage::Setup);
            }
            plan.extend([Stage::Test, Stage::Release, Stage::Publish]);
        }
        RunKind::CreateMakefile => plan.extend([Stage::Agent, Stage::Test, Stage::Commit, Stage::Merge]),
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
        questions: None,
        transcript: Vec::new(),
        brief: None,
        release_tag: None,
        release_url: None,
        release_notes: None,
        note: None,
        waiting_on: None,
    };
    let dir = env.paths.runs_dir().join(&id);
    let _ = fs::create_dir_all(&dir);
    let log_file = File::create(dir.join("log.txt")).ok();
    let state = Arc::new(Mutex::new(RunState { record, log: Vec::new() }));
    let cancel = Cancel::default();
    let (replies_tx, replies_rx) = mpsc::channel();
    let run = Run {
        project,
        env,
        cancel: cancel.clone(),
        state: state.clone(),
        replies: Mutex::new(replies_rx),
        log_file: Mutex::new(log_file),
        id,
        worktree,
        branch,
    };
    let thread = thread::spawn(move || run.execute(kind));
    RunHandle { state, cancel, replies: replies_tx, thread }
}

impl Run {
    fn execute(self, kind: RunKind) {
        self.persist();
        let outcome = catch_unwind(AssertUnwindSafe(|| match &kind {
            RunKind::Build { task, bump } => self.build(task, *bump),
            RunKind::Release { bump } => self.release_only(*bump),
            RunKind::CreateMakefile => self.create_makefile(),
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
            record.questions = None;
            record.waiting_on = None;
            record.finished_at = Some(Local::now().to_rfc3339());
        });
    }

    fn update(&self, change: impl FnOnce(&mut RunRecord)) {
        change(&mut self.state.lock().unwrap().record);
        self.persist();
        (self.env.notify)();
    }

    /// Names what the run is waiting on now. A new wait restarts the timer and drops any old link.
    fn set_wait(&self, label: impl Into<String>) {
        self.update(|record| record.waiting_on = Some(Wait::new(label)));
    }

    /// The forge, for a call the run has to wait on.
    fn github(&self) -> &dyn Forge {
        self.set_wait(GITHUB_WAIT);
        &*self.env.forge
    }

    fn persist(&self) {
        let _ = save(&self.env.paths, &self.state.lock().unwrap().record);
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
            record.waiting_on = None;
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
        self.set_wait(git_wait_label(args));
        if self.stream(git_cmd(dir, args), None, &mut |line| self.log(line))? {
            Ok(())
        } else {
            Err(Stop::Failed(format!("git {} failed", args.join(" "))))
        }
    }

    /// Runs a shell command in the worktree, returning whether it passed and its output tail.
    fn shell_stage(&self, line: &str, envs: &[(String, String)]) -> Result<(bool, String), Stop> {
        self.log(&format!("$ {line}"));
        self.set_wait(format!("Waiting for `{}`", first_line(line)));
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

    /// Makes a detached worktree at the tip of the default branch and returns that commit.
    /// `full` also checks what a whole run needs, before any agent time is spent.
    fn prepare(&self, full: bool) -> Result<String, Stop> {
        let repo = &self.project.path;
        if origin_url(repo).is_none() {
            return Err(Stop::Failed("This repo has no `origin` remote to push to".into()));
        }
        if full {
            self.set_wait("Waiting for the GitHub CLI check");
            self.env.forge.ready(repo).map_err(|e| format!("GitHub CLI isn't ready: {e}"))?;
        }
        let default = &self.project.default_branch;
        self.git_stream(repo, &["fetch", "--tags", "origin", default])?;
        self.pull_local_branch();
        let base_sha = git(repo, &["rev-parse", "--verify", &format!("origin/{default}")])?;

        if let Some(parent) = self.worktree.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let _ = git(repo, &["worktree", "prune"]);
        let path = self.worktree.to_string_lossy().into_owned();
        self.git_stream(repo, &["worktree", "add", "--detach", &path, &base_sha])?;
        self.update(|record| record.worktree = Some(self.worktree.clone()));

        if full {
            if !self.worktree.join("Makefile").exists() {
                return Err(Stop::Failed(format!("origin/{default} has no Makefile. Use Create Makefile first.")));
            }
            if !self.make_has_test_target()? {
                return Err(Stop::Failed("The Makefile has no `test` target".into()));
            }
        }
        Ok(base_sha)
    }

    /// Brings the local default branch up to date too. Only ever a fast-forward: runs work from
    /// `origin` either way, so local edits or commits are a warning rather than a reason to stop.
    fn pull_local_branch(&self) {
        let repo = &self.project.path;
        let default = &self.project.default_branch;
        if git(repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{default}")]).is_err() {
            return;
        }
        let current = git(repo, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
        let pulled = if current == *default {
            git(repo, &["merge", "--ff-only", &format!("origin/{default}")])
        } else {
            git(repo, &["fetch", "origin", &format!("{default}:{default}")])
        };
        match pulled {
            Ok(_) => self.log(&format!("Local {default} is up to date with origin/{default}")),
            Err(error) => self.log(&format!("WARNING: could not update local {default}: {error}")),
        }
    }

    fn make_has_test_target(&self) -> Result<bool, Stop> {
        self.set_wait("Waiting for `make -n test`");
        let mut cmd = command("make");
        cmd.current_dir(&self.worktree).args(["-n", "test"]);
        self.stream(cmd, None, &mut |_| {})
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
        let (passed, tail) = self.shell_stage(TEST_COMMAND, &self.build_env())?;
        if passed { Ok(()) } else { Err(Stop::TestsFailed(tail)) }
    }

    fn call_agent(&self, role: Role, prompt: String, resume: Option<String>) -> Result<AgentResult, Stop> {
        let attempt = self.state.lock().unwrap().record.stages.last().map_or(1, |stage| stage.attempt);
        self.set_wait(agent_wait_label(&role, attempt, self.project.max_attempts.max(1)));
        let mut env = self.build_env();
        env.extend(push_guard_env(origin_url(&self.project.path).as_deref()));
        let request = AgentRequest {
            role,
            dir: &self.worktree,
            prompt,
            resume,
            model: &self.project.model,
            permission_mode: &self.project.permission_mode,
            allowed_tools: &self.project.allowed_tools,
            env,
        };
        self.env
            .agent
            .run(request, &self.cancel, &mut |line| self.log(line))
            .map_err(Stop::Failed)?
            .ok_or(Stop::Cancelled)
    }

    /// Questions the user until the agent can state one specific solution.
    fn grill(&self, task: &str) -> Result<Brief, Stop> {
        let mut prompt = task.to_string();
        let mut session = None;
        let mut told_to_finish = false;
        loop {
            let result = self.call_agent(Role::Grill, prompt, session.clone())?;
            session = result.session_id.clone().or(session);
            let turn = parse_grill_turn(&result);

            if turn.status == "ready" && !turn.brief.trim().is_empty() {
                let title = if turn.title.trim().is_empty() { task } else { &turn.title };
                self.log(&format!("Agreed: {title} ({} bump)", turn.bump.label()));
                self.update(|record| record.brief = Some(turn.brief.clone()));
                return Ok(Brief { title: subject_line(title), text: turn.brief, bump: turn.bump });
            }
            if told_to_finish || turn.questions.is_empty() {
                return Err(Stop::Failed("The grill ended without producing a brief".into()));
            }

            let questions = turn.questions;
            self.update(|record| {
                record.questions = Some(questions.clone());
                record.waiting_on = Some(Wait::new("Waiting for your answers"));
            });
            let reply = self.wait_for_reply()?;
            let answer = match reply {
                GrillReply::Answer(answer) => {
                    prompt = answer.clone();
                    answer
                }
                GrillReply::Proceed => {
                    told_to_finish = true;
                    prompt = "Stop asking questions. Settle anything still open with the simplest \
                              sensible choice, and return status ready with the brief now."
                        .to_string();
                    "(Go ahead with your best judgment.)".to_string()
                }
            };
            self.log(&format!("You: {answer}"));
            let questions: Vec<String> = questions.into_iter().map(|question| question.text).collect();
            self.update(|record| {
                record.questions = None;
                record.waiting_on = None;
                record.transcript.push(Exchange { questions, answer });
            });
        }
    }

    fn wait_for_reply(&self) -> Result<GrillReply, Stop> {
        let replies = self.replies.lock().unwrap();
        loop {
            if self.cancel.is_cancelled() {
                return Err(Stop::Cancelled);
            }
            match replies.recv_timeout(Duration::from_millis(100)) {
                Ok(reply) => return Ok(reply),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Stop::Cancelled),
            }
        }
    }

    fn run_build_agent(&self, prompt: String, resume: Option<String>) -> Result<AgentResult, Stop> {
        let wt = &self.worktree;
        let head_before = git(wt, &["rev-parse", "HEAD"])?;
        let tags_before = git(wt, &["tag", "--list"])?;

        let result = self.call_agent(Role::Build, prompt, resume)?;

        // The denylist asks; these checks are what make the later stages the only way through.
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
            self.log("WARNING: the tag list changed during the build stage");
        }
        // `make test` is the gate, so the Makefile can't change in the run it's judging.
        if !git(wt, &["status", "--porcelain", "--", "Makefile"])?.is_empty() {
            return Err(Stop::Failed("The agent changed the Makefile, which defines the test gate".into()));
        }
        if git(wt, &["status", "--porcelain"])?.is_empty() {
            return Err(Stop::Failed("The agent finished without changing anything".into()));
        }
        Ok(result)
    }

    /// `skip_grill_with` is the bump to use when the grill is skipped.
    fn build(&self, task: &str, skip_grill_with: Option<Bump>) -> Result<(), Stop> {
        let base = self.stage(Stage::Prepare, 1, || self.prepare(true))?;
        let (title, brief, bump) = match skip_grill_with {
            Some(bump) => (subject_line(task), None, bump),
            None => {
                let brief = self.stage(Stage::Grill, 1, || self.grill(task))?;
                (brief.title, Some(brief.text), brief.bump)
            }
        };
        self.stage(Stage::Branch, 1, || {
            self.git_stream(&self.worktree, &["switch", "--create", &self.branch])?;
            self.update(|record| record.branch = Some(self.branch.clone()));
            Ok(())
        })?;
        self.setup()?;

        let transcript = self.state.lock().unwrap().record.transcript.clone();
        let build_prompt = initial_build_prompt(task, brief.as_deref(), &transcript);
        let max_attempts = self.project.max_attempts.max(1);
        let mut attempt = 1;
        let mut prompt = build_prompt.clone();
        let mut session = None;
        let summary = loop {
            let result = self.stage(Stage::Agent, attempt, || self.run_build_agent(prompt.clone(), session.clone()))?;
            session = result.session_id.or(session);
            match self.stage(Stage::Test, attempt, || self.run_tests()) {
                Ok(()) => break result.summary,
                Err(Stop::TestsFailed(tail)) if attempt < max_attempts => {
                    prompt = feedback_prompt(&build_prompt, &tail);
                    attempt += 1;
                }
                Err(Stop::TestsFailed(_)) => {
                    return Err(Stop::Failed(format!("Tests still failing after {attempt} attempt(s)")));
                }
                Err(stop) => return Err(stop),
            }
        };

        self.stage(Stage::Commit, 1, || self.commit(&title, &summary))?;
        self.merge(&base, attempt)?;
        self.release(bump)
    }

    fn commit(&self, title: &str, summary: &str) -> Result<(), Stop> {
        let wt = &self.worktree;
        self.git_stream(wt, &["add", "--all"])?;
        for file in git(wt, &["diff", "--cached", "--name-status"])?.lines() {
            self.log(file);
        }
        let message = commit_message(title, summary, &self.id);
        let args = ["commit", "--file", "-"];
        self.set_wait(git_wait_label(&args));
        let cmd = git_cmd(wt, &args);
        if !self.stream(cmd, Some(message), &mut |line| self.log(line))? {
            return Err(Stop::Failed("git commit failed".into()));
        }
        let sha = git(wt, &["rev-parse", "HEAD"])?;
        self.update(|record| record.commit = Some(sha));
        Ok(())
    }

    /// Lands the branch on the default branch, keeping history linear as these repos do.
    fn merge(&self, base: &str, tested_attempt: u32) -> Result<(), Stop> {
        let wt = &self.worktree;
        let default = &self.project.default_branch;
        let target = format!("HEAD:refs/heads/{default}");
        let moved = self.stage(Stage::Merge, 1, || {
            self.git_stream(wt, &["fetch", "origin", default])?;
            let tip = git(wt, &["rev-parse", &format!("origin/{default}")])?;
            if tip == base {
                self.git_stream(wt, &["push", "origin", &target])?;
                return Ok(false);
            }
            self.log(&format!("{default} moved; rebasing onto it"));
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
            self.stage(Stage::Merge, 2, || self.git_stream(wt, &["push", "origin", &target]))?;
        }
        let sha = git(wt, &["rev-parse", "HEAD"])?;
        self.update(|record| {
            record.commit = Some(sha);
            record.pushed_to = Some(format!("origin/{default}"));
        });
        Ok(())
    }

    fn release_only(&self, bump: Bump) -> Result<(), Stop> {
        self.stage(Stage::Prepare, 1, || self.prepare(true))?;
        self.setup()?;
        self.stage(Stage::Test, 1, || self.run_tests())?;
        self.release(bump)
    }

    /// Has an agent write the Makefile, then holds it to what it claims about the project.
    fn create_makefile(&self) -> Result<(), Stop> {
        let wt = &self.worktree;
        self.stage(Stage::Prepare, 1, || {
            self.prepare(false)?;
            if wt.join("Makefile").exists() {
                return Err(Stop::Failed("This project already has a Makefile".into()));
            }
            Ok(())
        })?;

        let max_attempts = self.project.max_attempts.max(1);
        let mut attempt = 1;
        let mut prompt = "Write the Makefile for this project.".to_string();
        let mut session = None;
        let has_tests = loop {
            let result = self.stage(Stage::Agent, attempt, || self.run_makefile_agent(prompt.clone(), session.clone()))?;
            session = result.session_id.clone().or(session);
            let has_tests = result.structured.as_ref().and_then(|v| v["has_tests"].as_bool()).unwrap_or(true);
            match self.stage(Stage::Test, attempt, || self.check_new_gate(has_tests)) {
                Ok(()) => break has_tests,
                Err(Stop::TestsFailed(problem)) if attempt < max_attempts => {
                    prompt = format!("{problem}\n\nFix the Makefile.");
                    attempt += 1;
                }
                Err(Stop::TestsFailed(problem)) => {
                    return Err(Stop::Failed(format!("No working Makefile after {attempt} attempt(s). {problem}")));
                }
                Err(stop) => return Err(stop),
            }
        };
        if !has_tests {
            let note = "This project has no tests yet, so `make test` fails until it does. Runs can't land until then.";
            self.log(note);
            self.update(|record| record.note = Some(note.to_string()));
        }

        self.stage(Stage::Commit, 1, || {
            for line in fs::read_to_string(wt.join("Makefile")).map_err(|e| e.to_string())?.lines() {
                self.log(line);
            }
            // Just the Makefile: running the tests may have left other files behind.
            self.git_stream(wt, &["add", "Makefile"])?;
            self.git_stream(wt, &["commit", "--message", "Add Makefile"])
        })?;
        let target = format!("HEAD:refs/heads/{}", self.project.default_branch);
        self.stage(Stage::Merge, 1, || self.git_stream(wt, &["push", "origin", &target]))?;
        let sha = git(wt, &["rev-parse", "HEAD"])?;
        self.update(|record| {
            record.commit = Some(sha);
            record.pushed_to = Some(format!("origin/{}", self.project.default_branch));
        });
        Ok(())
    }

    fn run_makefile_agent(&self, prompt: String, resume: Option<String>) -> Result<AgentResult, Stop> {
        let wt = &self.worktree;
        let head_before = git(wt, &["rev-parse", "HEAD"])?;
        let result = self.call_agent(Role::Makefile, prompt, resume)?;
        if git(wt, &["rev-parse", "HEAD"])? != head_before {
            return Err(Stop::Failed("The agent moved HEAD, which only the pipeline may do".into()));
        }
        let changed = git(wt, &["status", "--porcelain"])?;
        if let Some(other) = changed.lines().find(|line| line.get(3..) != Some("Makefile")) {
            return Err(Stop::Failed(format!("The agent changed more than the Makefile: {other}")));
        }
        Ok(result)
    }

    /// A Makefile has to run the tests it says exist. And if it says there are none, `make test`
    /// has to fail, because a gate that passes with no tests would wave every change through.
    fn check_new_gate(&self, has_tests: bool) -> Result<(), Stop> {
        if !self.worktree.join("Makefile").exists() {
            return Err(Stop::TestsFailed("There is no Makefile in the project root.".into()));
        }
        if !self.make_has_test_target()? {
            return Err(Stop::TestsFailed("`make -n test` failed: the Makefile has no usable `test` target.".into()));
        }
        let (passed, tail) = self.shell_stage(TEST_COMMAND, &self.build_env())?;
        match (has_tests, passed) {
            (true, false) => Err(Stop::TestsFailed(format!("`{TEST_COMMAND}` failed:\n```\n{tail}\n```"))),
            (false, true) => Err(Stop::TestsFailed(format!(
                "You reported no tests, but `{TEST_COMMAND}` passed. With no tests it must exit non-zero."
            ))),
            _ => Ok(()),
        }
    }

    /// Cuts the release, then promotes it from draft to published once it's finished.
    fn release(&self, bump: Bump) -> Result<(), Stop> {
        let tag = self.stage(Stage::Release, 1, || self.cut_release(bump))?;
        self.stage(Stage::Publish, 1, || self.publish(&tag))
    }

    fn cut_release(&self, bump: Bump) -> Result<String, Stop> {
        let wt = &self.worktree;
        let previous = latest_tag(wt);
        let manifest = version::read_manifest(wt);
        let current = manifest
            .as_ref()
            .map(|(_, version)| version.clone())
            .or_else(|| previous.clone())
            .unwrap_or_else(|| "0.0.0".to_string());
        let version = version::bumped(&current, bump)?;
        let tag = format!("v{version}");
        self.log(&format!("Releasing {tag}"));
        self.update(|record| record.release_tag = Some(tag.clone()));
        if git(wt, &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")]).is_ok()
            || self.github().release(wt, &tag)?.is_some()
        {
            return Err(Stop::Failed(format!("{tag} already exists")));
        }

        let notes = self.release_notes(previous.as_deref());
        for line in notes.lines() {
            self.log(line);
        }
        self.update(|record| record.release_notes = Some(notes.clone()));

        if let Some((kind, _)) = &manifest {
            self.write_version(*kind, &version)?;
            self.git_stream(wt, &["add", "--all"])?;
            let message = format!("Release {version}\n\nFactory-Run: {}\n", self.id);
            let args = ["commit", "--file", "-"];
            self.set_wait(git_wait_label(&args));
            let cmd = git_cmd(wt, &args);
            if !self.stream(cmd, Some(message), &mut |line| self.log(line))? {
                return Err(Stop::Failed("Could not commit the version bump".into()));
            }
        }
        let sha = git(wt, &["rev-parse", "HEAD"])?;
        self.update(|record| record.commit = Some(sha.clone()));

        let main = format!("HEAD:refs/heads/{}", self.project.default_branch);
        if has_release_workflow(wt) {
            // These projects build on CI: a push to `release` produces the draft.
            self.git_stream(wt, &["push", "--atomic", "origin", &main, "HEAD:refs/heads/release"])?;
            self.wait_for_workflow(&sha)?;
            let draft = self.github().release(wt, &tag)?;
            if !draft.is_some_and(|release| release.is_draft) {
                return Err(Stop::Failed(format!("The release workflow passed but left no draft for {tag}")));
            }
            self.github().set_notes(wt, &tag, &notes)?;
        } else {
            self.git_stream(wt, &["push", "origin", &main])?;
            let title = format!("{} {tag}", self.project.name);
            self.github().create_draft(wt, &tag, &title, &sha, &notes)?;
            let command = self.project.release_command.trim();
            if !command.is_empty() {
                let env = [("FACTORY_TAG".to_string(), tag.clone())];
                let (passed, _) = self.shell_stage(command, &env)?;
                if !passed {
                    return Err(Stop::Failed(format!("Release command failed; {tag} is still a draft")));
                }
            }
        }
        Ok(tag)
    }

    fn write_version(&self, manifest: Manifest, version: &str) -> Result<(), Stop> {
        let wt = &self.worktree;
        let edit = match manifest {
            Manifest::Cargo => {
                version::set_cargo_version(wt, version)?;
                if !wt.join("Cargo.lock").exists() {
                    return Ok(());
                }
                // Release workflows build with --locked, so the lockfile has to move too.
                "cargo update --workspace --offline".to_string()
            }
            Manifest::Npm => format!("npm version {version} --no-git-tag-version"),
        };
        let (passed, _) = self.shell_stage(&edit, &[])?;
        if passed { Ok(()) } else { Err(Stop::Failed(format!("`{edit}` failed"))) }
    }

    /// Asks for a bullet summary of everything since the last tag, falling back to commit subjects:
    /// a failed summary shouldn't be what stops a tested change from shipping.
    fn release_notes(&self, previous: Option<&str>) -> String {
        let wt = &self.worktree;
        let range = previous.map(|tag| format!("{tag}..HEAD"));
        let mut log_args = vec!["log", "--format=%s", "--no-merges", "--max-count=50"];
        let mut stat_args = vec!["diff", "--stat"];
        match &range {
            Some(range) => {
                log_args.push(range);
                stat_args.push(range);
            }
            None => log_args.push("HEAD"),
        }
        let subjects = git(wt, &log_args).unwrap_or_default();
        let stat = if range.is_some() { git(wt, &stat_args).unwrap_or_default() } else { String::new() };
        let prompt = format!(
            "Write release notes for these changes as a concise markdown bullet list: one short, \
             user-facing bullet per meaningful change, in plain language. Output only the bullets.\n\n\
             Commits:\n{subjects}\n\nFiles changed:\n{stat}"
        );
        let summary = self.call_agent(Role::Summarize, prompt, None).map(|result| result.summary);
        let bullets = summary.map(|text| only_bullets(&text)).unwrap_or_default();
        if !bullets.is_empty() {
            return bullets;
        }
        self.log("Falling back to commit subjects for the release notes");
        subjects.lines().map(|subject| format!("- {subject}")).collect::<Vec<_>>().join("\n")
    }

    fn wait_for_workflow(&self, sha: &str) -> Result<(), Stop> {
        let started = Instant::now();
        let mut last_status = String::new();
        // One wait from start to finish, so the timer carries on as the workflow's status changes.
        let first = Wait::new("Waiting for the release workflow to start");
        let since = first.since.clone();
        self.update(|record| record.waiting_on = Some(first));
        loop {
            match self.env.forge.workflow_run(&self.worktree, RELEASE_WORKFLOW, sha)? {
                None if started.elapsed() > WORKFLOW_APPEAR_TIMEOUT => {
                    return Err(Stop::Failed(format!("{RELEASE_WORKFLOW} never started for {sha}")));
                }
                None => {}
                Some(run) => {
                    if run.status != last_status {
                        self.log(&format!("Release workflow: {} {}", run.status, run.url));
                        last_status = run.status.clone();
                        self.update(|record| {
                            record.waiting_on = Some(Wait {
                                label: format!("Waiting for the release workflow: {}", run.status),
                                since: since.clone(),
                                url: Some(run.url.clone()),
                            });
                        });
                    }
                    if run.status == "completed" {
                        if run.conclusion == "success" {
                            return Ok(());
                        }
                        return Err(Stop::Failed(format!(
                            "The release workflow ended with {}: {}",
                            run.conclusion, run.url
                        )));
                    }
                }
            }
            if started.elapsed() > WORKFLOW_RUN_TIMEOUT {
                return Err(Stop::Failed("Gave up waiting for the release workflow".into()));
            }
            let until = Instant::now() + self.env.poll_every;
            while Instant::now() < until {
                if self.cancel.is_cancelled() {
                    return Err(Stop::Cancelled);
                }
                thread::sleep(Duration::from_millis(50).min(self.env.poll_every));
            }
        }
    }

    fn publish(&self, tag: &str) -> Result<(), Stop> {
        let wt = &self.worktree;
        let still_draft = format!("{tag} is still a draft on GitHub and can be published by hand");
        self.github().publish(wt, tag).map_err(|e| format!("{still_draft}: {e}"))?;
        let release = self.github().release(wt, tag)?.ok_or_else(|| format!("{tag} disappeared after publishing"))?;
        if release.is_draft {
            return Err(Stop::Failed(still_draft));
        }
        self.log(&format!("Published {tag} {}", release.url));
        self.update(|record| record.release_url = Some(release.url));
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

/// Only the subcommand: the full arguments are noise here, and they're in the log anyway.
fn git_wait_label(args: &[&str]) -> String {
    format!("Waiting for `git {}`", args.first().copied().unwrap_or_default())
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

fn agent_wait_label(role: &Role, attempt: u32, max_attempts: u32) -> String {
    let (agent, retried) = match role {
        Role::Grill => ("grill", false),
        Role::Build => ("build", true),
        Role::Makefile => ("Makefile", true),
        Role::Summarize => ("release notes", false),
    };
    let mut label = format!("Waiting for the {agent} agent");
    if retried && max_attempts > 1 {
        label.push_str(&format!(" (attempt {attempt} of {max_attempts})"));
    }
    label
}

fn parse_grill_turn(result: &AgentResult) -> GrillTurn {
    let parsed = result.structured.clone().and_then(|value| serde_json::from_value(value).ok());
    // If the structured reply went missing, the text is still something to put to the user.
    let mut turn: GrillTurn = parsed.unwrap_or_else(|| GrillTurn {
        questions: Some(result.summary.trim()).filter(|s| !s.is_empty()).map(Question::plain).into_iter().collect(),
        ..Default::default()
    });
    turn.questions.iter_mut().for_each(Question::normalise);
    turn
}

fn has_release_workflow(dir: &Path) -> bool {
    let path = dir.join(".github").join("workflows").join(RELEASE_WORKFLOW);
    fs::read_to_string(path).is_ok_and(|text| text.contains("branches: [release]") || text.contains("- release"))
}

fn only_bullets(text: &str) -> String {
    let bullets: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- ") || line.starts_with("* "))
        .collect();
    bullets.join("\n")
}

/// Removes a run's worktree and local branch. Anything already pushed is untouched.
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

/// Makes `git push` fail inside an agent's process tree by rewriting push URLs to nowhere.
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

fn initial_build_prompt(task: &str, brief: Option<&str>, transcript: &[Exchange]) -> String {
    let mut prompt = format!("Task from the user:\n{task}\n");
    if let Some(brief) = brief {
        prompt.push_str(&format!("\nAgreed solution:\n{brief}\n"));
    }
    if !transcript.is_empty() {
        prompt.push_str("\nHow that was agreed:\n");
        for exchange in transcript {
            for question in &exchange.questions {
                prompt.push_str(&format!("Q: {question}\n"));
            }
            prompt.push_str(&format!("A: {}\n", exchange.answer));
        }
    }
    prompt
}

fn feedback_prompt(build_prompt: &str, tail: &str) -> String {
    format!(
        "The factory ran `{TEST_COMMAND}` on your changes and it failed. Fix the code so it passes.\n\n\
         {build_prompt}\nEnd of the test output:\n```\n{tail}\n```"
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

fn commit_message(title: &str, summary: &str, run_id: &str) -> String {
    let mut message = subject_line(title);
    message.push_str("\n\n");
    let summary = summary.trim();
    if !summary.is_empty() {
        message.extend(summary.chars().take(2000));
        message.push_str("\n\n");
    }
    message.push_str(&format!("Factory-Run: {run_id}\nCo-Authored-By: Claude <noreply@anthropic.com>\n"));
    message
}

pub fn save(paths: &Paths, record: &RunRecord) -> Result<(), String> {
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
            record.questions = None;
            record.waiting_on = None;
            let _ = save(paths, record);
        }
    }
    records.sort_by(|a, b| b.id.cmp(&a.id));
    records
}

pub fn read_log(paths: &Paths, id: &str) -> Vec<String> {
    fs::read_to_string(paths.runs_dir().join(id).join("log.txt"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{Release, WorkflowRun};
    use serde_json::{Value, json};
    use std::collections::HashMap;

    type Script = Box<dyn Fn(&AgentRequest, usize) -> Result<(), String> + Send + Sync>;
    type GrillScript = Box<dyn Fn(usize) -> Value + Send + Sync>;

    fn ready() -> Value {
        json!({"status": "ready", "title": "Fix the bug", "brief": "Do the agreed thing.", "bump": "patch"})
    }

    /// Stands in for Claude: runs closures in the worktree and records its prompts.
    struct FakeAgent {
        builds: Mutex<Vec<(String, Option<String>)>>,
        grills: Mutex<Vec<(String, Option<String>)>>,
        script: Script,
        grill: GrillScript,
        summary: Option<String>,
        reports_tests: bool,
    }

    impl FakeAgent {
        fn new(script: Script) -> Self {
            Self {
                builds: Mutex::new(Vec::new()),
                grills: Mutex::new(Vec::new()),
                script,
                grill: Box::new(|_| ready()),
                summary: Some("Intro line\n- Fixed the bug\n- Added a test".to_string()),
                reports_tests: true,
            }
        }
    }

    impl Agent for FakeAgent {
        fn run(
            &self,
            request: AgentRequest,
            _cancel: &Cancel,
            _log: &mut dyn FnMut(&str),
        ) -> Result<Option<AgentResult>, String> {
            let session_id = Some("session-1".to_string());
            match request.role {
                Role::Grill => {
                    let turn = {
                        let mut grills = self.grills.lock().unwrap();
                        grills.push((request.prompt.clone(), request.resume.clone()));
                        grills.len()
                    };
                    let structured = Some((self.grill)(turn));
                    Ok(Some(AgentResult { session_id, summary: String::new(), structured }))
                }
                Role::Summarize => match &self.summary {
                    Some(summary) => {
                        Ok(Some(AgentResult { session_id, summary: summary.clone(), structured: None }))
                    }
                    None => Err("summary unavailable".to_string()),
                },
                Role::Build | Role::Makefile => {
                    let attempt = {
                        let mut builds = self.builds.lock().unwrap();
                        builds.push((request.prompt.clone(), request.resume.clone()));
                        builds.len()
                    };
                    (self.script)(&request, attempt)?;
                    let structured = (request.role == Role::Makefile).then(|| json!({"has_tests": self.reports_tests}));
                    Ok(Some(AgentResult { session_id, summary: "Did the thing.".into(), structured }))
                }
            }
        }
    }

    /// Stands in for GitHub. `workflow` scripts what a release workflow does when polled.
    #[derive(Default)]
    struct FakeForge {
        not_ready: bool,
        releases: Mutex<HashMap<String, Release>>,
        notes: Mutex<HashMap<String, String>>,
        targets: Mutex<HashMap<String, String>>,
        polls: Mutex<usize>,
        workflow_conclusion: Option<String>,
        workflow_draft: Option<String>,
        /// What the workflow reports between appearing and completing. Empty means `in_progress`.
        workflow_statuses: Vec<&'static str>,
        publish_fails: bool,
    }

    impl Forge for FakeForge {
        fn ready(&self, _dir: &Path) -> Result<(), String> {
            if self.not_ready { Err("not logged in".into()) } else { Ok(()) }
        }

        fn workflow_run(&self, _dir: &Path, workflow: &str, _sha: &str) -> Result<Option<WorkflowRun>, String> {
            assert_eq!(workflow, "release.yml");
            let mut polls = self.polls.lock().unwrap();
            *polls += 1;
            let run = |status: &str, conclusion: &str| WorkflowRun {
                status: status.into(),
                conclusion: conclusion.into(),
                url: "https://example.test/run".into(),
            };
            let statuses: &[&str] = if self.workflow_statuses.is_empty() { &["in_progress"] } else { &self.workflow_statuses };
            Ok(match *polls {
                1 => None,
                poll if poll - 2 < statuses.len() => Some(run(statuses[poll - 2], "")),
                _ => {
                    let conclusion = self.workflow_conclusion.clone().unwrap_or("success".to_string());
                    if conclusion == "success" {
                        if let Some(tag) = &self.workflow_draft {
                            let draft = Release { is_draft: true, url: format!("https://example.test/{tag}") };
                            self.releases.lock().unwrap().insert(tag.clone(), draft);
                        }
                    }
                    Some(run("completed", &conclusion))
                }
            })
        }

        fn release(&self, _dir: &Path, tag: &str) -> Result<Option<Release>, String> {
            Ok(self.releases.lock().unwrap().get(tag).cloned())
        }

        fn create_draft(&self, _dir: &Path, tag: &str, _title: &str, sha: &str, notes: &str) -> Result<(), String> {
            let draft = Release { is_draft: true, url: format!("https://example.test/{tag}") };
            self.releases.lock().unwrap().insert(tag.to_string(), draft);
            self.notes.lock().unwrap().insert(tag.to_string(), notes.to_string());
            self.targets.lock().unwrap().insert(tag.to_string(), sha.to_string());
            Ok(())
        }

        fn set_notes(&self, _dir: &Path, tag: &str, notes: &str) -> Result<(), String> {
            self.notes.lock().unwrap().insert(tag.to_string(), notes.to_string());
            Ok(())
        }

        fn publish(&self, _dir: &Path, tag: &str) -> Result<(), String> {
            if self.publish_fails {
                return Err("HTTP 500".into());
            }
            self.releases.lock().unwrap().get_mut(tag).ok_or("no such release")?.is_draft = false;
            Ok(())
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
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
    }

    /// `git hash-object` fails on a missing file, in both sh and cmd.exe.
    fn makefile_requiring(file: &str) -> String {
        format!("test:\n\tgit hash-object {file}\n")
    }

    fn fixture() -> Fixture {
        fixture_with(&[("Makefile", &makefile_requiring("fixed.txt"))])
    }

    fn fixture_with(files: &[(&str, &str)]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&remote).unwrap();
        sh(&remote, &["init", "--bare", "-b", "main"]);
        sh(&repo, &["init", "-b", "main"]);
        configure(&repo);
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        for (name, text) in files {
            let path = repo.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-m", "initial"]);
        sh(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
        sh(&repo, &["push", "-u", "origin", "main"]);
        let paths = Paths { config_file: dir.path().join("config.toml"), data_dir: dir.path().join("data") };
        let project = Project { name: "demo".into(), path: repo.clone(), ..Default::default() };
        Fixture { dir, repo, remote, paths, project }
    }

    fn write_fixed(request: &AgentRequest, _attempt: usize) -> Result<(), String> {
        fs::write(request.dir.join("fixed.txt"), "ok\n").map_err(|e| e.to_string())
    }

    fn env_for(fx: &Fixture, agent: Arc<FakeAgent>, forge: Arc<FakeForge>) -> Env {
        Env {
            paths: fx.paths.clone(),
            agent,
            forge,
            notify: Arc::new(|| {}),
            poll_every: Duration::from_millis(1),
        }
    }

    fn run_with(fx: &Fixture, kind: RunKind, agent: FakeAgent, forge: FakeForge) -> (RunRecord, Arc<FakeAgent>, Arc<FakeForge>) {
        let agent = Arc::new(agent);
        let forge = Arc::new(forge);
        let handle = start(fx.project.clone(), kind, env_for(fx, agent.clone(), forge.clone()));
        handle.thread.join().unwrap();
        let record = handle.state.lock().unwrap().record.clone();
        (record, agent, forge)
    }

    fn run(fx: &Fixture, script: Script) -> (RunRecord, Arc<FakeAgent>, Arc<FakeForge>) {
        run_with(fx, build("Fix"), FakeAgent::new(script), FakeForge::default())
    }

    fn build(task: &str) -> RunKind {
        RunKind::Build { task: task.into(), bump: None }
    }

    fn build_without_grill(task: &str, bump: Bump) -> RunKind {
        RunKind::Build { task: task.into(), bump: Some(bump) }
    }

    fn remote_refs(fx: &Fixture) -> String {
        sh(&fx.remote, &["for-each-ref", "--format=%(refname)"])
    }

    fn stage_names(record: &RunRecord) -> Vec<&'static str> {
        record.stages.iter().map(|s| s.stage.label()).collect()
    }

    fn wait_until(state: &Arc<Mutex<RunState>>, what: &str, done: impl Fn(&RunState) -> bool) {
        let started = Instant::now();
        while !done(&state.lock().unwrap()) {
            assert!(started.elapsed() < Duration::from_secs(30), "never saw: {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_whole_pipeline_lands_on_main_and_publishes_a_release() {
        let fx = fixture();
        sh(&fx.repo, &["tag", "v0.3.1"]);
        sh(&fx.repo, &["push", "origin", "v0.3.1"]);
        let (record, agent, forge) = run(&fx, Box::new(write_fixed));

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(
            stage_names(&record),
            ["Prepare", "Grill", "Branch", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
        assert_eq!(record.pushed_to.as_deref(), Some("origin/main"));
        assert_eq!(sh(&fx.remote, &["show", "main:fixed.txt"]).trim(), "ok");

        let message = sh(&fx.remote, &["log", "-1", "--format=%B", "main"]);
        assert!(message.starts_with("Fix the bug\n\nDid the thing."), "{message}");

        // No manifest here, so the version comes from the latest tag.
        assert_eq!(record.release_tag.as_deref(), Some("v0.3.2"));
        assert!(!forge.releases.lock().unwrap()["v0.3.2"].is_draft, "published");
        assert_eq!(forge.notes.lock().unwrap()["v0.3.2"], "- Fixed the bug\n- Added a test");
        assert_eq!(forge.targets.lock().unwrap()["v0.3.2"], sh(&fx.remote, &["rev-parse", "main"]));
        assert_eq!(record.release_url.as_deref(), Some("https://example.test/v0.3.2"));

        assert!(agent.builds.lock().unwrap()[0].0.contains("Do the agreed thing."), "brief reaches the builder");
        assert_eq!(record.worktree, None);
        assert_eq!(sh(&fx.repo, &["branch", "--list", "factory/*"]), "", "local branch is cleaned up");
        assert!(!remote_refs(&fx).contains("factory/"), "work branches stay local");
    }

    #[test]
    fn the_grill_loops_until_ready_and_hands_everything_to_the_builder() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|turn| match turn {
            1 => json!({"status": "questions", "questions": ["Which screen?", "Keep the old one?"]}),
            _ => json!({"status": "ready", "title": "Add CSV export", "brief": "Add CSV to the list view.", "bump": "minor"}),
        });
        let agent = Arc::new(agent);
        let forge = Arc::new(FakeForge::default());
        let handle = start(fx.project.clone(), build("Add export"), env_for(&fx, agent.clone(), forge.clone()));

        wait_until(&handle.state, "questions", |s| s.record.questions.is_some());
        {
            let state = handle.state.lock().unwrap();
            assert_eq!(state.record.questions.as_ref().unwrap()[1].text, "Keep the old one?");
            assert!(state.record.branch.is_none(), "no branch until the solution is settled");
            assert!(agent.builds.lock().unwrap().is_empty(), "no building during the grill");
        }
        handle.replies.send(GrillReply::Answer("The list view. Replace it.".into())).unwrap();
        handle.thread.join().unwrap();

        let state = handle.state.lock().unwrap();
        let record = &state.record;
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(record.questions, None);
        assert_eq!(record.brief.as_deref(), Some("Add CSV to the list view."));
        assert_eq!(record.transcript.len(), 1);
        assert_eq!(record.release_tag.as_deref(), Some("v0.1.0"), "the grill's minor bump is used");

        let grills = agent.grills.lock().unwrap();
        assert_eq!(grills[1], ("The list view. Replace it.".to_string(), Some("session-1".to_string())));
        let prompt = &agent.builds.lock().unwrap()[0].0;
        assert!(prompt.contains("Add CSV to the list view."));
        assert!(prompt.contains("Q: Keep the old one?\n") && prompt.contains("A: The list view. Replace it."));
        assert!(sh(&fx.remote, &["log", "-1", "--format=%s", "main"]).starts_with("Add CSV export"));
    }

    #[test]
    fn the_plan_leaves_out_the_grill_only_when_a_bump_was_chosen() {
        let labels = |project: &Project, kind: &RunKind| -> Vec<&'static str> {
            plan_for(project, kind).iter().map(|stage| stage.label()).collect()
        };
        let plain = Project::default();
        assert_eq!(
            labels(&plain, &build("Fix")),
            ["Prepare", "Grill", "Branch", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
        assert_eq!(
            labels(&plain, &build_without_grill("Fix", Bump::Patch)),
            ["Prepare", "Branch", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
        let with_setup = Project { setup_command: "npm ci".into(), ..Default::default() };
        assert_eq!(
            labels(&with_setup, &build_without_grill("Fix", Bump::Major)),
            ["Prepare", "Branch", "Setup", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
    }

    #[test]
    fn a_skipped_grill_sends_the_prompt_straight_to_the_builder_and_publishes() {
        let fx = fixture();
        let task = "Fix the crash\n\nIt happens on startup.";
        let (record, agent, forge) =
            run_with(&fx, build_without_grill(task, Bump::Minor), FakeAgent::new(Box::new(write_fixed)), FakeForge::default());

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert!(agent.grills.lock().unwrap().is_empty());
        assert_eq!(
            stage_names(&record),
            ["Prepare", "Branch", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
        assert_eq!(record.kind.title(), "Fix the crash");
        assert_eq!(record.release_tag.as_deref(), Some("v0.1.0"), "the chosen minor bump is used");
        assert!(!forge.releases.lock().unwrap()["v0.1.0"].is_draft, "published");
        assert_eq!(sh(&fx.remote, &["log", "-1", "--format=%s", "main"]).trim(), "Fix the crash");

        let builds = agent.builds.lock().unwrap();
        assert_eq!(builds[0].0, format!("Task from the user:\n{task}\n"));
        assert!(!builds[0].0.contains("Agreed solution"));

        assert_eq!(record.brief, None);
        assert_eq!(record.questions, None);
        assert!(record.transcript.is_empty());
    }

    #[test]
    fn a_skipped_grill_still_retries_when_tests_fail() {
        let fx = fixture();
        let script: Script = Box::new(|request, attempt| {
            let file = if attempt == 1 { "wrong.txt" } else { "fixed.txt" };
            fs::write(request.dir.join(file), "x\n").map_err(|e| e.to_string())
        });
        let (record, agent, _) =
            run_with(&fx, build_without_grill("Fix the crash", Bump::Patch), FakeAgent::new(script), FakeForge::default());

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(
            stage_names(&record),
            ["Prepare", "Branch", "Build", "Test", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
        assert!(agent.grills.lock().unwrap().is_empty());
        let builds = agent.builds.lock().unwrap();
        assert!(builds[1].0.contains("it failed") && builds[1].0.contains("Fix the crash"), "{}", builds[1].0);
        assert!(!builds[1].0.contains("Agreed solution"));
        assert_eq!(builds[1].1.as_deref(), Some("session-1"), "the retry resumes the session");
    }

    #[test]
    fn a_grilled_prompt_keeps_its_brief_and_transcript() {
        let transcript = [Exchange { questions: vec!["Which?".to_string()], answer: "That one".to_string() }];
        assert_eq!(
            initial_build_prompt("Task", Some("Brief"), &transcript),
            "Task from the user:\nTask\n\nAgreed solution:\nBrief\n\nHow that was agreed:\nQ: Which?\nA: That one\n"
        );
    }

    #[test]
    fn saved_build_runs_without_a_bump_load_as_grilled() {
        let old: RunKind = serde_json::from_value(json!({"Build": {"task": "x"}})).unwrap();
        assert_eq!(old, build("x"));

        let skipped = build_without_grill("x", Bump::Minor);
        let saved = serde_json::to_value(&skipped).unwrap();
        assert_eq!(serde_json::from_value::<RunKind>(saved).unwrap(), skipped);
    }

    #[test]
    fn grill_options_reach_the_record_but_only_the_text_reaches_the_transcript() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|turn| match turn {
            1 => json!({"status": "questions", "questions": [
                {"text": "Which format?", "options": [" CSV ", "", "JSON"], "recommended": 2},
                {"text": "Anything else?"},
            ]}),
            _ => ready(),
        });
        let agent = Arc::new(agent);
        let handle = start(fx.project.clone(), build("Add export"), env_for(&fx, agent.clone(), Arc::new(FakeForge::default())));

        wait_until(&handle.state, "questions", |s| s.record.questions.is_some());
        {
            let state = handle.state.lock().unwrap();
            let questions = state.record.questions.as_ref().unwrap();
            assert_eq!(questions[0].text, "Which format?");
            assert_eq!(questions[0].options, ["CSV", "JSON"]);
            assert_eq!(questions[0].recommended, Some(1));
            assert_eq!(questions[1], Question::plain("Anything else?"));
        }
        handle.replies.send(GrillReply::Answer("1. JSON\n2. (no answer)".into())).unwrap();
        handle.thread.join().unwrap();

        let state = handle.state.lock().unwrap();
        assert_eq!(state.record.status, RunStatus::Succeeded, "{:?}", state.record.error);
        assert_eq!(state.record.questions, None);
        assert_eq!(
            state.record.transcript,
            [Exchange {
                questions: vec!["Which format?".to_string(), "Anything else?".to_string()],
                answer: "1. JSON\n2. (no answer)".to_string(),
            }]
        );
        assert_eq!(agent.grills.lock().unwrap()[1].0, "1. JSON\n2. (no answer)");
    }

    #[test]
    fn a_question_loads_from_a_plain_string_or_an_object() {
        let plain: Question = serde_json::from_value(json!("Which?")).unwrap();
        assert_eq!(plain, Question { text: "Which?".into(), options: vec![], recommended: None });

        let bare: Question = serde_json::from_value(json!({"text": "Which?"})).unwrap();
        assert_eq!(bare, plain);

        let full: Question = serde_json::from_value(json!({"text": "Which?", "options": ["A", "B"], "recommended": 1})).unwrap();
        assert_eq!(full, Question { text: "Which?".into(), options: vec!["A".into(), "B".into()], recommended: Some(1) });

        let saved = serde_json::to_value(&full).unwrap();
        assert_eq!(serde_json::from_value::<Question>(saved).unwrap(), full);
    }

    #[test]
    fn saved_runs_with_plain_string_questions_still_load() {
        let fx = fixture();
        let (record, _, _) = run(&fx, Box::new(write_fixed));
        let mut old = serde_json::to_value(&record).unwrap();
        old["id"] = json!("99999999-000000-0000");
        old["status"] = serde_json::to_value(RunStatus::Running).unwrap();
        old["questions"] = json!(["old"]);
        let dir = fx.paths.runs_dir().join("99999999-000000-0000");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("run.json"), old.to_string()).unwrap();

        let loaded = load_records(&fx.paths);
        assert_eq!(loaded.len(), 2, "the old record isn't dropped");
        assert_eq!(loaded[0].id, "99999999-000000-0000");
        assert_eq!(loaded[0].status, RunStatus::Interrupted);
        assert_eq!(loaded[0].questions, None);
    }

    fn parsed_question(question: serde_json::Value) -> Question {
        let result = AgentResult { structured: Some(json!({"status": "questions", "questions": [question]})), ..Default::default() };
        parse_grill_turn(&result).questions.remove(0)
    }

    #[test]
    fn options_are_cleaned_up_when_a_turn_is_parsed() {
        let cleaned = parsed_question(json!({"text": "Q", "options": [" A ", "  ", "B"], "recommended": 0}));
        assert_eq!(cleaned.options, ["A", "B"]);
        assert_eq!(cleaned.recommended, Some(0));

        let out_of_range = parsed_question(json!({"text": "Q", "options": ["A", "B"], "recommended": 2}));
        assert_eq!(out_of_range.recommended, None);

        let out_after_dropping = parsed_question(json!({"text": "Q", "options": ["A", "", ""], "recommended": 2}));
        assert_eq!(out_after_dropping.options, ["A"]);
        assert_eq!(out_after_dropping.recommended, None);

        let shifted = parsed_question(json!({"text": "Q", "options": ["", "A", "B"], "recommended": 2}));
        assert_eq!(shifted.options[shifted.recommended.unwrap()], "B", "still recommends the same option");

        let no_options = parsed_question(json!({"text": "Q", "recommended": 0}));
        assert_eq!(no_options, Question::plain("Q"));
    }

    #[test]
    fn a_missing_structured_reply_becomes_a_question_without_options() {
        let result = AgentResult { summary: " What now? ".into(), ..Default::default() };
        assert_eq!(parse_grill_turn(&result).questions, [Question::plain("What now?")]);
        assert!(parse_grill_turn(&AgentResult::default()).questions.is_empty());
    }

    #[test]
    fn enough_tells_the_grill_to_finish() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|turn| match turn {
            1 => json!({"status": "questions", "questions": ["Anything else?"]}),
            _ => ready(),
        });
        let agent = Arc::new(agent);
        let handle = start(fx.project.clone(), build("Fix"), env_for(&fx, agent.clone(), Arc::new(FakeForge::default())));
        wait_until(&handle.state, "questions", |s| s.record.questions.is_some());
        handle.replies.send(GrillReply::Proceed).unwrap();
        handle.thread.join().unwrap();

        assert_eq!(handle.state.lock().unwrap().record.status, RunStatus::Succeeded);
        assert!(agent.grills.lock().unwrap()[1].0.contains("Stop asking"));
    }

    #[test]
    fn a_grill_that_wont_finish_after_being_told_fails() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|_| json!({"status": "questions", "questions": ["And another?"]}));
        let agent = Arc::new(agent);
        let handle = start(fx.project.clone(), build("Fix"), env_for(&fx, agent.clone(), Arc::new(FakeForge::default())));
        wait_until(&handle.state, "questions", |s| s.record.questions.is_some());
        handle.replies.send(GrillReply::Proceed).unwrap();
        handle.thread.join().unwrap();

        assert_eq!(handle.state.lock().unwrap().record.status, RunStatus::Failed);
        assert!(agent.builds.lock().unwrap().is_empty());
    }

    #[test]
    fn stopping_during_the_grill_cancels_without_building() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|_| json!({"status": "questions", "questions": ["Which?"]}));
        let agent = Arc::new(agent);
        let handle = start(fx.project.clone(), build("Fix"), env_for(&fx, agent.clone(), Arc::new(FakeForge::default())));
        wait_until(&handle.state, "questions", |s| s.record.questions.is_some());
        handle.cancel.cancel();
        handle.thread.join().unwrap();

        let state = handle.state.lock().unwrap();
        assert_eq!(state.record.status, RunStatus::Cancelled);
        assert_eq!(state.record.questions, None);
        assert!(agent.builds.lock().unwrap().is_empty());
    }

    #[test]
    fn failing_tests_go_back_to_the_agent_with_the_output() {
        let fx = fixture();
        let script: Script = Box::new(|request, attempt| {
            let file = if attempt == 1 { "wrong.txt" } else { "fixed.txt" };
            fs::write(request.dir.join(file), "x\n").map_err(|e| e.to_string())
        });
        let (record, agent, _) = run(&fx, script);

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(
            stage_names(&record),
            ["Prepare", "Grill", "Branch", "Build", "Test", "Build", "Test", "Commit", "Merge", "Release", "Publish"]
        );
        let builds = agent.builds.lock().unwrap();
        assert_eq!(builds[0].1, None, "the build agent starts fresh rather than resuming the grill");
        assert!(builds[1].0.contains("make test") && builds[1].0.contains("it failed"), "{}", builds[1].0);
        assert!(builds[1].0.contains("Do the agreed thing."));
        assert_eq!(builds[1].1.as_deref(), Some("session-1"), "the retry resumes the session");
    }

    #[test]
    fn nothing_lands_when_tests_never_pass() {
        let fx = fixture();
        let refs_before = remote_refs(&fx);
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let script: Script = Box::new(|request, _| {
            fs::write(request.dir.join("wrong.txt"), "x\n").map_err(|e| e.to_string())
        });
        let (mut record, agent, forge) = run(&fx, script);

        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(agent.builds.lock().unwrap().len(), 3, "max_attempts");
        assert!(!stage_names(&record).contains(&"Commit"));
        assert_eq!(remote_refs(&fx), refs_before);
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before);
        assert!(forge.releases.lock().unwrap().is_empty());

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
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let script: Script = Box::new(|request, attempt| {
            write_fixed(request, attempt)?;
            git(request.dir, &["add", "."])?;
            git(request.dir, &["commit", "-m", "sneaky"])?;
            Ok(())
        });
        let (record, _, _) = run(&fx, script);
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("moved HEAD"));
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn an_agent_that_edits_the_makefile_fails_the_run() {
        let fx = fixture();
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let script: Script = Box::new(|request, _| {
            // Would otherwise turn the gate into a no-op.
            fs::write(request.dir.join("Makefile"), "test:\n\tgit --version\n").map_err(|e| e.to_string())
        });
        let (record, _, _) = run(&fx, script);
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.as_ref().unwrap().contains("Makefile"));
        assert!(!stage_names(&record).contains(&"Test"));
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before);
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
        let (record, _, _) = run(&fx, script);
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert!(!remote_refs(&fx).contains("sneaky"));
    }

    #[test]
    fn an_agent_that_changes_nothing_fails_the_run() {
        let fx = fixture();
        let (record, _, _) = run(&fx, Box::new(|_, _| Ok(())));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("without changing"));
    }

    #[test]
    fn missing_makefile_stops_before_any_agent_runs() {
        let fx = fixture_with(&[]);
        let (record, agent, _) = run(&fx, Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("Create Makefile"));
        assert!(agent.grills.lock().unwrap().is_empty());
    }

    #[test]
    fn a_makefile_without_a_test_target_stops_before_any_agent_runs() {
        let fx = fixture_with(&[("Makefile", "build:\n\tgit --version\n")]);
        let (record, agent, _) = run(&fx, Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("no `test` target"));
        assert!(agent.grills.lock().unwrap().is_empty());
    }

    #[test]
    fn no_origin_stops_before_any_agent_runs() {
        let fx = fixture();
        sh(&fx.repo, &["remote", "remove", "origin"]);
        let (record, agent, _) = run(&fx, Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("origin"));
        assert!(agent.grills.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unready_github_cli_stops_before_any_agent_runs() {
        let fx = fixture();
        let forge = FakeForge { not_ready: true, ..Default::default() };
        let (record, agent, _) = run_with(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), forge);
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("GitHub CLI"));
        assert!(agent.grills.lock().unwrap().is_empty());
    }

    #[test]
    fn moved_main_is_rebased_and_retested_before_pushing() {
        let fx = fixture();
        let remote = fx.remote.clone();
        let other = fx.dir.path().join("other");
        let script: Script = Box::new(move |request, attempt| {
            sh(request.dir.parent().unwrap(), &["clone", &remote.to_string_lossy(), &other.to_string_lossy()]);
            configure(&other);
            fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
            sh(&other, &["add", "."]);
            sh(&other, &["commit", "-m", "someone else's work"]);
            sh(&other, &["push", "origin", "main"]);
            write_fixed(request, attempt)
        });
        let (record, _, _) = run(&fx, script);

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(
            stage_names(&record),
            ["Prepare", "Grill", "Branch", "Build", "Test", "Commit", "Merge", "Test", "Merge", "Release", "Publish"]
        );
        assert_eq!(sh(&fx.remote, &["show", "main:theirs.txt"]).trim(), "theirs");
        assert_eq!(sh(&fx.remote, &["show", "main:fixed.txt"]).trim(), "ok");
    }

    const CARGO_TOML: &str = "[package]\nname = \"demo\"\nversion = \"0.3.1\"\nedition = \"2021\"\n\n[dependencies]\n";
    const CARGO_LOCK: &str = "version = 4\n\n[[package]]\nname = \"demo\"\nversion = \"0.3.1\"\n";
    const WORKFLOW: &str = "on:\n  push:\n    branches: [release]\n";

    #[test]
    fn this_repos_own_release_workflow_is_detected() {
        assert!(has_release_workflow(Path::new(env!("CARGO_MANIFEST_DIR"))));
    }

    fn ci_project() -> Fixture {
        fixture_with(&[
            ("Makefile", &makefile_requiring("fixed.txt")),
            ("Cargo.toml", CARGO_TOML),
            ("Cargo.lock", CARGO_LOCK),
            ("src/main.rs", "fn main() {}\n"),
            (".github/workflows/release.yml", WORKFLOW),
        ])
    }

    #[test]
    fn ci_projects_release_by_pushing_the_release_branch_and_waiting_for_the_draft() {
        let fx = ci_project();
        let forge = FakeForge { workflow_draft: Some("v0.3.2".into()), ..Default::default() };
        let (record, _, forge) = run_with(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), forge);

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(record.release_tag.as_deref(), Some("v0.3.2"));

        // The version bump lands on both branches, lockfile included, as a release commit.
        assert_eq!(sh(&fx.remote, &["log", "-1", "--format=%s", "main"]), "Release 0.3.2");
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), sh(&fx.remote, &["rev-parse", "release"]));
        assert!(sh(&fx.remote, &["show", "main:Cargo.toml"]).contains("version = \"0.3.2\""));
        assert!(sh(&fx.remote, &["show", "main:Cargo.lock"]).contains("version = \"0.3.2\""));
        assert_eq!(sh(&fx.remote, &["log", "-1", "--format=%s", "main~1"]), "Fix the bug");

        assert!(*forge.polls.lock().unwrap() >= 3, "waited for the workflow");
        let release = forge.releases.lock().unwrap()["v0.3.2"].clone();
        assert!(!release.is_draft, "promoted from draft");
        assert_eq!(forge.notes.lock().unwrap()["v0.3.2"], "- Fixed the bug\n- Added a test");
    }

    #[test]
    fn a_failed_release_workflow_stops_before_publishing() {
        let fx = ci_project();
        let forge = FakeForge {
            workflow_conclusion: Some("failure".into()),
            workflow_draft: Some("v0.3.2".into()),
            ..Default::default()
        };
        let (record, _, forge) = run_with(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), forge);

        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.as_ref().unwrap().contains("failure"));
        assert!(!stage_names(&record).contains(&"Publish"));
        assert!(forge.releases.lock().unwrap().is_empty());
        assert_eq!(sh(&fx.remote, &["show", "main:fixed.txt"]).trim(), "ok", "merged work stays merged");
    }

    #[test]
    fn a_workflow_that_leaves_no_draft_fails() {
        let fx = ci_project();
        let (record, _, _) = run_with(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), FakeForge::default());
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("no draft"));
    }

    #[test]
    fn a_failed_publish_says_the_draft_is_still_there() {
        let fx = fixture();
        let forge = FakeForge { publish_fails: true, ..Default::default() };
        let (record, _, forge) = run_with(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), forge);
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("still a draft"));
        assert!(forge.releases.lock().unwrap()["v0.0.1"].is_draft);
    }

    #[test]
    fn notes_fall_back_to_commit_subjects_when_summarizing_fails() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.summary = None;
        let (record, _, forge) = run_with(&fx, build("Fix"), agent, FakeForge::default());
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert!(forge.notes.lock().unwrap()["v0.0.1"].contains("- Fix the bug"));
    }

    #[test]
    fn an_existing_release_is_refused() {
        let fx = fixture();
        let forge = FakeForge::default();
        let existing = Release { is_draft: true, url: String::new() };
        forge.releases.lock().unwrap().insert("v0.0.1".into(), existing);
        let (record, _, _) = run_with(&fx, RunKind::Release { bump: Bump::Patch }, FakeAgent::new(Box::new(write_fixed)), forge);
        assert_eq!(record.status, RunStatus::Failed);
    }

    #[test]
    fn manual_release_tests_then_publishes_without_any_agent_code() {
        let fx = fixture_with(&[("Makefile", &makefile_requiring("README.md"))]);
        let (record, agent, forge) =
            run_with(&fx, RunKind::Release { bump: Bump::Minor }, FakeAgent::new(Box::new(write_fixed)), FakeForge::default());
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Test", "Release", "Publish"]);
        assert!(agent.builds.lock().unwrap().is_empty());
        assert!(!forge.releases.lock().unwrap()["v0.1.0"].is_draft);
    }

    #[test]
    fn manual_release_stops_at_failing_tests() {
        let fx = fixture();
        let (record, _, forge) =
            run_with(&fx, RunKind::Release { bump: Bump::Patch }, FakeAgent::new(Box::new(write_fixed)), FakeForge::default());
        assert_eq!(record.status, RunStatus::Failed);
        assert!(forge.releases.lock().unwrap().is_empty());
    }

    fn write_makefile(request: &AgentRequest, required_file: &str) -> Result<(), String> {
        fs::write(request.dir.join("Makefile"), makefile_requiring(required_file)).map_err(|e| e.to_string())
    }

    fn create_makefile_run(fx: &Fixture, agent: FakeAgent) -> (RunRecord, Arc<FakeAgent>) {
        // No GitHub access is needed, so an unready forge mustn't get in the way.
        let forge = FakeForge { not_ready: true, ..Default::default() };
        let (record, agent, _) = run_with(fx, RunKind::CreateMakefile, agent, forge);
        (record, agent)
    }

    #[test]
    fn an_agents_makefile_is_tested_then_pushed_to_main() {
        let fx = fixture_with(&[]);
        let script: Script = Box::new(|request, _| write_makefile(request, "README.md"));
        let (record, agent) = create_makefile_run(&fx, FakeAgent::new(script));

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Build", "Test", "Commit", "Merge"]);
        assert!(sh(&fx.remote, &["show", "main:Makefile"]).contains("git hash-object README.md"));
        assert_eq!(sh(&fx.remote, &["log", "-1", "--format=%s", "main"]), "Add Makefile");
        assert_eq!(record.note, None);
        assert!(agent.grills.lock().unwrap().is_empty(), "nothing to grill the user about");
    }

    #[test]
    fn a_failing_makefile_goes_back_to_the_agent_and_nothing_lands_until_it_passes() {
        let fx = fixture_with(&[]);
        let script: Script = Box::new(|request, attempt| {
            write_makefile(request, if attempt == 1 { "missing.txt" } else { "README.md" })
        });
        let (record, agent) = create_makefile_run(&fx, FakeAgent::new(script));

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(stage_names(&record), ["Prepare", "Build", "Test", "Build", "Test", "Commit", "Merge"]);
        let builds = agent.builds.lock().unwrap();
        assert!(builds[1].0.contains("make test") && builds[1].0.contains("failed"), "{}", builds[1].0);
        assert_eq!(builds[1].1.as_deref(), Some("session-1"));
        assert!(sh(&fx.remote, &["show", "main:Makefile"]).contains("README.md"));
    }

    #[test]
    fn a_makefile_that_never_passes_doesnt_land() {
        let fx = fixture_with(&[]);
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let script: Script = Box::new(|request, _| write_makefile(request, "missing.txt"));
        let (record, agent) = create_makefile_run(&fx, FakeAgent::new(script));
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(agent.builds.lock().unwrap().len(), 3);
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn a_project_with_no_tests_gets_a_failing_gate_and_a_note() {
        let fx = fixture_with(&[]);
        let mut agent = FakeAgent::new(Box::new(|request, _| write_makefile(request, "no-tests-yet")));
        agent.reports_tests = false;
        let (record, _) = create_makefile_run(&fx, agent);
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert!(record.note.unwrap().contains("no tests"));
        assert!(sh(&fx.remote, &["show", "main:Makefile"]).contains("no-tests-yet"));
    }

    #[test]
    fn claiming_no_tests_cant_land_a_gate_that_passes() {
        let fx = fixture_with(&[]);
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let mut agent = FakeAgent::new(Box::new(|request, _| write_makefile(request, "README.md")));
        agent.reports_tests = false;
        let (record, agent) = create_makefile_run(&fx, agent);
        assert_eq!(record.status, RunStatus::Failed);
        assert!(agent.builds.lock().unwrap()[1].0.contains("must exit non-zero"));
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn a_makefile_agent_can_only_touch_the_makefile() {
        let fx = fixture_with(&[]);
        let main_before = sh(&fx.remote, &["rev-parse", "main"]);
        let script: Script = Box::new(|request, _| {
            write_makefile(request, "README.md")?;
            fs::write(request.dir.join("README.md"), "changed\n").map_err(|e| e.to_string())
        });
        let (record, _) = create_makefile_run(&fx, FakeAgent::new(script));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("more than the Makefile"));
        assert_eq!(sh(&fx.remote, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn create_makefile_wont_overwrite_one_or_start_an_agent() {
        let fx = fixture();
        let original = sh(&fx.remote, &["show", "main:Makefile"]);
        let (record, agent) = create_makefile_run(&fx, FakeAgent::new(Box::new(write_fixed)));
        assert_eq!(record.status, RunStatus::Failed);
        assert!(agent.builds.lock().unwrap().is_empty());
        assert_eq!(sh(&fx.remote, &["show", "main:Makefile"]), original);
    }

    /// Pushes a commit to the remote from another clone, so the fixture's checkout falls behind.
    fn push_from_elsewhere(fx: &Fixture, file: &str) -> String {
        let other = fx.dir.path().join(format!("clone-{file}"));
        sh(fx.dir.path(), &["clone", &fx.remote.to_string_lossy(), &other.to_string_lossy()]);
        configure(&other);
        fs::write(other.join(file), "theirs\n").unwrap();
        sh(&other, &["add", "."]);
        sh(&other, &["commit", "-m", "someone else's work"]);
        sh(&other, &["push", "origin", "main"]);
        sh(&other, &["rev-parse", "HEAD"])
    }

    fn logged(fx: &Fixture, record: &RunRecord, text: &str) -> bool {
        read_log(&fx.paths, &record.id).iter().any(|line| line.contains(text))
    }

    /// Fails the run right after Prepare, which is all these tests need to see.
    fn stop_after_pull() -> FakeAgent {
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|_| json!({"status": "ready"}));
        agent
    }

    #[test]
    fn the_first_step_pulls_the_latest_main_into_the_checkout_and_the_run() {
        let fx = fixture();
        let tip = push_from_elsewhere(&fx, "theirs.txt");
        let script: Script = Box::new(|request, attempt| {
            assert!(request.dir.join("theirs.txt").exists(), "the run starts from the latest main");
            write_fixed(request, attempt)
        });
        let (record, _, _) = run(&fx, script);
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert!(fx.repo.join("theirs.txt").exists(), "the checkout is pulled too");
        assert!(sh(&fx.repo, &["merge-base", "--is-ancestor", &tip, "main"]).is_empty());
        assert!(logged(&fx, &record, "Local main is up to date"));
    }

    #[test]
    fn pulling_updates_main_even_with_another_branch_checked_out() {
        let fx = fixture();
        sh(&fx.repo, &["switch", "--create", "my-work"]);
        let tip = push_from_elsewhere(&fx, "theirs.txt");
        let (record, _, _) = run_with(&fx, build("Fix"), stop_after_pull(), FakeForge::default());
        assert_eq!(sh(&fx.repo, &["symbolic-ref", "--short", "HEAD"]), "my-work", "{:?}", record.error);
        assert_eq!(sh(&fx.repo, &["rev-parse", "main"]), tip);
        assert!(!fx.repo.join("theirs.txt").exists(), "the checked-out branch is left alone");
    }

    #[test]
    fn local_commits_on_main_are_kept_and_the_run_still_starts_from_origin() {
        let fx = fixture();
        fs::write(fx.repo.join("mine.txt"), "mine\n").unwrap();
        sh(&fx.repo, &["add", "."]);
        sh(&fx.repo, &["commit", "-m", "unpushed work"]);
        let mine = sh(&fx.repo, &["rev-parse", "main"]);
        push_from_elsewhere(&fx, "theirs.txt");

        let script: Script = Box::new(|request, attempt| {
            assert!(request.dir.join("theirs.txt").exists());
            assert!(!request.dir.join("mine.txt").exists(), "unpushed work stays out of the run");
            write_fixed(request, attempt)
        });
        let (record, _, _) = run(&fx, script);
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(sh(&fx.repo, &["rev-parse", "main"]), mine, "local work is untouched");
        assert!(logged(&fx, &record, "WARNING: could not update local main"));
    }

    #[test]
    fn uncommitted_edits_that_pulling_would_overwrite_are_kept() {
        let fx = fixture();
        push_from_elsewhere(&fx, "theirs.txt");
        fs::write(fx.repo.join("theirs.txt"), "my uncommitted version\n").unwrap();
        let (record, _, _) = run(&fx, Box::new(write_fixed));
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(fs::read_to_string(fx.repo.join("theirs.txt")).unwrap(), "my uncommitted version\n");
        assert!(logged(&fx, &record, "WARNING: could not update local main"));
    }

    #[test]
    fn cancelling_kills_a_running_test_command() {
        let recipe = if cfg!(windows) { "test:\n\tping -n 60 127.0.0.1\n" } else { "test:\n\tsleep 60\n" };
        let fx = fixture_with(&[("Makefile", recipe)]);
        let agent = Arc::new(FakeAgent::new(Box::new(write_fixed)));
        let env = env_for(&fx, agent, Arc::new(FakeForge::default()));
        let handle = start(fx.project.clone(), build("Fix"), env);

        let started = Instant::now();
        wait_until(&handle.state, "the test stage", |s| s.log.iter().any(|line| line.contains("Test #1")));
        handle.cancel.cancel();
        handle.thread.join().unwrap();

        assert!(started.elapsed() < Duration::from_secs(45));
        let state = handle.state.lock().unwrap();
        assert_eq!(state.record.status, RunStatus::Cancelled);
        assert_eq!(state.record.stages.last().unwrap().status, StageStatus::Cancelled);
    }

    /// Spends real tokens, so it only runs with `cargo test -- --ignored`. GitHub is still faked.
    #[test]
    #[ignore]
    fn real_claude_runs_the_whole_pipeline() {
        let mut fx = fixture();
        fx.project.model = "haiku".into();
        let program = crate::agent::locate_claude(None).expect("claude binary");
        let forge = Arc::new(FakeForge::default());
        let env = Env {
            paths: fx.paths.clone(),
            agent: Arc::new(crate::agent::ClaudeCli { program }),
            forge: forge.clone(),
            notify: Arc::new(|| {}),
            poll_every: Duration::from_millis(1),
        };
        let task = "Create a file named fixed.txt in the current directory containing the word ok.";
        let handle = start(fx.project.clone(), build(task), env);

        // Answer the grill's first round, then tell it to get on with it.
        let mut answered = 0;
        while !handle.thread.is_finished() {
            let waiting = handle.state.lock().unwrap().record.questions.is_some();
            let rounds = handle.state.lock().unwrap().record.transcript.len();
            if waiting && rounds == answered {
                let reply = if answered == 0 {
                    GrillReply::Answer("Exactly as stated: only that one file, with just the word ok.".into())
                } else {
                    GrillReply::Proceed
                };
                handle.replies.send(reply).unwrap();
                answered += 1;
            }
            thread::sleep(Duration::from_millis(100));
        }
        handle.thread.join().unwrap();

        let state = handle.state.lock().unwrap();
        let record = &state.record;
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}\n{}", record.error, state.log.join("\n"));
        assert!(sh(&fx.remote, &["show", "main:fixed.txt"]).contains("ok"));
        println!("{}", state.log.join("\n"));
        println!("--- brief\n{}", record.brief.as_deref().unwrap_or(""));
        println!("--- commit\n{}", sh(&fx.remote, &["log", "-1", "--format=%B", "main"]));
        println!("--- notes\n{}", record.release_notes.as_deref().unwrap_or(""));
    }

    #[test]
    fn runs_are_saved_and_stale_running_ones_load_as_interrupted() {
        let fx = fixture();
        let (record, _, _) = run(&fx, Box::new(write_fixed));
        let mut stale = record.clone();
        stale.id = "99999999-000000-0000".into();
        stale.status = RunStatus::Running;
        stale.questions = Some(vec![Question::plain("Still there?")]);
        save(&fx.paths, &stale).unwrap();

        let loaded = load_records(&fx.paths);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, stale.id, "newest first");
        assert_eq!(loaded[0].status, RunStatus::Interrupted);
        assert_eq!(loaded[0].questions, None);
        assert_eq!(loaded[1].status, RunStatus::Succeeded);
        assert!(read_log(&fx.paths, &record.id).iter().any(|line| line.contains("Commit #1")));
    }

    /// An env that records every value `waiting_on` takes, in order. It reads them from `run.json`
    /// each time the run notifies, on the run's own thread, so none can be missed.
    fn recording_env(fx: &Fixture, agent: FakeAgent, forge: FakeForge) -> (Env, Arc<Mutex<Vec<Option<Wait>>>>) {
        let seen: Arc<Mutex<Vec<Option<Wait>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut env = env_for(fx, Arc::new(agent), Arc::new(forge));
        let (recorder, runs_dir) = (seen.clone(), fx.paths.runs_dir());
        env.notify = Arc::new(move || {
            for entry in fs::read_dir(&runs_dir).unwrap().flatten() {
                let Ok(text) = fs::read_to_string(entry.path().join("run.json")) else { continue };
                let wait = serde_json::from_str::<RunRecord>(&text).unwrap().waiting_on;
                let mut seen = recorder.lock().unwrap();
                if seen.last() != Some(&wait) {
                    seen.push(wait);
                }
            }
        });
        (env, seen)
    }

    fn run_recording_waits(fx: &Fixture, kind: RunKind, agent: FakeAgent, forge: FakeForge) -> (RunRecord, Vec<Option<Wait>>) {
        let (env, seen) = recording_env(fx, agent, forge);
        let handle = start(fx.project.clone(), kind, env);
        handle.thread.join().unwrap();
        let record = handle.state.lock().unwrap().record.clone();
        let seen = seen.lock().unwrap().clone();
        (record, seen)
    }

    /// The labels in order, without the gaps between stages or repeats of the same label.
    fn wait_labels(waits: &[Option<Wait>]) -> Vec<String> {
        let mut labels: Vec<String> = waits.iter().flatten().map(|wait| wait.label.clone()).collect();
        labels.dedup();
        labels
    }

    fn wait_labelled<'a>(waits: &'a [Option<Wait>], label: &str) -> &'a Wait {
        waits.iter().flatten().find(|wait| wait.label == label).unwrap_or_else(|| panic!("never waited on: {label}"))
    }

    #[test]
    fn every_step_of_a_run_names_what_it_waits_on() {
        let fx = ci_project();
        let forge = FakeForge { workflow_draft: Some("v0.3.2".into()), ..Default::default() };
        let (record, waits) = run_recording_waits(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), forge);

        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        assert_eq!(
            wait_labels(&waits),
            [
                "Waiting for the GitHub CLI check",
                "Waiting for `git fetch`",
                "Waiting for `git worktree`",
                "Waiting for `make -n test`",
                "Waiting for the grill agent",
                "Waiting for `git switch`",
                "Waiting for the build agent (attempt 1 of 3)",
                "Waiting for `make test`",
                "Waiting for `git add`",
                "Waiting for `git commit`",
                "Waiting for `git fetch`",
                "Waiting for `git push`",
                "Waiting for GitHub",
                "Waiting for the release notes agent",
                "Waiting for `cargo update --workspace --offline`",
                "Waiting for `git add`",
                "Waiting for `git commit`",
                "Waiting for `git push`",
                "Waiting for the release workflow to start",
                "Waiting for the release workflow: in_progress",
                "Waiting for the release workflow: completed",
                "Waiting for GitHub",
            ]
        );
    }

    #[test]
    fn a_new_wait_restarts_the_timer_and_a_finished_stage_clears_the_wait() {
        let fx = fixture();
        let (record, waits) = run_recording_waits(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), FakeForge::default());
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);

        let fetch = wait_labelled(&waits, "Waiting for `git fetch`");
        let worktree = wait_labelled(&waits, "Waiting for `git worktree`");
        assert_ne!(fetch.since, worktree.since);

        // Prepare's last step is followed by a gap, not by the Grill's first wait.
        let last_in_prepare = waits.iter().position(|wait| wait.as_ref().is_some_and(|w| w.label == "Waiting for `make -n test`")).unwrap();
        assert_eq!(waits[last_in_prepare + 1], None);
    }

    #[test]
    fn only_a_multi_line_commands_first_line_is_named() {
        let mut fx = fixture();
        fx.project.setup_command = "git --version\ngit --help".into();
        let (_, waits) = run_recording_waits(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), FakeForge::default());
        wait_labelled(&waits, "Waiting for `git --version`");
    }

    #[test]
    fn agent_labels_only_show_the_attempt_for_roles_that_retry() {
        assert_eq!(agent_wait_label(&Role::Grill, 1, 3), "Waiting for the grill agent");
        assert_eq!(agent_wait_label(&Role::Summarize, 1, 3), "Waiting for the release notes agent");
        assert_eq!(agent_wait_label(&Role::Build, 2, 3), "Waiting for the build agent (attempt 2 of 3)");
        assert_eq!(agent_wait_label(&Role::Makefile, 1, 2), "Waiting for the Makefile agent (attempt 1 of 2)");
        assert_eq!(agent_wait_label(&Role::Build, 1, 1), "Waiting for the build agent");
        assert_eq!(agent_wait_label(&Role::Makefile, 1, 1), "Waiting for the Makefile agent");
    }

    #[test]
    fn a_retried_build_shows_its_attempt_in_the_wait() {
        let fx = fixture();
        let script: Script = Box::new(|request, attempt| {
            let file = if attempt == 1 { "wrong.txt" } else { "fixed.txt" };
            fs::write(request.dir.join(file), "x\n").map_err(|e| e.to_string())
        });
        let (record, waits) = run_recording_waits(&fx, build("Fix"), FakeAgent::new(script), FakeForge::default());
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);
        wait_labelled(&waits, "Waiting for the build agent (attempt 1 of 3)");
        wait_labelled(&waits, "Waiting for the build agent (attempt 2 of 3)");
    }

    #[test]
    fn a_single_attempt_build_doesnt_mention_attempts() {
        let mut fx = fixture();
        fx.project.max_attempts = 1;
        let (_, waits) = run_recording_waits(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), FakeForge::default());
        wait_labelled(&waits, "Waiting for the build agent");
    }

    #[test]
    fn pending_questions_wait_on_the_user_until_the_reply() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|turn| match turn {
            1 => json!({"status": "questions", "questions": ["Which?"]}),
            _ => ready(),
        });
        let (env, seen) = recording_env(&fx, agent, FakeForge::default());
        let handle = start(fx.project.clone(), build("Fix"), env);

        // Set in one update, so there's no moment with questions and some other wait.
        wait_until(&handle.state, "questions", |s| s.record.questions.is_some());
        {
            let state = handle.state.lock().unwrap();
            assert_eq!(state.record.waiting_on.as_ref().unwrap().label, "Waiting for your answers");
        }
        handle.replies.send(GrillReply::Answer("That one".into())).unwrap();
        wait_until(&handle.state, "the reply to land", |s| !s.record.transcript.is_empty());
        {
            let state = handle.state.lock().unwrap();
            assert_eq!(state.record.questions, None);
            let label = state.record.waiting_on.as_ref().map(|wait| wait.label.as_str());
            assert_ne!(label, Some("Waiting for your answers"));
        }
        handle.thread.join().unwrap();
        assert_eq!(handle.state.lock().unwrap().record.status, RunStatus::Succeeded);

        let labels = wait_labels(&seen.lock().unwrap());
        let answers = labels.iter().position(|label| label == "Waiting for your answers").unwrap();
        assert_eq!(labels[answers + 1], "Waiting for the grill agent");
    }

    #[test]
    fn the_release_workflow_is_one_wait_with_a_link_to_the_run() {
        let fx = ci_project();
        let forge = FakeForge {
            workflow_draft: Some("v0.3.2".into()),
            workflow_statuses: vec!["queued", "queued", "in_progress"],
            ..Default::default()
        };
        let (record, waits) = run_recording_waits(&fx, build("Fix"), FakeAgent::new(Box::new(write_fixed)), forge);
        assert_eq!(record.status, RunStatus::Succeeded, "{:?}", record.error);

        let starting = wait_labelled(&waits, "Waiting for the release workflow to start");
        let queued = wait_labelled(&waits, "Waiting for the release workflow: queued");
        let in_progress = wait_labelled(&waits, "Waiting for the release workflow: in_progress");
        assert_eq!(starting.url, None);
        assert_eq!(queued.url.as_deref(), Some("https://example.test/run"));
        assert_eq!(in_progress.url.as_deref(), Some("https://example.test/run"));
        assert_eq!(starting.since, queued.since, "the status changing doesn't restart the timer");
        assert_eq!(queued.since, in_progress.since, "the status changing doesn't restart the timer");

        // What comes next is a new wait: the link goes and the timer restarts.
        let last = waits.iter().rposition(|wait| wait.as_ref().is_some_and(|w| w.url.is_some())).unwrap();
        let next = waits[last + 1].as_ref().unwrap();
        assert_eq!(next.label, "Waiting for GitHub");
        assert_eq!(next.url, None);
        assert_ne!(next.since, in_progress.since);
    }

    #[test]
    fn a_run_that_succeeded_or_failed_is_not_waiting_on_anything() {
        let (succeeded, _, _) = run(&fixture(), Box::new(write_fixed));
        assert_eq!(succeeded.status, RunStatus::Succeeded, "{:?}", succeeded.error);
        assert_eq!(succeeded.waiting_on, None);

        let (failed, _, _) = run(&fixture(), Box::new(|_, _| Ok(())));
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.waiting_on, None);
    }

    #[test]
    fn a_cancelled_run_is_not_waiting_on_anything() {
        let fx = fixture();
        let mut agent = FakeAgent::new(Box::new(write_fixed));
        agent.grill = Box::new(|_| json!({"status": "questions", "questions": ["Which?"]}));
        let handle = start(fx.project.clone(), build("Fix"), env_for(&fx, Arc::new(agent), Arc::new(FakeForge::default())));
        wait_until(&handle.state, "the wait", |s| s.record.waiting_on.as_ref().is_some_and(|w| w.label.contains("your answers")));
        handle.cancel.cancel();
        handle.thread.join().unwrap();

        let state = handle.state.lock().unwrap();
        assert_eq!(state.record.status, RunStatus::Cancelled);
        assert_eq!(state.record.waiting_on, None);
    }

    #[test]
    fn a_saved_run_without_a_wait_still_loads() {
        let fx = fixture();
        let (record, _, _) = run(&fx, Box::new(write_fixed));
        let mut old = serde_json::to_value(&record).unwrap();
        old.as_object_mut().unwrap().remove("waiting_on");
        let dir = fx.paths.runs_dir().join(&record.id);
        fs::write(dir.join("run.json"), old.to_string()).unwrap();

        let loaded = load_records(&fx.paths);
        assert_eq!(loaded.len(), 1, "the old record isn't dropped");
        assert_eq!(loaded[0].waiting_on, None);
    }

    #[test]
    fn an_interrupted_run_is_not_waiting_on_anything() {
        let fx = fixture();
        let (record, _, _) = run(&fx, Box::new(write_fixed));
        let mut stale = record.clone();
        stale.id = "99999999-000000-0000".into();
        stale.status = RunStatus::Running;
        stale.waiting_on = Some(Wait::new("Waiting for `make test`"));
        save(&fx.paths, &stale).unwrap();

        let loaded = load_records(&fx.paths);
        assert_eq!(loaded[0].id, stale.id);
        assert_eq!(loaded[0].status, RunStatus::Interrupted);
        assert_eq!(loaded[0].waiting_on, None);
        let saved = fs::read_to_string(fx.paths.runs_dir().join(&stale.id).join("run.json")).unwrap();
        assert_eq!(serde_json::from_str::<RunRecord>(&saved).unwrap().waiting_on, None, "cleared on disk too");
    }
}
