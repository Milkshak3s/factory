//! The window: a list of projects on the left, the selected project and its runs on the right.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset, Local};
use eframe::egui::{self, Color32, RichText, ScrollArea, TextEdit};

use crate::agent::{Agent, ClaudeCli, locate_claude};
use crate::config::{self, Config, Paths, Project};
use crate::forge::{Forge, GhCli, locate_gh};
use crate::git::{self, RepoStatus};
use crate::pipeline::{self, Env, GrillReply, Question, RunKind, RunRecord, RunState, RunStatus, Stage, StageStatus};
use crate::proc::{Cancel, command};
use crate::update::{self, Step, Updater};
use crate::version::Bump;
use crate::wsl::{self, DistroState, ToolCheck};

const REFRESH_EVERY: Duration = Duration::from_secs(30);
const EXIT_GRACE: Duration = Duration::from_secs(5);
const WORKFLOW_POLL: Duration = Duration::from_secs(15);
/// Room kept beside a wait's label for its timer and link, so a long command can't push them away.
const WAIT_TRAILING_WIDTH: f32 = 130.0;

const GREEN: Color32 = Color32::from_rgb(70, 170, 100);
const RED: Color32 = Color32::from_rgb(215, 85, 80);
const AMBER: Color32 = Color32::from_rgb(220, 165, 60);
const BLUE: Color32 = Color32::from_rgb(90, 150, 230);

struct RunEntry {
    state: Arc<Mutex<RunState>>,
    /// Only set while this session owns the run's thread.
    cancel: Option<Cancel>,
    replies: Option<Sender<GrillReply>>,
    thread: Option<JoinHandle<()>>,
    log_loaded: bool,
    /// The user's unsent answers to the questions waiting on this run, one per question.
    drafts: Vec<QuestionDraft>,
}

/// One question's answer so far: a picked option or typed text, never both.
#[derive(Debug, Default, PartialEq)]
struct QuestionDraft {
    selected: Option<usize>,
    other: String,
}

impl QuestionDraft {
    fn new(question: &Question) -> Self {
        QuestionDraft { selected: question.recommended, other: String::new() }
    }

    fn click_option(&mut self, option: usize) {
        if self.selected == Some(option) {
            self.selected = None;
        } else {
            self.selected = Some(option);
            self.other.clear();
        }
    }

    /// Call after `other` was edited.
    fn other_edited(&mut self) {
        if !self.other.trim().is_empty() {
            self.selected = None;
        }
    }

    fn answer<'a>(&'a self, question: &'a Question) -> Option<&'a str> {
        let other = self.other.trim();
        if other.is_empty() {
            self.selected.and_then(|index| question.options.get(index)).map(String::as_str)
        } else {
            Some(other)
        }
    }
}

/// The reply to a round of questions, or `None` if none of them has an answer yet.
fn build_reply(questions: &[Question], drafts: &[QuestionDraft]) -> Option<String> {
    let answers: Vec<Option<&str>> = questions.iter().zip(drafts).map(|(question, draft)| draft.answer(question)).collect();
    if answers.iter().all(Option::is_none) {
        return None;
    }
    let lines: Vec<String> = answers
        .iter()
        .enumerate()
        .map(|(index, answer)| format!("{}. {}", index + 1, answer.unwrap_or("(no answer)")))
        .collect();
    Some(lines.join("\n"))
}

/// What *Enough, build it* sends, given the round's reply so far.
fn enough_reply(reply: Option<String>) -> GrillReply {
    match reply {
        Some(reply) => GrillReply::Answer(format!("{reply}\n\nThat's everything from me: settle anything still open and write the brief.")),
        None => GrillReply::Proceed,
    }
}

/// Results from threads doing WSL work, which can take a distro's whole boot time.
enum WslEvent {
    State(String, DistroState),
    Tools(String, Result<ToolCheck, String>),
    /// What *Add…* found in a picked folder.
    Added(PathBuf, Vec<Project>),
    Notice(String),
}

/// What a project's blockers depend on besides the repo.
enum Tooling<'a> {
    Native {
        claude_found: bool,
        gh_status: Option<&'a Result<(), String>>,
    },
    Wsl {
        distro: &'a str,
        on_windows_drive: bool,
        state: Option<&'a DistroState>,
        tools: Option<&'a Result<ToolCheck, String>>,
    },
}

/// Whether what's known about a repo can be relied on. A WSL repo's error is usually about the
/// distro, so it says nothing about the repo: its empty status mustn't read as "no origin".
fn repo_known(status: Option<&RepoStatus>, in_wsl: bool) -> bool {
    status.is_some_and(|status| !in_wsl || status.error.is_none())
}

/// Why a project can't run right now. A stopped distro isn't a reason, since a run wakes it, and
/// the tools were last checked before it stopped, so the check at Prepare has the final word.
fn project_blocker(default_branch: &str, known: bool, status: &RepoStatus, tooling: &Tooling) -> Option<String> {
    if known && !status.has_origin {
        return Some("This repo has no origin remote, so the pipeline has nowhere to push.".to_string());
    }
    if known && !status.has_makefile {
        return Some(format!("origin/{default_branch} has no Makefile, and `make test` is what every run has to pass."));
    }
    match tooling {
        Tooling::Native { claude_found: false, .. } => {
            Some("Could not find the claude binary. Set claude_path in config.toml.".to_string())
        }
        Tooling::Native { gh_status: Some(Err(error)), .. } => {
            let reason = if error.contains("could not run") { "isn't installed" } else { "isn't logged in" };
            Some(format!("The GitHub CLI {reason}. It's needed to watch CI and publish releases."))
        }
        Tooling::Native { .. } => None,
        Tooling::Wsl { on_windows_drive: true, .. } => Some(wsl::WINDOWS_DRIVE_NOTICE.to_string()),
        Tooling::Wsl { distro, state, tools, .. } => {
            let state: Option<&DistroState> = *state;
            match state {
                Some(DistroState::Stopped) => None,
                Some(DistroState::Unavailable(_)) => state.and_then(|state| state.label(distro)),
                _ => match tools {
                    Some(Ok(check)) => check.problem(distro, true),
                    Some(Err(error)) => Some(wsl::could_not_check(distro, error)),
                    None => None,
                },
            }
        }
    }
}

/// *Create Makefile* needs an agent but, not being a full run, doesn't need `gh`.
fn can_create_makefile(tooling: &Tooling) -> bool {
    match tooling {
        Tooling::Native { claude_found, .. } => *claude_found,
        Tooling::Wsl { on_windows_drive: true, .. } | Tooling::Wsl { state: Some(DistroState::Unavailable(_)), .. } => false,
        Tooling::Wsl { state: Some(DistroState::Stopped), .. } => true,
        Tooling::Wsl { distro, tools, .. } => match tools {
            Some(Ok(check)) => check.problem(distro, false).is_none(),
            _ => true,
        },
    }
}

/// The WSL half of a status refresh. Looks at which distros are running first, so that an automatic
/// refresh leaves a stopped one stopped. Everything it decides arrives through the channels.
fn refresh_distros(
    projects: Vec<(String, PathBuf, String)>,
    manual: bool,
    status_tx: Sender<(PathBuf, RepoStatus)>,
    wsl_tx: Sender<WslEvent>,
    ctx: egui::Context,
) {
    let listing = wsl::list_running();
    let mut distros: Vec<&str> = projects.iter().map(|(distro, _, _)| distro.as_str()).collect();
    distros.sort_unstable();
    distros.dedup();
    for distro in distros {
        let state = wsl::refresh_decision(manual, &listing, distro);
        let available = state == DistroState::Available;
        let _ = wsl_tx.send(WslEvent::State(distro.to_string(), state));
        ctx.request_repaint();
        if !available {
            continue;
        }
        let in_distro: Vec<(PathBuf, String)> =
            projects.iter().filter(|(name, _, _)| name == distro).map(|(_, path, branch)| (path.clone(), branch.clone())).collect();
        let distro = distro.to_string();
        let (status_tx, wsl_tx, ctx) = (status_tx.clone(), wsl_tx.clone(), ctx.clone());
        thread::spawn(move || {
            if manual {
                wsl::refresh_env(&distro);
            }
            let _ = wsl_tx.send(WslEvent::Tools(distro.clone(), wsl::check_tools(&distro)));
            ctx.request_repaint();
            for (path, default_branch) in in_distro {
                let (tx, ctx) = (status_tx.clone(), ctx.clone());
                thread::spawn(move || {
                    let status = git::status(&path, &default_branch);
                    let _ = tx.send((path, status));
                    ctx.request_repaint();
                });
            }
        });
    }
}

pub struct App {
    paths: Paths,
    config: Config,
    claude: Option<PathBuf>,
    gh: PathBuf,
    /// `None` until the first check comes back. `Err` holds why `gh` can't be used.
    gh_status: Option<Result<(), String>>,
    gh_tx: Sender<Result<(), String>>,
    gh_rx: mpsc::Receiver<Result<(), String>>,
    selected: Option<usize>,
    task: String,
    statuses: HashMap<PathBuf, RepoStatus>,
    status_tx: Sender<(PathBuf, RepoStatus)>,
    status_rx: mpsc::Receiver<(PathBuf, RepoStatus)>,
    /// Apart from `statuses`, so a stopped distro doesn't disturb its projects' last known status.
    distro_states: HashMap<String, DistroState>,
    distro_tools: HashMap<String, Result<ToolCheck, String>>,
    wsl_tx: Sender<WslEvent>,
    wsl_rx: mpsc::Receiver<WslEvent>,
    last_refresh: Option<Instant>,
    runs: Vec<RunEntry>,
    selected_run: Option<String>,
    release_bump: Option<Bump>,
    /// The bump for a run that skips the grill. Deliberately not remembered between runs.
    build_bump: Bump,
    notice: Option<String>,
    running_last_frame: usize,
    /// Only set when running from an installed copy.
    updater: Option<Updater>,
}

fn status_color(status: RunStatus) -> Color32 {
    match status {
        RunStatus::Running => BLUE,
        RunStatus::Succeeded => GREEN,
        RunStatus::Failed => RED,
        RunStatus::Cancelled | RunStatus::Interrupted => AMBER,
    }
}

fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "Running",
        RunStatus::Succeeded => "Succeeded",
        RunStatus::Failed => "Failed",
        RunStatus::Cancelled => "Cancelled",
        RunStatus::Interrupted => "Interrupted",
    }
}

/// A run's dot: amber if it wants an answer. Waits that need nothing from the user stay blue.
fn run_dot_color(record: &RunRecord) -> Color32 {
    if record.questions.is_some() { AMBER } else { status_color(record.status) }
}

fn wait_label(record: &RunRecord) -> Option<&str> {
    record.waiting_on.as_ref().map(|wait| wait.label.as_str())
}

/// A project's dot and tooltip, given its runs newest first. Any run wanting an answer turns it
/// amber, even behind a newer run. Otherwise it follows the latest run. `None` if there are no runs.
fn project_dot(records: &[RunRecord]) -> Option<(Color32, Option<String>)> {
    let latest = records.first()?;
    Some(match records.iter().find(|record| record.questions.is_some()) {
        Some(asking) => (AMBER, wait_label(asking).map(str::to_string)),
        None => (run_dot_color(latest), wait_label(latest).map(str::to_string)),
    })
}

fn dot(ui: &mut egui::Ui, color: Color32, tooltip: Option<&str>) {
    let response = ui.label(RichText::new("●").color(color));
    if let Some(tooltip) = tooltip {
        response.on_hover_text(tooltip);
    }
}

/// How long ago `since` was, as `42s`, `3m 07s` or `1h 02m`. `None` if `since` can't be read.
fn elapsed_text(since: &str, now: DateTime<FixedOffset>) -> Option<String> {
    let since = DateTime::parse_from_rfc3339(since).ok()?;
    let seconds = now.signed_duration_since(since).num_seconds().max(0);
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60)
    })
}

/// The stages *Run pipeline* will go through, shown beside the button.
fn pipeline_hint(grill: bool, branch: &str) -> String {
    let start = if grill { "Grill → build" } else { "Build" };
    format!("{start} → {} → merge to {branch} → publish release", pipeline::TEST_COMMAND)
}

fn status_summary(status: &RepoStatus) -> String {
    if let Some(error) = &status.error {
        return error.clone();
    }
    let mut parts = vec![status.branch.clone()];
    if status.dirty > 0 {
        parts.push(format!("{} changed", status.dirty));
    }
    if status.ahead > 0 {
        parts.push(format!("{} ahead", status.ahead));
    }
    if status.behind > 0 {
        parts.push(format!("{} behind", status.behind));
    }
    if let Some(tag) = &status.latest_tag {
        parts.push(tag.clone());
    }
    if !status.last_commit_age.is_empty() {
        parts.push(status.last_commit_age.clone());
    }
    parts.join(" · ")
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let paths = Paths::locate();
        let (config, notice) = match Config::load(&paths.config_file) {
            Ok(config) => (config, None),
            Err(error) => (Config::default(), Some(format!("Could not read config: {error}"))),
        };
        let claude = locate_claude(config.claude_path.as_deref());
        let runs = pipeline::load_records(&paths)
            .into_iter()
            .map(|record| RunEntry {
                state: Arc::new(Mutex::new(RunState { record, log: Vec::new() })),
                cancel: None,
                replies: None,
                thread: None,
                log_loaded: false,
                drafts: Vec::new(),
            })
            .collect();
        let (status_tx, status_rx) = mpsc::channel();
        let (gh_tx, gh_rx) = mpsc::channel();
        let (wsl_tx, wsl_rx) = mpsc::channel();
        let updater = Updater::detect(&paths.data_dir);
        Self {
            paths,
            selected: (!config.projects.is_empty()).then_some(0),
            config,
            claude,
            gh: locate_gh(),
            gh_status: None,
            gh_tx,
            gh_rx,
            task: String::new(),
            statuses: HashMap::new(),
            status_tx,
            status_rx,
            distro_states: HashMap::new(),
            distro_tools: HashMap::new(),
            wsl_tx,
            wsl_rx,
            last_refresh: None,
            runs,
            selected_run: None,
            release_bump: None,
            build_bump: Bump::Patch,
            notice,
            running_last_frame: 0,
            updater,
        }
    }

    /// The bar across the top of the window, while there's a newer release to install.
    fn update_bar(&mut self, ui: &mut egui::Ui) {
        let running = self.running_count();
        let Some(updater) = &mut self.updater else { return };
        let Some(available) = updater.available.clone() else { return };
        egui::Panel::top("update_bar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(BLUE, format!("Factory v{} is available", available.version));
                match updater.step.clone() {
                    Step::Idle => {
                        if ui.button("Update now").clicked() {
                            updater.start_download(ui.ctx());
                        }
                    }
                    Step::Downloading => {
                        ui.add_enabled(false, egui::Button::new("Downloading…"));
                    }
                    Step::Confirm(package) => {
                        ui.colored_label(AMBER, update::confirm_text(running));
                        let label = if running > 0 { "Update anyway" } else { "Update now" };
                        if ui.button(label).clicked() {
                            if let Err(error) = updater.install(ui.ctx(), &package) {
                                self.notice = Some(error);
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            updater.step = Step::Idle;
                        }
                    }
                    Step::Installing => {
                        ui.add_enabled(false, egui::Button::new("Installing…"));
                    }
                }
                if !available.url.is_empty() {
                    ui.hyperlink_to("Release notes", &available.url);
                }
            });
        });
    }

    /// The row at the bottom of the Projects panel, for the installed app only.
    fn version_row(&mut self, ui: &mut egui::Ui) {
        let Some(updater) = &mut self.updater else { return };
        egui::Panel::bottom("version_row").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.small(format!("Factory v{}", update::CURRENT_VERSION));
                let checking = updater.is_checking();
                let label = if checking { "Checking…" } else { "Check for updates" };
                if ui.add_enabled(!checking, egui::Button::new(label).small()).clicked() {
                    updater.start_check(ui.ctx(), true);
                }
            });
        });
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save(&self.paths.config_file) {
            self.notice = Some(format!("Could not save config: {error}"));
        }
    }

    /// `manual` is for something the user just did, which may wake a stopped distro. The timer's
    /// refreshes never do.
    fn refresh_statuses(&mut self, ctx: &egui::Context, manual: bool) {
        self.last_refresh = Some(Instant::now());
        let mut in_distros = Vec::new();
        for project in &self.config.projects {
            let path = project.path.clone();
            let default_branch = project.default_branch.clone();
            if let Some(target) = wsl::target(&path) {
                in_distros.push((target.distro, path, default_branch));
                continue;
            }
            let tx = self.status_tx.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let status = git::status(&path, &default_branch);
                let _ = tx.send((path, status));
                ctx.request_repaint();
            });
        }
        if !in_distros.is_empty() {
            let (status_tx, wsl_tx, ctx) = (self.status_tx.clone(), self.wsl_tx.clone(), ctx.clone());
            thread::spawn(move || refresh_distros(in_distros, manual, status_tx, wsl_tx, ctx));
        }
        // Look again each time, so installing gh takes effect without restarting the app.
        self.gh = locate_gh();
        let gh = GhCli { program: self.gh.clone() };
        let tx = self.gh_tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(gh.ready(&std::env::temp_dir()));
            ctx.request_repaint();
        });
    }

    fn add_projects(&mut self, ctx: &egui::Context) {
        let Some(folder) = rfd::FileDialog::new().set_title("Add a repo, or a folder of repos").pick_folder() else {
            return;
        };
        if wsl::is_on_windows_drive(&folder) {
            self.notice = Some(wsl::WINDOWS_DRIVE_NOTICE.to_string());
            return;
        }
        if wsl::target(&folder).is_some() {
            // Reading the distro's PATH can take as long as its boot, so not on the UI thread.
            let (tx, ctx) = (self.wsl_tx.clone(), ctx.clone());
            thread::spawn(move || {
                let found = config::discover(&folder);
                let _ = tx.send(WslEvent::Added(folder, found));
                ctx.request_repaint();
            });
            return;
        }
        let found = config::discover(&folder);
        self.add_found(ctx, &folder, found);
    }

    fn add_found(&mut self, ctx: &egui::Context, folder: &std::path::Path, found: Vec<Project>) {
        if found.is_empty() {
            self.notice = Some(format!("No git repos found at {}", folder.display()));
            return;
        }
        let mut added = 0;
        for project in found {
            if !self.config.projects.iter().any(|p| wsl::same_path(&p.path, &project.path)) {
                self.config.projects.push(project);
                added += 1;
            }
        }
        self.notice = Some(format!("Added {added} project(s)"));
        self.selected = self.selected.or(Some(0));
        self.save_config();
        self.refresh_statuses(ctx, true);
    }

    fn start_run(&mut self, ctx: &egui::Context, project: Project, kind: RunKind) {
        // A distro has its own claude, so the Windows one doesn't matter to a WSL project.
        let program = match &self.claude {
            Some(program) => Some(program.clone()),
            None => wsl::target(&project.path).map(|_| PathBuf::from("claude")),
        };
        let agent: Arc<dyn Agent> = match program {
            Some(program) => Arc::new(ClaudeCli { program }),
            None => {
                self.notice = Some("Could not find the claude binary".to_string());
                return;
            }
        };
        let repaint = ctx.clone();
        let env = Env {
            paths: self.paths.clone(),
            agent,
            forge: Arc::new(GhCli { program: self.gh.clone() }),
            notify: Arc::new(move || repaint.request_repaint()),
            poll_every: WORKFLOW_POLL,
        };
        let handle = pipeline::start(project, kind, env);
        self.selected_run = Some(handle.state.lock().unwrap().record.id.clone());
        self.runs.insert(
            0,
            RunEntry {
                state: handle.state,
                cancel: Some(handle.cancel),
                replies: Some(handle.replies),
                thread: Some(handle.thread),
                log_loaded: true,
                drafts: Vec::new(),
            },
        );
    }

    fn running_count(&self) -> usize {
        self.runs.iter().filter(|r| r.state.lock().unwrap().record.status == RunStatus::Running).count()
    }

    /// The color and tooltip for a project's dot, or `None` if it has no runs yet.
    fn project_dot(&self, name: &str) -> Option<(Color32, Option<String>)> {
        let records: Vec<RunRecord> = self
            .runs
            .iter()
            .filter_map(|run| {
                let state = run.state.lock().unwrap();
                (state.record.project == name).then(|| state.record.clone())
            })
            .collect();
        project_dot(&records)
    }

    /// `<distro> is stopped`, or why WSL can't be used, if either applies.
    fn distro_note(&self, distro: Option<&str>) -> Option<String> {
        let distro = distro?;
        self.distro_states.get(distro)?.label(distro)
    }

    fn project_list(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Projects");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Add…").on_hover_text("Pick a repo, or a folder containing repos").clicked() {
                    self.add_projects(ui.ctx());
                }
                if ui.button("Refresh").clicked() {
                    self.refresh_statuses(ui.ctx(), true);
                }
            });
        });
        ui.separator();
        if self.config.projects.is_empty() {
            ui.weak("No projects yet. Use Add… to pick a repo or a folder of repos.");
        }
        ScrollArea::vertical().show(ui, |ui| {
            for index in 0..self.config.projects.len() {
                let (name, path) = {
                    let project = &self.config.projects[index];
                    (project.name.clone(), project.path.clone())
                };
                let (color, tooltip) = self.project_dot(&name).unwrap_or((Color32::DARK_GRAY, None));
                ui.horizontal(|ui| {
                    dot(ui, color, tooltip.as_deref());
                    if ui.selectable_label(self.selected == Some(index), RichText::new(&name).strong()).clicked() {
                        self.selected = Some(index);
                        self.selected_run = None;
                        self.release_bump = None;
                        self.build_bump = Bump::Patch;
                    }
                });
                let distro = wsl::target(&path).map(|target| target.distro);
                let distro_note = self.distro_note(distro.as_deref());
                if let Some(distro) = &distro {
                    ui.small(format!("WSL: {distro}"));
                }
                if let Some(note) = &distro_note {
                    ui.label(RichText::new(note).small().color(AMBER));
                }
                match self.statuses.get(&path) {
                    Some(status) => {
                        ui.small(status_summary(status));
                        if !status.last_commit.is_empty() {
                            ui.add(egui::Label::new(RichText::new(&status.last_commit).small().weak()).truncate());
                        }
                    }
                    // A stopped distro's status isn't on its way, so there's nothing to wait for.
                    None if distro_note.is_some() => {}
                    None => {
                        ui.small("…");
                    }
                }
                ui.add_space(8.0);
            }
        });
    }

    fn project_detail(&mut self, ui: &mut egui::Ui, index: usize) {
        let project = self.config.projects[index].clone();
        let status = self.statuses.get(&project.path).cloned().unwrap_or_default();

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(&project.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Remove").on_hover_text("Removes it from this list only").clicked() {
                    self.config.projects.remove(index);
                    self.selected = (!self.config.projects.is_empty()).then_some(0);
                    self.save_config();
                }
                if ui.button("Open folder").clicked() {
                    open_folder(&project.path);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let path = RichText::new(project.path.display().to_string()).weak();
                    ui.add(egui::Label::new(path).truncate());
                });
            });
        });
        if index >= self.config.projects.len() || self.config.projects[index].path != project.path {
            return;
        }
        ui.separator();

        let target = wsl::target(&project.path);
        if let Some(target) = &target {
            ui.horizontal(|ui| {
                ui.weak(format!("WSL: {}", target.distro));
                if self.distro_states.get(&target.distro) == Some(&DistroState::Stopped) {
                    ui.colored_label(AMBER, DistroState::Stopped.label(&target.distro).unwrap_or_default());
                }
            });
        }
        let tooling = match &target {
            Some(target) => Tooling::Wsl {
                distro: &target.distro,
                on_windows_drive: target.is_on_windows_drive(),
                state: self.distro_states.get(&target.distro),
                tools: self.distro_tools.get(&target.distro),
            },
            None => Tooling::Native { claude_found: self.claude.is_some(), gh_status: self.gh_status.as_ref() },
        };
        let known = repo_known(self.statuses.get(&project.path), target.is_some());
        let needs_makefile = known && status.has_origin && !status.has_makefile;
        let blocker = project_blocker(&project.default_branch, known, &status, &tooling);
        let makefile_enabled = can_create_makefile(&tooling);
        if let Some(blocker) = &blocker {
            ui.colored_label(AMBER, blocker);
        }
        if needs_makefile {
            let label = format!("Create Makefile and push to {}", project.default_branch);
            let hint = "An agent writes it from the project's tests and CI, and it has to pass before it's pushed";
            let enabled = makefile_enabled;
            if ui.add_enabled(enabled, egui::Button::new(label)).on_hover_text(hint).clicked() {
                self.start_run(ui.ctx(), project.clone(), RunKind::CreateMakefile);
            }
        }

        ui.add(
            TextEdit::multiline(&mut self.task)
                .hint_text("What should the agent fix or build?")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            let can_build = blocker.is_none() && !self.task.trim().is_empty();
            if ui.add_enabled(can_build, egui::Button::new("Run pipeline")).clicked() {
                let bump = (!self.config.projects[index].grill).then_some(self.build_bump);
                let kind = RunKind::Build { task: self.task.trim().to_string(), bump };
                self.task.clear();
                self.build_bump = Bump::Patch;
                self.start_run(ui.ctx(), project.clone(), kind);
            }
            let toggle = ui.checkbox(&mut self.config.projects[index].grill, "Grill first").on_hover_text(
                "Unchecked, the prompt goes straight to the build agent, and the run still publishes a release with no questions asked",
            );
            if toggle.changed() {
                self.save_config();
            }
            let grill = self.config.projects[index].grill;
            if !grill {
                for bump in [Bump::Patch, Bump::Minor, Bump::Major] {
                    ui.radio_value(&mut self.build_bump, bump, bump.label());
                }
            }
            ui.weak(pipeline_hint(grill, &project.default_branch));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(blocker.is_none(), egui::Button::new("Cut release…")).clicked() {
                    self.release_bump = Some(Bump::Patch);
                }
            });
        });

        if self.release_bump.is_some() {
            self.release_prompt(ui, &project);
        }

        self.settings(ui, index);
        ui.separator();
        self.runs_view(ui, &project);
    }

    fn release_prompt(&mut self, ui: &mut egui::Ui, project: &Project) {
        let mut confirm = false;
        let mut close = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(format!(
                "Test origin/{} and, if it passes, bump the version and publish a release. Published releases are public.",
                project.default_branch
            ));
            ui.horizontal(|ui| {
                if let Some(selected) = &mut self.release_bump {
                    for bump in [Bump::Patch, Bump::Minor, Bump::Major] {
                        ui.radio_value(selected, bump, bump.label());
                    }
                }
                confirm = ui.button("Test, release and publish").clicked();
                close = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            if let Some(bump) = self.release_bump.take() {
                self.start_run(ui.ctx(), project.clone(), RunKind::Release { bump });
            }
        } else if close {
            self.release_bump = None;
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui, index: usize) {
        let mut changed = false;
        egui::CollapsingHeader::new("Settings").show(ui, |ui| {
            let project = &mut self.config.projects[index];
            egui::Grid::new("settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                let mut text_row = |ui: &mut egui::Ui, label: &str, value: &mut String, tip: &str| {
                    ui.label(label).on_hover_text(tip);
                    let edit = TextEdit::singleline(value).desired_width(f32::INFINITY);
                    changed |= ui.add(edit).changed();
                    ui.end_row();
                };
                text_row(ui, "Setup command", &mut project.setup_command, "Runs in each fresh worktree before the build, e.g. npm ci");
                text_row(ui, "Default branch", &mut project.default_branch, "Runs start from origin/<this> and merge back into it");
                text_row(
                    ui,
                    "Release command",
                    &mut project.release_command,
                    "Only without a release workflow: runs once the draft exists, with FACTORY_TAG set",
                );
                text_row(ui, "Model", &mut project.model, "Blank uses Claude Code's default");
                text_row(
                    ui,
                    "Allowed tools",
                    &mut project.allowed_tools,
                    "Comma-separated, e.g. Bash(cargo check:*). The factory's denylist still applies.",
                );

                ui.label("Agent permissions");
                egui::ComboBox::from_id_salt("permission_mode")
                    .selected_text(project.permission_mode.clone())
                    .show_ui(ui, |ui| {
                        for mode in ["acceptEdits", "default", "bypassPermissions"] {
                            changed |= ui.selectable_value(&mut project.permission_mode, mode.to_string(), mode).changed();
                        }
                    });
                ui.end_row();

                ui.label("Max attempts").on_hover_text("Agent/test rounds before the run fails");
                changed |= ui.add(egui::DragValue::new(&mut project.max_attempts).range(1..=10)).changed();
                ui.end_row();
            });
            if project.permission_mode == "bypassPermissions" {
                ui.colored_label(RED, "bypassPermissions lets the agent run anything on this machine, unprompted.");
            }
        });
        if changed {
            self.save_config();
        }
    }

    fn runs_view(&mut self, ui: &mut egui::Ui, project: &Project) {
        let records: Vec<RunRecord> = self
            .runs
            .iter()
            .map(|run| run.state.lock().unwrap().record.clone())
            .filter(|record| record.project == project.name)
            .collect();
        if records.is_empty() {
            ui.weak("No runs yet.");
            return;
        }
        if self.selected_run.as_ref().is_none_or(|id| !records.iter().any(|r| &r.id == id)) {
            self.selected_run = Some(records[0].id.clone());
        }

        egui::Panel::left("run_list").default_size(280.0).show(ui, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                for record in &records {
                    let selected = self.selected_run.as_deref() == Some(record.id.as_str());
                    ui.horizontal(|ui| {
                        dot(ui, run_dot_color(record), wait_label(record));
                        if ui.selectable_label(selected, truncated(&record.kind.title(), 34)).clicked() {
                            self.selected_run = Some(record.id.clone());
                        }
                    });
                }
            });
        });

        let Some(id) = self.selected_run.clone() else { return };
        egui::CentralPanel::default().show(ui, |ui| self.run_detail(ui, &id, project));
    }

    fn run_detail(&mut self, ui: &mut egui::Ui, id: &str, project: &Project) {
        let Some(position) = self.runs.iter().position(|r| r.state.lock().unwrap().record.id == id) else {
            return;
        };
        if !self.runs[position].log_loaded {
            self.runs[position].state.lock().unwrap().log = pipeline::read_log(&self.paths, id);
            self.runs[position].log_loaded = true;
        }
        let record = self.runs[position].state.lock().unwrap().record.clone();

        ui.horizontal_wrapped(|ui| {
            ui.colored_label(status_color(record.status), status_label(record.status));
            ui.strong(record.kind.title());
        });
        stage_strip(ui, &record);
        wait_row(ui, &record);

        if let Some(error) = &record.error {
            ui.colored_label(RED, error);
        }
        if let Some(target) = &record.pushed_to {
            let commit = record.commit.as_deref().map(|c| &c[..c.len().min(10)]).unwrap_or("");
            ui.colored_label(GREEN, format!("Pushed {commit} to {target}"));
        }
        if let Some(note) = &record.note {
            ui.colored_label(AMBER, note);
        }
        if let Some(url) = &record.release_url {
            ui.hyperlink_to(format!("Published {}", record.release_tag.as_deref().unwrap_or("release")), url);
        }

        self.grill_panel(ui, position, &record);

        ui.horizontal(|ui| {
            if record.status == RunStatus::Running {
                if let Some(cancel) = &self.runs[position].cancel {
                    if ui.button("Stop").clicked() {
                        cancel.cancel();
                    }
                }
            } else if let Some(worktree) = &record.worktree {
                if ui.button("Open worktree").clicked() {
                    open_folder(worktree);
                }
                if ui.button("Discard worktree").clicked() {
                    self.discard(ui.ctx(), position, project);
                }
            }
            if ui.button("Copy log").clicked() {
                let text = self.runs[position].state.lock().unwrap().log.join("\n");
                ui.ctx().copy_text(text);
            }
        });
        ui.separator();

        let state = self.runs[position].state.clone();
        let state = state.lock().unwrap();
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        ScrollArea::both()
            .id_salt(id)
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show_rows(ui, row_height, state.log.len(), |ui, rows| {
                for line in &state.log[rows] {
                    ui.add(egui::Label::new(RichText::new(line).monospace()).extend());
                }
            });
    }

    /// The Q&A with the grill agent, plus the agreed brief once there is one.
    fn grill_panel(&mut self, ui: &mut egui::Ui, position: usize, record: &RunRecord) {
        if !record.transcript.is_empty() || record.brief.is_some() {
            egui::CollapsingHeader::new("Requirements").id_salt(&record.id).show(ui, |ui| {
                ScrollArea::vertical().id_salt("requirements").max_height(180.0).show(ui, |ui| {
                    for exchange in &record.transcript {
                        for question in &exchange.questions {
                            ui.label(question);
                        }
                        ui.colored_label(BLUE, &exchange.answer);
                        ui.add_space(4.0);
                    }
                    if let Some(brief) = &record.brief {
                        ui.strong("Agreed solution");
                        ui.label(brief);
                    }
                });
            });
        }

        let Some(questions) = &record.questions else { return };
        let entry = &mut self.runs[position];
        if entry.drafts.len() != questions.len() {
            entry.drafts = questions.iter().map(QuestionDraft::new).collect();
        }
        let mut reply = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.colored_label(AMBER, "The agent needs answers before it can build.");
            ScrollArea::vertical().id_salt(("questions", &record.id)).max_height(360.0).show(ui, |ui| {
                for (number, (question, draft)) in questions.iter().zip(&mut entry.drafts).enumerate() {
                    ui.push_id((&record.id, number), |ui| question_input(ui, number, question, draft));
                }
            });
            ui.horizontal(|ui| {
                let answers = build_reply(questions, &entry.drafts);
                if ui.add_enabled(answers.is_some(), egui::Button::new("Send answers")).clicked() {
                    reply = answers.clone().map(GrillReply::Answer);
                }
                let hint = "Have the agent settle whatever is left and write the brief";
                if ui.button("Enough, build it").on_hover_text(hint).clicked() {
                    reply = Some(enough_reply(answers));
                }
            });
        });
        if let Some(reply) = reply {
            // Hide them now, so a second click can't send a reply nobody is waiting for. This comes
            // before sending so it can't overwrite the wait the pipeline sets once it has the reply.
            {
                let mut state = entry.state.lock().unwrap();
                state.record.questions = None;
                state.record.waiting_on = None;
            }
            if let Some(replies) = &entry.replies {
                let _ = replies.send(reply);
            }
            entry.drafts.clear();
        }
    }

    fn discard(&mut self, ctx: &egui::Context, position: usize, project: &Project) {
        if wsl::target(&project.path).is_some() {
            // Removing it can wake the distro, so not on the UI thread and not holding the lock.
            let state = self.runs[position].state.clone();
            let mut record = state.lock().unwrap().record.clone();
            let (repo, paths, tx, ctx) = (project.path.clone(), self.paths.clone(), self.wsl_tx.clone(), ctx.clone());
            thread::spawn(move || {
                let result = pipeline::discard(&repo, &mut record);
                {
                    let mut state = state.lock().unwrap();
                    state.record.worktree = record.worktree;
                    state.record.branch = record.branch;
                    let _ = pipeline::save(&paths, &state.record);
                }
                if let Err(error) = result {
                    let _ = tx.send(WslEvent::Notice(format!("Could not discard worktree: {error}")));
                }
                ctx.request_repaint();
            });
            return;
        }
        let result = {
            let mut state = self.runs[position].state.lock().unwrap();
            let result = pipeline::discard(&project.path, &mut state.record);
            let _ = pipeline::save(&self.paths, &state.record);
            result
        };
        if let Err(error) = result {
            self.notice = Some(format!("Could not discard worktree: {error}"));
        }
    }
}

/// One question: its text, a button per option, and a field for typing something else.
fn question_input(ui: &mut egui::Ui, number: usize, question: &Question, draft: &mut QuestionDraft) {
    ui.label(format!("{}. {}", number + 1, question.text));
    if !question.options.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for (index, option) in question.options.iter().enumerate() {
                let label = if question.recommended == Some(index) { format!("{option} (recommended)") } else { option.clone() };
                if ui.selectable_label(draft.selected == Some(index), label).clicked() {
                    draft.click_option(index);
                }
            }
        });
    }
    let hint = if question.options.is_empty() { "Your answer" } else { "Other" };
    let other = TextEdit::multiline(&mut draft.other).hint_text(hint).desired_rows(1).desired_width(f32::INFINITY);
    if ui.add(other).changed() {
        draft.other_edited();
    }
    ui.add_space(8.0);
}

fn truncated(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    } else {
        text.to_string()
    }
}

/// One chip per planned stage, showing the latest attempt of each.
fn stage_strip(ui: &mut egui::Ui, record: &RunRecord) {
    ui.horizontal_wrapped(|ui| {
        for (i, stage) in record.plan.iter().copied().enumerate() {
            if i > 0 {
                ui.weak("→");
            }
            ui.label(chip_text(record, stage));
        }
    });
}

/// What a running run is waiting on. The row keeps its height with nothing to show, so the log and
/// buttons below don't jump between stages.
fn wait_row(ui: &mut egui::Ui, record: &RunRecord) {
    if record.status != RunStatus::Running {
        return;
    }
    let height = ui.spacing().interact_size.y;
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), height), layout, |ui| {
        ui.set_min_height(height);
        let Some(wait) = &record.waiting_on else { return };
        // Every second, or the timer would stand still while waiting on the user: no spinner there.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let label = if record.questions.is_some() {
            RichText::new(&wait.label).color(AMBER)
        } else {
            ui.spinner();
            RichText::new(&wait.label)
        };
        ui.scope(|ui| {
            ui.set_max_width((ui.available_width() - WAIT_TRAILING_WIDTH).max(0.0));
            ui.add(egui::Label::new(label).truncate());
        });
        if let Some(elapsed) = elapsed_text(&wait.since, Local::now().fixed_offset()) {
            ui.weak(elapsed);
        }
        if let Some(url) = &wait.url {
            ui.hyperlink_to("View run", url);
        }
    });
}

fn chip_text(record: &RunRecord, stage: Stage) -> RichText {
    let attempts: Vec<_> = record.stages.iter().filter(|s| s.stage == stage).collect();
    let mut text = stage.label().to_string();
    if attempts.len() > 1 {
        text.push_str(&format!(" ×{}", attempts.len()));
    }
    let chip = RichText::new(text);
    match attempts.last().map(|s| s.status) {
        None => chip.weak(),
        Some(StageStatus::Running) if record.status == RunStatus::Running => chip.color(BLUE).strong(),
        Some(StageStatus::Running) | Some(StageStatus::Cancelled) => chip.color(AMBER),
        Some(StageStatus::Passed) => chip.color(GREEN),
        Some(StageStatus::Failed) => chip.color(RED).strong(),
    }
}

fn open_folder(path: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = command(opener).arg(path).spawn();
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        while let Ok((path, status)) = self.status_rx.try_recv() {
            self.statuses.insert(path, status);
        }
        while let Ok(status) = self.gh_rx.try_recv() {
            self.gh_status = Some(status);
        }
        while let Ok(event) = self.wsl_rx.try_recv() {
            match event {
                WslEvent::State(distro, state) => {
                    self.distro_states.insert(distro, state);
                }
                WslEvent::Tools(distro, tools) => {
                    self.distro_tools.insert(distro, tools);
                }
                WslEvent::Added(folder, found) => self.add_found(&ctx, &folder, found),
                WslEvent::Notice(notice) => self.notice = Some(notice),
            }
        }
        let running = self.running_count();
        // A run just finished, so the repo it pushed from may look different.
        let run_finished = running < self.running_last_frame;
        self.running_last_frame = running;
        if run_finished || self.last_refresh.is_none_or(|at| at.elapsed() >= REFRESH_EVERY) {
            self.refresh_statuses(&ctx, false);
        }
        ctx.request_repaint_after(REFRESH_EVERY);

        if let Some(updater) = &mut self.updater {
            if let Some(notice) = updater.poll(&ctx, running) {
                self.notice = Some(notice);
            }
        }
        self.update_bar(ui);

        if let Some(notice) = self.notice.clone() {
            egui::Panel::bottom("notice").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(notice);
                    if ui.small_button("Dismiss").clicked() {
                        self.notice = None;
                    }
                });
            });
        }

        egui::Panel::left("projects")
            .default_size(300.0)
            .show(ui, |ui| {
                self.version_row(ui);
                self.project_list(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| match self.selected {
            Some(index) if index < self.config.projects.len() => self.project_detail(ui, index),
            _ => {
                ui.centered_and_justified(|ui| {
                    ui.weak("Select or add a project");
                });
            }
        });
    }

    fn on_exit(&mut self) {
        for run in &self.runs {
            if let Some(cancel) = &run.cancel {
                cancel.cancel();
            }
        }
        // Exiting kills the run threads, so give them time to kill their child processes first.
        let deadline = Instant::now() + EXIT_GRACE;
        while Instant::now() < deadline
            && self.runs.iter().filter_map(|run| run.thread.as_ref()).any(|thread| !thread.is_finished())
        {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Wait;

    fn good_repo() -> RepoStatus {
        RepoStatus { has_origin: true, has_makefile: true, ..Default::default() }
    }

    fn native(claude_found: bool, gh: Option<&Result<(), String>>) -> Tooling<'_> {
        Tooling::Native { claude_found, gh_status: gh }
    }

    fn in_ubuntu<'a>(state: Option<&'a DistroState>, tools: Option<&'a Result<ToolCheck, String>>) -> Tooling<'a> {
        Tooling::Wsl { distro: "Ubuntu", on_windows_drive: false, state, tools }
    }

    fn blocker(tooling: &Tooling) -> Option<String> {
        project_blocker("main", true, &good_repo(), tooling)
    }

    #[test]
    fn native_blockers_come_from_the_windows_tools() {
        let not_installed = Err("could not run gh: not found".to_string());
        let logged_out = Err("HTTP 401".to_string());
        assert_eq!(blocker(&native(true, Some(&Ok(())))), None);
        assert_eq!(blocker(&native(true, None)), None);
        assert!(blocker(&native(false, None)).unwrap().contains("claude binary"));
        assert!(blocker(&native(true, Some(&not_installed))).unwrap().contains("isn't installed"));
        assert!(blocker(&native(true, Some(&logged_out))).unwrap().contains("isn't logged in"));
    }

    #[test]
    fn repo_blockers_come_first_for_any_project() {
        let tooling = native(false, None);
        let no_origin = RepoStatus::default();
        assert!(project_blocker("main", true, &no_origin, &tooling).unwrap().contains("no origin"));
        let no_makefile = RepoStatus { has_origin: true, ..Default::default() };
        assert!(project_blocker("dev", true, &no_makefile, &tooling).unwrap().contains("origin/dev has no Makefile"));
    }

    #[test]
    fn the_windows_tools_are_not_needed_for_a_wsl_project() {
        let available = DistroState::Available;
        let fine = Ok(ToolCheck { missing: Vec::new(), gh_logged_in: true });
        let tooling = in_ubuntu(Some(&available), Some(&fine));
        assert_eq!(blocker(&tooling), None);
        assert!(can_create_makefile(&tooling));
    }

    #[test]
    fn a_distro_missing_tools_blocks_with_its_name_and_a_makefile_only_needs_the_agent() {
        let available = DistroState::Available;
        let no_gh = Ok(ToolCheck { missing: vec!["gh".into()], gh_logged_in: false });
        let tooling = in_ubuntu(Some(&available), Some(&no_gh));
        let text = blocker(&tooling).unwrap();
        assert!(text.contains("Ubuntu") && text.contains("gh"), "{text}");
        assert!(can_create_makefile(&tooling));

        let no_claude = Ok(ToolCheck { missing: vec!["claude".into()], gh_logged_in: true });
        let tooling = in_ubuntu(Some(&available), Some(&no_claude));
        assert!(blocker(&tooling).unwrap().contains("claude"));
        assert!(!can_create_makefile(&tooling));

        let failed = Err("no output from the distro".to_string());
        let text = blocker(&in_ubuntu(Some(&available), Some(&failed))).unwrap();
        assert_eq!(text, "Could not check the tools in Ubuntu: no output from the distro");
    }

    #[test]
    fn a_stopped_distro_is_not_a_blocker_even_with_an_old_or_empty_status() {
        let stopped = DistroState::Stopped;
        let old_problem = Ok(ToolCheck { missing: vec!["make".into()], gh_logged_in: false });
        let tooling = in_ubuntu(Some(&stopped), Some(&old_problem));
        assert_eq!(blocker(&tooling), None);
        assert!(can_create_makefile(&tooling));

        // With no status yet, or only an error from the distro, nothing is known about the repo.
        assert!(!repo_known(None, true));
        let errored = RepoStatus { error: Some("wsl.exe failed".into()), ..Default::default() };
        assert!(!repo_known(Some(&errored), true));
        assert_eq!(project_blocker("main", repo_known(Some(&errored), true), &errored, &tooling), None);
        assert!(repo_known(Some(&good_repo()), true));
        assert!(repo_known(Some(&errored), false), "native projects keep treating it as known");
    }

    #[test]
    fn wsl_being_unavailable_blocks_with_its_own_message() {
        let unavailable = DistroState::Unavailable("not installed".into());
        let tooling = in_ubuntu(Some(&unavailable), None);
        assert_eq!(blocker(&tooling).unwrap(), "WSL isn't available: not installed");
        assert!(!can_create_makefile(&tooling));
    }

    #[test]
    fn a_project_saved_on_a_windows_drive_is_blocked() {
        let tooling = Tooling::Wsl { distro: "Ubuntu", on_windows_drive: true, state: None, tools: None };
        assert_eq!(blocker(&tooling).unwrap(), wsl::WINDOWS_DRIVE_NOTICE);
        assert!(!can_create_makefile(&tooling));
    }

    #[test]
    fn the_hint_only_mentions_the_grill_when_it_will_run() {
        assert_eq!(pipeline_hint(true, "main"), "Grill → build → make test → merge to main → publish release");
        assert_eq!(pipeline_hint(false, "main"), "Build → make test → merge to main → publish release");
    }

    fn pick(option: usize) -> QuestionDraft {
        QuestionDraft { selected: Some(option), other: String::new() }
    }

    fn typed(text: &str) -> QuestionDraft {
        QuestionDraft { selected: None, other: text.to_string() }
    }

    fn choice() -> Question {
        Question { text: "Which?".into(), options: vec!["A".into(), "B".into()], recommended: Some(1) }
    }

    #[test]
    fn a_draft_starts_on_the_recommended_option() {
        assert_eq!(QuestionDraft::new(&choice()), pick(1));
        assert_eq!(QuestionDraft::new(&Question::plain("Why?")), QuestionDraft::default());
    }

    #[test]
    fn the_reply_has_one_numbered_line_per_question_without_the_recommended_suffix() {
        let questions = [choice(), Question::plain("Why?"), choice()];
        let drafts = [pick(1), QuestionDraft::default(), pick(0)];
        assert_eq!(build_reply(&questions, &drafts).as_deref(), Some("1. B\n2. (no answer)\n3. A"));
    }

    #[test]
    fn typed_text_wins_over_a_selection_and_is_trimmed() {
        let draft = QuestionDraft { selected: Some(0), other: "  neither  ".into() };
        assert_eq!(build_reply(&[choice()], &[draft]).as_deref(), Some("1. neither"));
        let blank = QuestionDraft { selected: Some(0), other: "  ".into() };
        assert_eq!(build_reply(&[choice()], &[blank]).as_deref(), Some("1. A"));
    }

    #[test]
    fn no_answers_at_all_means_no_reply() {
        assert_eq!(build_reply(&[choice(), Question::plain("Why?")], &[QuestionDraft::default(), typed("   ")]), None);
    }

    #[test]
    fn clicking_selects_replaces_and_deselects_and_clears_typed_text() {
        let mut draft = typed("mine");
        draft.click_option(0);
        assert_eq!(draft, pick(0));
        draft.click_option(1);
        assert_eq!(draft, pick(1));
        draft.click_option(1);
        assert_eq!(draft, QuestionDraft::default());
    }

    #[test]
    fn typing_clears_the_selection_and_clearing_the_text_does_not_bring_it_back() {
        let mut draft = pick(0);
        draft.other = "  ".into();
        draft.other_edited();
        assert_eq!(draft.selected, Some(0), "whitespace isn't an answer");

        draft.other = "mine".into();
        draft.other_edited();
        assert_eq!(draft.selected, None);

        draft.other.clear();
        draft.other_edited();
        assert_eq!(draft, QuestionDraft::default());
    }

    #[test]
    fn enough_sends_what_is_selected_or_else_just_proceeds() {
        match enough_reply(Some("1. A".into())) {
            GrillReply::Answer(answer) => {
                assert_eq!(answer, "1. A\n\nThat's everything from me: settle anything still open and write the brief.")
            }
            GrillReply::Proceed => panic!("the selections would be lost"),
        }
        assert!(matches!(enough_reply(None), GrillReply::Proceed));
    }

    fn elapsed(since: &str) -> Option<String> {
        elapsed_text(since, DateTime::parse_from_rfc3339("2026-01-01T12:00:00+00:00").unwrap())
    }

    #[test]
    fn elapsed_time_reads_in_seconds_minutes_or_hours() {
        assert_eq!(elapsed("2026-01-01T12:00:00+00:00").as_deref(), Some("0s"));
        assert_eq!(elapsed("2026-01-01T11:59:18+00:00").as_deref(), Some("42s"));
        assert_eq!(elapsed("2026-01-01T11:59:00+00:00").as_deref(), Some("1m 00s"));
        assert_eq!(elapsed("2026-01-01T11:56:53+00:00").as_deref(), Some("3m 07s"));
        assert_eq!(elapsed("2026-01-01T11:00:01+00:00").as_deref(), Some("59m 59s"));
        assert_eq!(elapsed("2026-01-01T11:00:00+00:00").as_deref(), Some("1h 00m"));
        assert_eq!(elapsed("2026-01-01T10:57:30+00:00").as_deref(), Some("1h 02m"));
    }

    #[test]
    fn elapsed_time_copes_with_other_offsets_future_times_and_junk() {
        assert_eq!(elapsed("2026-01-01T13:59:18+02:00").as_deref(), Some("42s"));
        assert_eq!(elapsed("2026-01-01T12:00:30+00:00").as_deref(), Some("0s"), "a future time clamps to zero");
        assert_eq!(elapsed("not a time"), None);
        assert_eq!(elapsed(""), None);
    }

    fn run_record(status: RunStatus, questions: bool, wait: Option<&str>) -> RunRecord {
        RunRecord {
            id: "id".into(),
            project: "demo".into(),
            kind: RunKind::CreateMakefile,
            status,
            plan: Vec::new(),
            stages: Vec::new(),
            started_at: String::new(),
            finished_at: None,
            error: None,
            commit: None,
            pushed_to: None,
            worktree: None,
            branch: None,
            questions: questions.then(|| vec![Question::plain("Which?")]),
            transcript: Vec::new(),
            brief: None,
            release_tag: None,
            release_url: None,
            release_notes: None,
            note: None,
            waiting_on: wait.map(|label| Wait { label: label.into(), since: String::new(), url: None }),
        }
    }

    #[test]
    fn a_run_dot_is_amber_only_when_the_run_wants_an_answer() {
        let asking = run_record(RunStatus::Running, true, Some("Waiting for your answers"));
        assert_eq!(run_dot_color(&asking), AMBER);
        let ci = run_record(RunStatus::Running, false, Some("Waiting for the release workflow: queued"));
        assert_eq!(run_dot_color(&ci), BLUE);
        assert_eq!(run_dot_color(&run_record(RunStatus::Failed, false, None)), RED);
    }

    #[test]
    fn a_project_dot_is_amber_if_any_run_wants_an_answer() {
        let records = [
            run_record(RunStatus::Running, false, Some("Waiting for `make test`")),
            run_record(RunStatus::Running, true, Some("Waiting for your answers")),
        ];
        assert_eq!(project_dot(&records), Some((AMBER, Some("Waiting for your answers".to_string()))));
    }

    #[test]
    fn otherwise_a_project_dot_follows_its_latest_run() {
        let ci = "Waiting for the release workflow: in_progress";
        let records = [run_record(RunStatus::Running, false, Some(ci)), run_record(RunStatus::Failed, false, None)];
        assert_eq!(project_dot(&records), Some((BLUE, Some(ci.to_string()))), "a CI wait stays blue");

        let records = [run_record(RunStatus::Succeeded, false, None), run_record(RunStatus::Failed, false, None)];
        assert_eq!(project_dot(&records), Some((GREEN, None)), "no wait, no tooltip");

        assert_eq!(project_dot(&[]), None);
    }
}
