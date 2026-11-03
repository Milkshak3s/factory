//! The window: a list of projects on the left, the selected project and its runs on the right.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ScrollArea, TextEdit};

use crate::agent::{Agent, ClaudeCli, locate_claude};
use crate::config::{self, Config, Paths, Project, PushTarget};
use crate::git::{self, RepoStatus};
use crate::pipeline::{self, Env, RunKind, RunRecord, RunState, RunStatus, Stage, StageStatus};
use crate::proc::{Cancel, command};

const REFRESH_EVERY: Duration = Duration::from_secs(30);
const EXIT_GRACE: Duration = Duration::from_secs(5);

const GREEN: Color32 = Color32::from_rgb(70, 170, 100);
const RED: Color32 = Color32::from_rgb(215, 85, 80);
const AMBER: Color32 = Color32::from_rgb(220, 165, 60);
const BLUE: Color32 = Color32::from_rgb(90, 150, 230);

struct RunEntry {
    state: Arc<Mutex<RunState>>,
    /// Only set while this session owns the run's thread.
    cancel: Option<Cancel>,
    thread: Option<JoinHandle<()>>,
    log_loaded: bool,
}

pub struct App {
    paths: Paths,
    config: Config,
    claude: Option<PathBuf>,
    selected: Option<usize>,
    task: String,
    statuses: HashMap<PathBuf, RepoStatus>,
    status_tx: mpsc::Sender<(PathBuf, RepoStatus)>,
    status_rx: mpsc::Receiver<(PathBuf, RepoStatus)>,
    last_refresh: Option<Instant>,
    runs: Vec<RunEntry>,
    selected_run: Option<String>,
    release_tag: Option<String>,
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
                thread: None,
                log_loaded: false,
            })
            .collect();
        let (status_tx, status_rx) = mpsc::channel();
        Self {
            paths,
            selected: (!config.projects.is_empty()).then_some(0),
            config,
            claude,
            task: String::new(),
            statuses: HashMap::new(),
            status_tx,
            status_rx,
            last_refresh: None,
            runs,
            selected_run: None,
            release_tag: None,
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
            let tx = self.status_tx.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let status = git::status(&path);
                let _ = tx.send((path, status));
                ctx.request_repaint();
            });
        }
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
            notify: Arc::new(move || repaint.request_repaint()),
        };
        let handle = pipeline::start(project, kind, env);
        self.selected_run = Some(handle.state.lock().unwrap().record.id.clone());
        self.runs.insert(
            0,
            RunEntry {
                state: handle.state,
                cancel: Some(handle.cancel),
                thread: Some(handle.thread),
                log_loaded: true,
            },
        );
    }

    fn running_count(&self) -> usize {
        self.runs.iter().filter(|r| r.state.lock().unwrap().record.status == RunStatus::Running).count()
    }

    fn last_run_status(&self, name: &str) -> Option<RunStatus> {
        self.runs.iter().find_map(|run| {
            let state = run.state.lock().unwrap();
            (state.record.project == name).then_some(state.record.status)
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
                let color = self.last_run_status(&name).map(status_color).unwrap_or(Color32::DARK_GRAY);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(color));
                    if ui.selectable_label(self.selected == Some(index), RichText::new(&name).strong()).clicked() {
                        self.selected = Some(index);
                        self.selected_run = None;
                        self.release_tag = None;
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

        let blocker = if project.test_command.trim().is_empty() {
            Some("Set a test command in Settings first. It's the gate every run has to pass.")
        } else if !status.has_origin && self.statuses.contains_key(&project.path) {
            Some("This repo has no origin remote, so the pipeline has nowhere to push.")
        } else if self.claude.is_none() {
            Some("Could not find the claude binary. Set claude_path in config.toml.")
        } else {
            None
        };
        if let Some(blocker) = blocker {
            ui.colored_label(AMBER, blocker);
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
            let destination = match project.push_target {
                PushTarget::Branch => "a factory/ branch".to_string(),
                PushTarget::Default => format!("{} directly", project.default_branch),
            };
            ui.weak(format!("Agent → {} → commit → push to {destination}", project.test_command.trim()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(blocker.is_none(), egui::Button::new("Cut release…")).clicked() {
                    self.release_tag = Some(git::next_tag(status.latest_tag.as_deref()));
                }
            });
        });

        if self.release_tag.is_some() {
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
                "Test origin/{} and, if it passes, tag it and push the tag. A pushed tag is public.",
                project.default_branch
            ));
            ui.horizontal(|ui| {
                ui.label("Tag");
                if let Some(tag) = &mut self.release_tag {
                    ui.add(TextEdit::singleline(tag).desired_width(140.0));
                }
                let ready = self.release_tag.as_ref().is_some_and(|t| !t.trim().is_empty());
                confirm = ui.add_enabled(ready, egui::Button::new("Test, tag and push")).clicked();
                close = ui.button("Cancel").clicked();
            });
        });
        if confirm {
            if let Some(tag) = self.release_tag.take() {
                self.start_run(ui.ctx(), project.clone(), RunKind::Release { tag: tag.trim().to_string() });
            }
        } else if close {
            self.release_tag = None;
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
                text_row(ui, "Test command", &mut project.test_command, "Required. The agent can't skip this.");
                text_row(ui, "Setup command", &mut project.setup_command, "Runs first in each fresh worktree, e.g. npm ci");
                text_row(ui, "Default branch", &mut project.default_branch, "Runs start from origin/<this>");
                text_row(ui, "Release command", &mut project.release_command, "Runs after the tag is pushed, with FACTORY_TAG set");
                text_row(ui, "Model", &mut project.model, "Blank uses Claude Code's default");
                text_row(
                    ui,
                    "Allowed tools",
                    &mut project.allowed_tools,
                    "Comma-separated, e.g. Bash(cargo check:*). The factory's denylist still applies.",
                );

                ui.label("Push to");
                ui.horizontal(|ui| {
                    changed |= ui.radio_value(&mut project.push_target, PushTarget::Branch, "New branch").changed();
                    changed |= ui.radio_value(&mut project.push_target, PushTarget::Default, "Default branch").changed();
                });
                ui.end_row();

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
