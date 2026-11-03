//! Self-update for the installed MSI: polls this repo's releases through `gh`, downloads the new
//! MSI, and hands over to a helper that installs it once the app has exited.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Deserialize;

use crate::forge::{GhCli, locate_gh};
use crate::proc::command;

pub const REPO: &str = "Milkshak3s/factory";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);

/// A newer release that has an MSI this app knows how to install.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: String,
    pub tag: String,
    pub url: String,
    pub asset: String,
}

/// `x.y.z` with an optional leading `v`, compared numerically.
fn parse_version(text: &str) -> Option<[u64; 3]> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let numbers: Vec<u64> = text
        .split('.')
        // Checked by hand because u64's parser also accepts a leading `+`.
        .map(|part| if part.bytes().all(|b| b.is_ascii_digit()) { part.parse::<u64>().ok() } else { None })
        .collect::<Option<_>>()?;
    numbers.try_into().ok()
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(candidate), Some(current)) if candidate > current)
}

fn asset_name(version: &str) -> String {
    format!("factory-{version}-x64.msi")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
}

/// Decides from `gh release view` output whether there's something to install.
fn parse_release(json: &str, current: &str) -> Result<Option<Update>, String> {
    let release: ReleaseJson = serde_json::from_str(json).map_err(|e| format!("Could not read the release: {e}"))?;
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let version = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name).to_string();
    let asset = asset_name(&version);
    if !release.assets.iter().any(|candidate| candidate.name == asset) {
        return Ok(None);
    }
    Ok(Some(Update { version, tag: release.tag_name, url: release.url, asset }))
}

fn gh() -> GhCli {
    GhCli { program: locate_gh() }
}

fn check() -> Result<Option<Update>, String> {
    let args = ["release", "view", "--repo", REPO, "--json", "tagName,url,assets"];
    parse_release(&gh().run(&std::env::temp_dir(), &args, None)?, CURRENT_VERSION)
}

fn download(update: &Update, updates_dir: &Path) -> Result<PathBuf, String> {
    let dir = updates_dir.join(&update.tag);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dir_arg = dir.to_string_lossy();
    let args: [&str; 10] = [
        "release", "download", &update.tag, "--repo", REPO, "--pattern", &update.asset, "--dir", &dir_arg, "--clobber",
    ];
    gh().run(&std::env::temp_dir(), &args, None)?;
    let msi = dir.join(&update.asset);
    match fs::metadata(&msi) {
        Ok(file) if file.len() > 0 => Ok(msi),
        _ => Err(format!("{} is missing or empty after the download", msi.display())),
    }
}

/// Lowercased with backslashes and no trailing separator, the way Windows treats paths as equal.
fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    text.strip_prefix(r"\\?\").unwrap_or(&text).trim_end_matches('\\').to_string()
}

fn is_install_dir(exe_dir: &Path, local_app_data: &Path) -> bool {
    normalized(exe_dir) == normalized(&local_app_data.join("Programs").join("Factory"))
}

/// The installed exe, if this process is the MSI's copy rather than a source build.
fn installed_exe() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    is_install_dir(dir, Path::new(&local_app_data)).then(|| dir.join("factory.exe"))
}

/// A PowerShell literal. PowerShell also reads the typographic quotes as single quotes, and a
/// Windows user profile path can contain any of them.
fn ps_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// Waits for the app to exit, installs the MSI, then relaunches whatever is installed by then. It
/// goes through .NET because the PowerShell cmdlets treat `[` in a path as a wildcard.
fn helper_script(pid: u32, msi: &Path, exe: &Path) -> String {
    let install_args = ps_quote(&format!("/i \"{}\" /passive /norestart", msi.display()));
    let exe_dir = exe.parent().unwrap_or(exe);
    let exe_quoted = ps_quote(&exe.display().to_string());
    let exe_dir_quoted = ps_quote(&exe_dir.display().to_string());
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         try {{ \
         $installer = [Diagnostics.Process]::Start((Join-Path $env:SystemRoot 'System32\\msiexec.exe'), {install_args}); \
         $installer.WaitForExit() \
         }} catch {{}}; \
         $app = New-Object Diagnostics.ProcessStartInfo; \
         $app.FileName = {exe_quoted}; \
         $app.WorkingDirectory = {exe_dir_quoted}; \
         $app.UseShellExecute = $true; \
         [void][Diagnostics.Process]::Start($app)"
    )
}

fn helper_args(pid: u32, msi: &Path, exe: &Path) -> Vec<String> {
    vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-Command".to_string(),
        helper_script(pid, msi, exe),
    ]
}

fn spawn_helper(msi: &Path, exe: &Path) -> Result<(), String> {
    command("powershell")
        .args(helper_args(std::process::id(), msi, exe))
        // The app's working directory can be the install folder, which the MSI has to replace.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not start the update: {e}"))
}

/// Where the update bar has got to.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Idle,
    Downloading,
    /// Downloaded, and waiting for the user to accept stopping the runs in progress.
    Confirm(PathBuf),
    Installing,
}

enum Message {
    Checked { manual: bool, result: Result<Option<Update>, String> },
    Downloaded(Result<PathBuf, String>),
}

pub struct Updater {
    exe: PathBuf,
    updates_dir: PathBuf,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    checking: bool,
    last_check: Option<Instant>,
    pub available: Option<Update>,
    pub step: Step,
}

impl Updater {
    /// `None` unless running from the install folder, so source builds never touch any of this.
    pub fn detect(data_dir: &Path) -> Option<Self> {
        installed_exe().map(|exe| Self::new(exe, data_dir.join("updates")))
    }

    fn new(exe: PathBuf, updates_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { exe, updates_dir, tx, rx, checking: false, last_check: None, available: None, step: Step::Idle }
    }

    pub fn is_checking(&self) -> bool {
        self.checking
    }

    pub fn start_check(&mut self, ctx: &egui::Context, manual: bool) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.last_check = Some(Instant::now());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(Message::Checked { manual, result: check() });
            ctx.request_repaint();
        });
    }

    pub fn start_download(&mut self, ctx: &egui::Context) {
        let Some(update) = self.available.clone() else { return };
        self.step = Step::Downloading;
        let updates_dir = self.updates_dir.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = download(&update, &updates_dir).map_err(|e| format!("Could not download the update: {e}"));
            let _ = tx.send(Message::Downloaded(result));
            ctx.request_repaint();
        });
    }

    /// Starts the helper and closes the app. The normal `on_exit` then stops any runs.
    pub fn install(&mut self, ctx: &egui::Context, msi: &Path) -> Result<(), String> {
        match spawn_helper(msi, &self.exe) {
            Ok(()) => {
                self.step = Step::Installing;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                Ok(())
            }
            Err(error) => {
                self.step = Step::Idle;
                Err(error)
            }
        }
    }

    /// Takes in finished background work and starts the next check if one is due. Call every
    /// frame. Returns anything the user should be told.
    pub fn poll(&mut self, ctx: &egui::Context, running: usize) -> Option<String> {
        let mut notice = None;
        while let Ok(message) = self.rx.try_recv() {
            let told = match message {
                Message::Checked { manual, result } => self.apply_check(manual, result),
                Message::Downloaded(Ok(msi)) if running > 0 => {
                    self.step = Step::Confirm(msi);
                    None
                }
                Message::Downloaded(Ok(msi)) => self.install(ctx, &msi).err(),
                Message::Downloaded(Err(error)) => {
                    self.step = Step::Idle;
                    Some(error)
                }
            };
            notice = told.or(notice);
        }
        if self.last_check.is_none_or(|at| at.elapsed() >= CHECK_EVERY) {
            self.start_check(ctx, false);
        }
        notice
    }

    fn apply_check(&mut self, manual: bool, result: Result<Option<Update>, String>) -> Option<String> {
        self.checking = false;
        match result {
            Ok(update) => {
                let up_to_date = update.is_none();
                // Don't swap the release out from under a download or prompt that's under way.
                if self.step == Step::Idle {
                    self.available = update;
                }
                (manual && up_to_date).then(|| format!("Factory is up to date (v{CURRENT_VERSION})"))
            }
            Err(error) => manual.then_some(error),
        }
    }
}

/// The prompt shown when the download finishes during a run. The count stays live while it's up.
pub fn confirm_text(running: usize) -> String {
    match running {
        0 => "The runs have finished. Update now?".to_string(),
        _ => format!("{running} run(s) in progress will be stopped. Update anyway?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str]) -> String {
        let assets: Vec<_> = assets.iter().map(|name| serde_json::json!({"name": name, "size": 1})).collect();
        serde_json::json!({"tagName": tag, "url": "https://example.test/r", "assets": assets}).to_string()
    }

    fn update(version: &str) -> Update {
        Update {
            version: version.to_string(),
            tag: format!("v{version}"),
            url: String::new(),
            asset: asset_name(version),
        }
    }

    fn updater() -> Updater {
        Updater::new(PathBuf::from("factory.exe"), PathBuf::from("updates"))
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.0"), "not a string comparison");
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.2.0", "0.2.0"), "equal isn't newer");
        assert!(!is_newer("0.1.9", "0.2.0"));
    }

    #[test]
    fn a_leading_v_is_allowed() {
        assert_eq!(parse_version("v1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_version("1.2.3"), Some([1, 2, 3]));
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
    }

    #[test]
    fn invalid_versions_are_never_newer() {
        for text in ["", "nightly", "1.2", "1.2.3.4", "1.2.x", "vv1.2.3", "1..3", "1.2.+3", "1.2.3-beta", " 1.2.3"] {
            assert_eq!(parse_version(text), None, "{text:?}");
            assert!(!is_newer(text, "0.0.1"), "{text:?}");
        }
        assert!(!is_newer("1.0.0", "garbage"));
    }

    #[test]
    fn a_newer_release_with_the_matching_msi_is_an_update() {
        let json = release("v0.3.0", &["other.zip", "factory-0.3.0-x64.msi"]);
        let found = parse_release(&json, "0.2.0").unwrap().unwrap();
        assert_eq!(
            found,
            Update {
                version: "0.3.0".into(),
                tag: "v0.3.0".into(),
                url: "https://example.test/r".into(),
                asset: "factory-0.3.0-x64.msi".into(),
            }
        );
    }

    #[test]
    fn a_tag_without_the_v_keeps_its_own_spelling_for_the_download() {
        let found = parse_release(&release("0.3.0", &["factory-0.3.0-x64.msi"]), "0.2.0").unwrap().unwrap();
        assert_eq!((found.tag.as_str(), found.version.as_str()), ("0.3.0", "0.3.0"));
    }

    #[test]
    fn only_the_exact_asset_name_counts() {
        for assets in [
            vec![],
            vec!["factory.exe"],
            vec!["factory-0.2.9-x64.msi"],
            vec!["factory-0.3.0-x86.msi"],
            vec!["factory-v0.3.0-x64.msi"],
            vec!["restock-0.3.0-x64.msi"],
            vec!["Factory-0.3.0-x64.msi"],
        ] {
            assert_eq!(parse_release(&release("v0.3.0", &assets), "0.2.0").unwrap(), None, "{assets:?}");
        }
    }

    #[test]
    fn an_older_equal_or_unparseable_tag_is_no_update() {
        for tag in ["v0.1.0", "v0.2.0", "latest"] {
            let json = release(tag, &[&format!("factory-{}-x64.msi", tag.trim_start_matches('v'))]);
            assert_eq!(parse_release(&json, "0.2.0").unwrap(), None, "{tag}");
        }
    }

    #[test]
    fn unreadable_output_is_an_error() {
        assert!(parse_release("not json", "0.2.0").is_err());
    }

    #[test]
    fn the_install_dir_is_recognized_regardless_of_case() {
        let local = Path::new(r"C:\Users\Jo Smith\AppData\Local");
        assert!(is_install_dir(Path::new(r"C:\Users\Jo Smith\AppData\Local\Programs\Factory"), local));
        assert!(is_install_dir(Path::new(r"c:\users\JO SMITH\appdata\local\PROGRAMS\factory"), local));
        assert!(is_install_dir(Path::new(r"C:\Users\Jo Smith\AppData\Local\Programs\Factory\"), local));
        assert!(is_install_dir(Path::new(r"\\?\C:\Users\Jo Smith\AppData\Local\Programs\Factory"), local));
    }

    #[test]
    fn other_dirs_are_not_the_install_dir() {
        let local = Path::new(r"C:\Users\Jo\AppData\Local");
        for dir in [
            r"C:\Users\Jo\projects\factory\target\release",
            r"C:\Users\Jo\AppData\Local\Programs",
            r"C:\Users\Jo\AppData\Local\Programs\Factory\bin",
            r"C:\Users\Jo\AppData\Local\Programs\FactoryOld",
            r"C:\Users\Other\AppData\Local\Programs\Factory",
            r"C:\Program Files\Factory",
        ] {
            assert!(!is_install_dir(Path::new(dir), local), "{dir}");
        }
    }

    #[test]
    fn quoting_doubles_every_kind_of_single_quote() {
        assert_eq!(ps_quote(r"C:\Users\Jo Smith"), r"'C:\Users\Jo Smith'");
        assert_eq!(ps_quote(r"C:\Users\O'Brien"), r"'C:\Users\O''Brien'");
        assert_eq!(ps_quote("O\u{2019}Brien"), "'O\u{2019}\u{2019}Brien'");
    }

    #[test]
    fn the_helper_waits_installs_then_relaunches_with_paths_quoted() {
        let msi = Path::new(r"C:\Users\Jo O'Brien\AppData\Local\factory\updates\v0.3.0\factory-0.3.0-x64.msi");
        let exe = Path::new(r"C:\Users\Jo O'Brien\AppData\Local\Programs\Factory\factory.exe");
        let args = helper_args(4242, msi, exe);
        assert_eq!(args[..3], ["-NoProfile", "-NonInteractive", "-Command"]);
        assert_eq!(args.len(), 4);

        let script = &args[3];
        assert!(script.contains(
            r#"'/i "C:\Users\Jo O''Brien\AppData\Local\factory\updates\v0.3.0\factory-0.3.0-x64.msi" /passive /norestart'"#
        ));
        assert!(script.contains(r"'C:\Users\Jo O''Brien\AppData\Local\Programs\Factory\factory.exe'"));
        assert!(script.contains(r"'C:\Users\Jo O''Brien\AppData\Local\Programs\Factory'"));

        let wait = script.find("Wait-Process -Id 4242").unwrap();
        let install = script.find("msiexec").unwrap();
        let wait_for_install = script.find("WaitForExit").unwrap();
        let relaunch = script.find("$app.FileName").unwrap();
        assert!(wait < install && install < wait_for_install && wait_for_install < relaunch);
        // The relaunch is outside the try, so it happens whatever msiexec did.
        assert!(script.find("catch").unwrap() < relaunch);
    }

    /// PowerShell itself has to agree: through Rust's argument quoting and back out unchanged.
    #[cfg(windows)]
    #[test]
    fn powershell_reads_quoted_text_back_unchanged() {
        let text = r#"/i "C:\Users\Jo O'Brien [x]\a b.msi" /passive"#;
        let script = format!("Write-Output {}", ps_quote(text));
        let output = command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), text);
    }

    #[cfg(windows)]
    #[test]
    fn powershell_can_parse_the_helper_script() {
        let msi = Path::new(r"C:\Users\Jo O'Brien\updates\v0.3.0\factory-0.3.0-x64.msi");
        let exe = Path::new(r"C:\Users\Jo O'Brien\Programs\Factory\factory.exe");
        let output = command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "[void][scriptblock]::Create($env:FACTORY_TEST_HELPER)"])
            .env("FACTORY_TEST_HELPER", helper_script(1, msi, exe))
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn background_failures_stay_silent_and_change_nothing() {
        let mut updater = updater();
        updater.available = Some(update("0.3.0"));
        updater.checking = true;
        assert_eq!(updater.apply_check(false, Err("gh is logged out".into())), None);
        assert_eq!(updater.available, Some(update("0.3.0")));
        assert!(!updater.checking, "the next interval can retry");
    }

    #[test]
    fn manual_checks_report_their_result() {
        let mut updater = updater();
        assert_eq!(updater.apply_check(true, Err("gh is logged out".into())).as_deref(), Some("gh is logged out"));
        assert_eq!(updater.apply_check(true, Ok(None)), Some(format!("Factory is up to date (v{CURRENT_VERSION})")));
        assert_eq!(updater.apply_check(false, Ok(None)), None);
        assert_eq!(updater.apply_check(true, Ok(Some(update("9.0.0")))), None, "the bar says it instead");
        assert_eq!(updater.available, Some(update("9.0.0")));
    }

    #[test]
    fn a_check_does_not_replace_the_release_being_downloaded() {
        let mut updater = updater();
        updater.available = Some(update("0.3.0"));
        updater.step = Step::Downloading;
        updater.apply_check(false, Ok(Some(update("0.4.0"))));
        assert_eq!(updater.available, Some(update("0.3.0")));
        updater.apply_check(false, Ok(None));
        assert_eq!(updater.available, Some(update("0.3.0")));

        updater.step = Step::Idle;
        updater.apply_check(false, Ok(None));
        assert_eq!(updater.available, None, "a withdrawn release takes the bar away");
    }

    #[test]
    fn the_prompt_counts_the_runs_it_would_stop() {
        assert_eq!(confirm_text(2), "2 run(s) in progress will be stopped. Update anyway?");
        assert_eq!(confirm_text(0), "The runs have finished. Update now?");
    }

    #[test]
    fn the_workflow_builds_the_asset_name_the_updater_looks_for() {
        let workflow = include_str!("../.github/workflows/release.yml");
        assert!(workflow.contains(&asset_name("$version")));
        assert!(workflow.contains("windows-latest"));
        assert!(workflow.contains("contents: write"));
        assert!(workflow.contains("cargo build --release --locked"));
        assert!(workflow.contains("gh release create \"v$version\" $msi --draft --target"));
    }

    #[test]
    fn the_msi_is_per_user_and_upgradeable() {
        let wxs = include_str!("../wix/main.wxs");
        assert!(wxs.contains(r#"Scope="perUser""#));
        assert!(wxs.contains(r#"UpgradeCode="7B3E4F0A-9C1D-4E52-A6B8-2D5F1C8E7A34""#));
        assert!(wxs.contains("<MajorUpgrade"));
        assert!(wxs.contains(r#"EmbedCab="yes""#));
        assert!(wxs.contains(r#"Id="LocalAppDataFolder""#));
        assert!(wxs.contains(r#"Name="Programs""#));
        assert!(wxs.contains(r#"Name="Factory""#));
        assert!(wxs.contains(r#"Root="HKCU""#));
    }
}
