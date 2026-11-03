//! The window: a list of projects on the left, the selected project and its runs on the right.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ScrollArea, TextEdit};

use crate::agent::{Agent, ClaudeCli, locate_claude};
use crate::config::{self, Config, Paths, Project};
use crate::forge::{Forge, GhCli, locate_gh};
use crate::git::{self, RepoStatus};
use crate::pipeline::{self, Env, GrillReply, Question, RunKind, RunRecord, RunState, RunStatus, Stage, StageStatus};
use crate::proc::{Cancel, command};
use crate::version::Bump;

const REFRESH_EVERY: Duration = Duration::from_secs(30);
const EXIT_GRACE: Duration = Duration::from_secs(5);
const WORKFLOW_POLL: Duration = Duration::from_secs(15);

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
    last_refresh: Option<Instant>,
    runs: Vec<RunEntry>,
    selected_run: Option<String>,
    release_bump: Option<Bump>,
    notice: Option<String>,
    running_last_frame: usize,
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
            last_refresh: None,
            runs,
            selected_run: None,
            release_bump: None,
            notice,
            running_last_frame: 0,
        }
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save(&self.paths.config_file) {
            self.notice = Some(format!("Could not save config: {error}"));
        }
    }

    fn refresh_statuses(&mut self, ctx: &egui::Context) {
        self.last_refresh = Some(Instant::now());
        for project in &self.config.projects {
            let path = project.path.clone();
            let default_branch = project.default_branch.clone();
            let tx = self.status_tx.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let status = git::status(&path, &default_branch);
                let _ = tx.send((path, status));
                ctx.request_repaint();
            });
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
        let found = config::discover(&folder);
        if found.is_empty() {
            self.notice = Some(format!("No git repos found at {}", folder.display()));
            return;
        }
        let mut added = 0;
        for project in found {
            if !self.config.projects.iter().any(|p| p.path == project.path) {
                self.config.projects.push(project);
                added += 1;
            }
        }
        self.notice = Some(format!("Added {added} project(s)"));
        self.selected = self.selected.or(Some(0));
        self.save_config();
        self.refresh_statuses(ctx);
    }

    fn start_run(&mut self, ctx: &egui::Context, project: Project, kind: RunKind) {
        let agent: Arc<dyn Agent> = match &self.claude {
            Some(program) => Arc::new(ClaudeCli { program: program.clone() }),
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

    /// The color for a project's dot: its latest run, or amber if that run wants an answer.
    fn last_run_color(&self, name: &str) -> Option<Color32> {
        self.runs.iter().find_map(|run| {
            let state = run.state.lock().unwrap();
            let record = &state.record;
            (record.project == name).then(|| {
                if record.questions.is_some() { AMBER } else { status_color(record.status) }
            })
        })
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
                    self.refresh_statuses(ui.ctx());
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
                let color = self.last_run_color(&name).unwrap_or(Color32::DARK_GRAY);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(color));
                    if ui.selectable_label(self.selected == Some(index), RichText::new(&name).strong()).clicked() {
                        self.selected = Some(index);
                        self.selected_run = None;
                        self.release_bump = None;
                    }
                });
                match self.statuses.get(&path) {
                    Some(status) => {
                        ui.small(status_summary(status));
                        if !status.last_commit.is_empty() {
                            ui.add(egui::Label::new(RichText::new(&status.last_commit).small().weak()).truncate());
                        }
                    }
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

        let known = self.statuses.contains_key(&project.path);
        let needs_makefile = known && status.has_origin && !status.has_makefile;
        let blocker = if known && !status.has_origin {
            Some("This repo has no origin remote, so the pipeline has nowhere to push.".to_string())
        } else if needs_makefile {
            Some(format!("origin/{} has no Makefile, and `make test` is what every run has to pass.", project.default_branch))
        } else if self.claude.is_none() {
            Some("Could not find the claude binary. Set claude_path in config.toml.".to_string())
        } else if let Some(Err(error)) = &self.gh_status {
            let reason = if error.contains("could not run") { "isn't installed" } else { "isn't logged in" };
            Some(format!("The GitHub CLI {reason}. It's needed to watch CI and publish releases."))
        } else {
            None
        };
        if let Some(blocker) = &blocker {
            ui.colored_label(AMBER, blocker);
        }
        if needs_makefile {
            let label = format!("Create Makefile and push to {}", project.default_branch);
            let hint = "An agent writes it from the project's tests and CI, and it has to pass before it's pushed";
            let enabled = self.claude.is_some();
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
                let kind = RunKind::Build { task: self.task.trim().to_string() };
                self.task.clear();
                self.start_run(ui.ctx(), project.clone(), kind);
            }
            ui.weak(format!(
                "Grill → build → {} → merge to {} → publish release",
                pipeline::TEST_COMMAND,
                project.default_branch
            ));
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
                        ui.label(RichText::new("●").color(status_color(record.status)));
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
                    self.discard(position, project);
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
            if let Some(replies) = &entry.replies {
                let _ = replies.send(reply);
            }
            // Hide them now, so a second click can't send a reply nobody is waiting for.
            entry.state.lock().unwrap().record.questions = None;
            entry.drafts.clear();
        }
    }

    fn discard(&mut self, position: usize, project: &Project) {
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
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
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
        let running = self.running_count();
        // A run just finished, so the repo it pushed from may look different.
        let run_finished = running < self.running_last_frame;
        self.running_last_frame = running;
        if run_finished || self.last_refresh.is_none_or(|at| at.elapsed() >= REFRESH_EVERY) {
            self.refresh_statuses(&ctx);
        }
        ctx.request_repaint_after(REFRESH_EVERY);

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
            .show(ui, |ui| self.project_list(ui));

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
}
