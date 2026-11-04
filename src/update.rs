//! Self-update for installed copies: polls this repo's releases through `gh`, downloads the new
//! package for this kind of install, and hands over to a helper that installs it once the app has
//! exited. The helper is the only part that differs by platform: PowerShell and msiexec on Windows,
//! a generated `sh` script elsewhere. A failed install leaves the current version in place, and
//! either way the helper relaunches what's installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Deserialize;

use crate::forge::{GhCli, locate_gh};
use crate::proc::{command, own_process_group};

pub const REPO: &str = "Milkshak3s/factory";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);
const DEB_EXE: &str = "/usr/bin/factory";

/// How this copy was installed, which decides what to download and how to install it.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallKind {
    Msi,
    /// The installed `Factory.app`.
    MacApp(PathBuf),
    /// The `.AppImage` file itself.
    AppImage(PathBuf),
    Deb,
}

/// A newer release that has a package this install knows how to use.
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

fn asset_name(version: &str, kind: &InstallKind) -> String {
    match kind {
        InstallKind::Msi => format!("factory-{version}-x64.msi"),
        InstallKind::MacApp(_) => format!("factory-{version}-arm64.dmg"),
        InstallKind::AppImage(_) => format!("factory-{version}-x64.AppImage"),
        InstallKind::Deb => format!("factory-{version}-x64.deb"),
    }
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
fn parse_release(json: &str, current: &str, kind: &InstallKind) -> Result<Option<Update>, String> {
    let release: ReleaseJson = serde_json::from_str(json).map_err(|e| format!("Could not read the release: {e}"))?;
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let version = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name).to_string();
    let asset = asset_name(&version, kind);
    if !release.assets.iter().any(|candidate| candidate.name == asset) {
        return Ok(None);
    }
    Ok(Some(Update { version, tag: release.tag_name, url: release.url, asset }))
}

fn gh() -> GhCli {
    GhCli { program: locate_gh() }
}

fn check(kind: &InstallKind) -> Result<Option<Update>, String> {
    let args = ["release", "view", "--repo", REPO, "--json", "tagName,url,assets"];
    parse_release(&gh().run(&std::env::temp_dir(), &args, None)?, CURRENT_VERSION, kind)
}

fn download(update: &Update, updates_dir: &Path) -> Result<PathBuf, String> {
    let dir = updates_dir.join(&update.tag);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dir_arg = dir.to_string_lossy();
    let args: [&str; 10] = [
        "release", "download", &update.tag, "--repo", REPO, "--pattern", &update.asset, "--dir", &dir_arg, "--clobber",
    ];
    gh().run(&std::env::temp_dir(), &args, None)?;
    let package = dir.join(&update.asset);
    match fs::metadata(&package) {
        Ok(file) if file.len() > 0 => Ok(package),
        _ => Err(format!("{} is missing or empty after the download", package.display())),
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

/// `Factory.app`, if `exe` is its binary and it sits in an Applications folder. Elsewhere, such as a
/// mounted DMG or a path translocated by Gatekeeper, there's nothing lasting to replace.
fn installed_mac_app(exe: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let app = exe.ancestors().nth(3)?;
    let parent = app.parent()?;
    let is_bundle_binary = app.file_name()?.to_str()? == "Factory.app" && app.join("Contents").join("MacOS").join("factory") == exe;
    let in_applications = parent == Path::new("/Applications") || home.is_some_and(|home| parent == home.join("Applications"));
    (is_bundle_binary && in_applications).then(|| app.to_path_buf())
}

/// What detection looks at, gathered by `Updater::detect`.
struct HostFacts<'a> {
    /// A value of `std::env::consts::OS`.
    os: &'a str,
    exe: &'a Path,
    local_app_data: Option<&'a Path>,
    home: Option<&'a Path>,
    /// `$APPIMAGE`, which the AppImage runtime sets.
    appimage: Option<&'a Path>,
    appimage_is_file: bool,
}

/// The kind of install this process belongs to and the exe to relaunch, or `None` for a source build.
fn detect_install(host: &HostFacts) -> Option<(InstallKind, PathBuf)> {
    match host.os {
        "windows" => {
            let dir = host.exe.parent()?;
            is_install_dir(dir, host.local_app_data?).then(|| (InstallKind::Msi, dir.join("factory.exe")))
        }
        "macos" => installed_mac_app(host.exe, host.home).map(|app| (InstallKind::MacApp(app), host.exe.to_path_buf())),
        "linux" => match host.appimage {
            // has_root rather than is_absolute, which is the same on Unix and lets tests run anywhere.
            Some(appimage) => (appimage.has_root() && host.appimage_is_file)
                .then(|| (InstallKind::AppImage(appimage.to_path_buf()), host.exe.to_path_buf())),
            None => (host.exe == Path::new(DEB_EXE)).then(|| (InstallKind::Deb, host.exe.to_path_buf())),
        },
        _ => None,
    }
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

/// A literal for a POSIX shell.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn sh_path(path: &Path) -> String {
    sh_quote(&path.to_string_lossy())
}

fn sh_wait_for_exit(pid: u32) -> String {
    format!("while kill -0 {pid} 2>/dev/null; do sleep 1; done")
}

/// Waits for the app to exit, swaps the new app in beside the old one, and opens whichever is there
/// by then. The old app only goes away once the new one is in its place.
fn mac_helper_script(pid: u32, dmg: &Path, app: &Path) -> String {
    let wait = sh_wait_for_exit(pid);
    let app = sh_path(app);
    let dmg = sh_path(dmg);
    format!(
        r#"{wait}
app={app}
dmg={dmg}
new="$app.new"
old="$app.old"
if mount=$(mktemp -d); then
  if hdiutil attach -nobrowse -readonly -mountpoint "$mount" "$dmg" >/dev/null 2>&1; then
    rm -rf "$new"
    if ditto "$mount/Factory.app" "$new"; then
      rm -rf "$old"
      if mv "$app" "$old"; then
        if mv "$new" "$app"; then
          rm -rf "$old"
        else
          mv "$old" "$app"
        fi
      fi
    fi
    rm -rf "$new"
    hdiutil detach "$mount" >/dev/null 2>&1 || hdiutil detach -force "$mount" >/dev/null 2>&1
  fi
  rmdir "$mount" 2>/dev/null
fi
open "$app"
"#
    )
}

/// Waits for the app to exit, renames the new AppImage over the old one, and relaunches whichever is
/// there by then. The copy is beside the target so the rename stays on one filesystem. The variables
/// the AppImage runtime set for this run would mislead the next one.
fn appimage_helper_script(pid: u32, package: &Path, target: &Path) -> String {
    let wait = sh_wait_for_exit(pid);
    let package = sh_path(package);
    let target = sh_path(target);
    format!(
        r#"unset APPIMAGE APPDIR ARGV0
{wait}
package={package}
target={target}
swap="$target.update.$$"
if cp "$package" "$swap" && chmod +x "$swap" && mv -f "$swap" "$target"; then
  :
else
  rm -f "$swap"
fi
nohup "$target" </dev/null >/dev/null 2>&1 &
"#
    )
}

/// Waits for the app to exit, installs through polkit, and relaunches whatever is installed by then,
/// including when the user cancels the password prompt or `pkexec` is missing. The absolute path is
/// what makes apt treat the argument as a file rather than a package name.
fn deb_helper_script(pid: u32, package: &Path) -> String {
    let wait = sh_wait_for_exit(pid);
    let package = sh_path(package);
    let exe = sh_quote(DEB_EXE);
    format!(
        r#"{wait}
pkexec apt-get install -y {package} >/dev/null 2>&1
nohup {exe} </dev/null >/dev/null 2>&1 &
"#
    )
}

/// The `sh` script that installs `package`, for every kind of install but the MSI.
fn shell_helper_script(kind: &InstallKind, pid: u32, package: &Path) -> Option<String> {
    match kind {
        InstallKind::Msi => None,
        InstallKind::MacApp(app) => Some(mac_helper_script(pid, package, app)),
        InstallKind::AppImage(target) => Some(appimage_helper_script(pid, package, target)),
        InstallKind::Deb => Some(deb_helper_script(pid, package)),
    }
}

fn helper_command(kind: &InstallKind, pid: u32, package: &Path, exe: &Path) -> Command {
    let mut cmd = match shell_helper_script(kind, pid, package) {
        Some(script) => {
            let mut cmd = command("sh");
            cmd.arg("-c").arg(script);
            // Detached, so nothing aimed at the app's own process group reaches it.
            own_process_group(&mut cmd);
            cmd
        }
        None => {
            let mut cmd = command("powershell");
            cmd.args(helper_args(pid, package, exe));
            cmd
        }
    };
    // The app's working directory can be the install folder, which the update has to replace.
    cmd.current_dir(std::env::temp_dir()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

fn spawn_helper(kind: &InstallKind, package: &Path, exe: &Path) -> Result<(), String> {
    helper_command(kind, std::process::id(), package, exe)
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
    kind: InstallKind,
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
    /// `None` unless running from an installed copy, so source builds never touch any of this.
    pub fn detect(data_dir: &Path) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let env_path = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        let local_app_data = env_path("LOCALAPPDATA");
        let home = env_path("HOME");
        let appimage = env_path("APPIMAGE");
        let (kind, exe) = detect_install(&HostFacts {
            os: std::env::consts::OS,
            exe: &exe,
            local_app_data: local_app_data.as_deref(),
            home: home.as_deref(),
            appimage: appimage.as_deref(),
            appimage_is_file: appimage.as_deref().is_some_and(Path::is_file),
        })?;
        Some(Self::new(kind, exe, data_dir.join("updates")))
    }

    fn new(kind: InstallKind, exe: PathBuf, updates_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { kind, exe, updates_dir, tx, rx, checking: false, last_check: None, available: None, step: Step::Idle }
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
        let kind = self.kind.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(Message::Checked { manual, result: check(&kind) });
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
    pub fn install(&mut self, ctx: &egui::Context, package: &Path) -> Result<(), String> {
        match spawn_helper(&self.kind, package, &self.exe) {
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
                Message::Downloaded(Ok(package)) if running > 0 => {
                    self.step = Step::Confirm(package);
                    None
                }
                Message::Downloaded(Ok(package)) => self.install(ctx, &package).err(),
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
            asset: asset_name(version, &InstallKind::Msi),
        }
    }

    fn updater() -> Updater {
        Updater::new(InstallKind::Msi, PathBuf::from("factory.exe"), PathBuf::from("updates"))
    }

    fn all_kinds() -> [InstallKind; 4] {
        [
            InstallKind::Msi,
            InstallKind::MacApp(PathBuf::from("/Applications/Factory.app")),
            InstallKind::AppImage(PathBuf::from("/home/jo/factory.AppImage")),
            InstallKind::Deb,
        ]
    }

    fn linux<'a>(exe: &'a str, appimage: Option<&'a str>, appimage_is_file: bool) -> HostFacts<'a> {
        HostFacts {
            os: "linux",
            exe: Path::new(exe),
            local_app_data: None,
            home: Some(Path::new("/home/jo")),
            appimage: appimage.map(Path::new),
            appimage_is_file,
        }
    }

    fn mac(exe: &str) -> HostFacts<'_> {
        HostFacts {
            os: "macos",
            exe: Path::new(exe),
            local_app_data: None,
            home: Some(Path::new("/Users/jo")),
            appimage: None,
            appimage_is_file: false,
        }
    }

    #[test]
    fn every_kind_of_install_has_its_own_exact_asset() {
        let names = all_kinds().map(|kind| asset_name("1.2.3", &kind));
        assert_eq!(
            names,
            ["factory-1.2.3-x64.msi", "factory-1.2.3-arm64.dmg", "factory-1.2.3-x64.AppImage", "factory-1.2.3-x64.deb"]
        );
    }

    #[test]
    fn a_release_only_counts_if_it_has_this_installs_asset() {
        for kind in all_kinds() {
            let own = asset_name("0.3.0", &kind);
            let found = parse_release(&release("v0.3.0", &[&own]), "0.2.0", &kind).unwrap().unwrap();
            assert_eq!(found.asset, own);

            let others: Vec<String> =
                all_kinds().iter().filter(|other| **other != kind).map(|other| asset_name("0.3.0", other)).collect();
            let others: Vec<&str> = others.iter().map(String::as_str).collect();
            assert_eq!(parse_release(&release("v0.3.0", &others), "0.2.0", &kind).unwrap(), None, "{kind:?}");
        }
    }

    #[test]
    fn the_msi_install_is_detected_from_the_install_folder() {
        // Forward slashes, because only Windows splits a path on backslashes and CI runs this everywhere.
        let local = Some(Path::new("C:/Users/Jo/AppData/Local"));
        let windows = |exe: &'static str, local_app_data: Option<&'static Path>| {
            detect_install(&HostFacts {
                os: "windows",
                exe: Path::new(exe),
                local_app_data,
                home: None,
                appimage: None,
                appimage_is_file: false,
            })
        };
        let dir = Path::new("C:/Users/Jo/AppData/Local/Programs/Factory");
        assert_eq!(
            windows("C:/Users/Jo/AppData/Local/Programs/Factory/factory.exe", local),
            Some((InstallKind::Msi, dir.join("factory.exe")))
        );
        assert_eq!(windows("C:/src/factory/target/release/factory.exe", local), None);
        assert_eq!(windows("C:/Users/Jo/AppData/Local/Programs/Factory/factory.exe", None), None);
    }

    #[test]
    fn a_mac_app_counts_only_in_an_applications_folder() {
        let exe = "/Applications/Factory.app/Contents/MacOS/factory";
        assert_eq!(
            detect_install(&mac(exe)),
            Some((InstallKind::MacApp(PathBuf::from("/Applications/Factory.app")), PathBuf::from(exe)))
        );
        let exe = "/Users/jo/Applications/Factory.app/Contents/MacOS/factory";
        assert_eq!(
            detect_install(&mac(exe)),
            Some((InstallKind::MacApp(PathBuf::from("/Users/jo/Applications/Factory.app")), PathBuf::from(exe)))
        );
    }

    #[test]
    fn a_mac_app_anywhere_else_has_no_updater() {
        for exe in [
            "/Volumes/Factory 0.5.0/Factory.app/Contents/MacOS/factory",
            "/private/var/folders/ab/AppTranslocation/1234/d/Factory.app/Contents/MacOS/factory",
            "/Users/other/Applications/Factory.app/Contents/MacOS/factory",
            "/Users/jo/projects/factory/target/release/factory",
            "/Applications/Other.app/Contents/MacOS/factory",
            "/Applications/Factory.app/Contents/MacOS/helper",
            "/Applications/Factory.app/Contents/Resources/factory",
            "/usr/bin/factory",
        ] {
            assert_eq!(detect_install(&mac(exe)), None, "{exe}");
        }
    }

    #[test]
    fn an_appimage_needs_an_absolute_path_to_a_real_file() {
        let exe = "/tmp/.mount_factory/usr/bin/factory";
        let target = "/home/jo/factory.AppImage";
        assert_eq!(
            detect_install(&linux(exe, Some(target), true)),
            Some((InstallKind::AppImage(PathBuf::from(target)), PathBuf::from(exe)))
        );
        assert_eq!(detect_install(&linux(exe, Some(target), false)), None, "not a file");
        assert_eq!(detect_install(&linux(exe, Some("factory.AppImage"), true)), None, "relative");
    }

    #[test]
    fn a_deb_install_is_the_system_binary_outside_any_appimage() {
        assert_eq!(detect_install(&linux(DEB_EXE, None, false)), Some((InstallKind::Deb, PathBuf::from(DEB_EXE))));
        assert_eq!(detect_install(&linux(DEB_EXE, Some("missing.AppImage"), false)), None);
    }

    #[test]
    fn source_builds_and_other_systems_have_no_updater() {
        assert_eq!(detect_install(&linux("/home/jo/factory/target/release/factory", None, false)), None);
        assert_eq!(detect_install(&linux("/usr/local/bin/factory", None, false)), None);
        let bsd = HostFacts { os: "freebsd", ..linux(DEB_EXE, None, false) };
        assert_eq!(detect_install(&bsd), None);
    }

    #[test]
    fn sh_quoting_survives_single_quotes() {
        assert_eq!(sh_quote("/home/jo smith"), "'/home/jo smith'");
        assert_eq!(sh_quote("/home/O'Brien"), r"'/home/O'\''Brien'");
    }

    /// Fails unless every step is in the script, in this order.
    fn assert_in_order(script: &str, steps: &[&str]) {
        let mut from = 0;
        for step in steps {
            let at = script[from..].find(step).unwrap_or_else(|| panic!("no {step:?} after byte {from} in:\n{script}"));
            from += at + step.len();
        }
    }

    #[test]
    fn the_mac_helper_swaps_the_app_and_restores_it_if_the_move_fails() {
        let script = mac_helper_script(
            4242,
            Path::new("/Users/O'Brien/updates/factory-0.5.0-arm64.dmg"),
            Path::new("/Users/O'Brien/Applications/Factory.app"),
        );
        assert!(script.contains(r"dmg='/Users/O'\''Brien/updates/factory-0.5.0-arm64.dmg'"));
        assert!(script.contains(r"app='/Users/O'\''Brien/Applications/Factory.app'"));
        assert_in_order(
            &script,
            &[
                "kill -0 4242",
                "hdiutil attach -nobrowse -readonly -mountpoint \"$mount\" \"$dmg\"",
                "ditto \"$mount/Factory.app\" \"$new\"",
                "mv \"$app\" \"$old\"",
                "mv \"$new\" \"$app\"",
                "rm -rf \"$old\"",
                "else",
                "mv \"$old\" \"$app\"",
                "hdiutil detach \"$mount\"",
                "open \"$app\"",
            ],
        );
        assert!(script.contains("new=\"$app.new\""));
    }

    #[test]
    fn the_appimage_helper_clears_the_runtimes_variables_swaps_then_relaunches() {
        let script = appimage_helper_script(
            4242,
            Path::new("/home/O'Brien/updates/factory-0.5.0-x64.AppImage"),
            Path::new("/home/O'Brien/factory.AppImage"),
        );
        assert!(script.contains(r"package='/home/O'\''Brien/updates/factory-0.5.0-x64.AppImage'"));
        assert!(script.contains(r"target='/home/O'\''Brien/factory.AppImage'"));
        assert_in_order(
            &script,
            &[
                "unset APPIMAGE APPDIR ARGV0",
                "kill -0 4242",
                "cp \"$package\" \"$swap\"",
                "chmod +x \"$swap\"",
                "mv -f \"$swap\" \"$target\"",
                "rm -f \"$swap\"",
                "nohup \"$target\"",
            ],
        );
    }

    #[test]
    fn the_deb_helper_installs_through_polkit_then_relaunches_whatever_happened() {
        let script = deb_helper_script(4242, Path::new("/home/O'Brien/updates/factory-0.5.0-x64.deb"));
        assert_in_order(
            &script,
            &[
                "kill -0 4242",
                r"pkexec apt-get install -y '/home/O'\''Brien/updates/factory-0.5.0-x64.deb'",
                "nohup '/usr/bin/factory'",
            ],
        );
        // Nothing stops the script between a failed pkexec and the relaunch.
        assert!(!script.contains("set -e") && !script.contains("&&") && !script.contains("exit"));
    }

    #[test]
    fn each_kind_gets_the_helper_for_it() {
        let package = Path::new("/tmp/package");
        let exe = Path::new("/tmp/exe");
        for kind in all_kinds() {
            let cmd = helper_command(&kind, 1, package, exe);
            let args: Vec<_> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            if kind == InstallKind::Msi {
                assert_eq!(cmd.get_program(), "powershell");
                assert_eq!(args, helper_args(1, package, exe));
            } else {
                assert_eq!(cmd.get_program(), "sh");
                assert_eq!(args, ["-c".to_string(), shell_helper_script(&kind, 1, package).unwrap()]);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn sh_can_parse_every_helper_script() {
        let package = Path::new("/home/O'Brien/a b/package");
        for kind in all_kinds() {
            let Some(script) = shell_helper_script(&kind, 1, package) else { continue };
            let output = command("sh").args(["-n", "-c", script.as_str()]).output().unwrap();
            assert!(output.status.success(), "{kind:?}: {}", String::from_utf8_lossy(&output.stderr));
        }
    }

    /// The ID of a process that has already exited, so the helper doesn't wait.
    #[cfg(unix)]
    fn finished_pid() -> u32 {
        let mut child = command("true").spawn().unwrap();
        child.wait().unwrap();
        child.id()
    }

    #[cfg(unix)]
    fn write_fake_appimage(path: &Path, label: &str, marker: &Path) {
        let marker = sh_path(marker);
        fs::write(path, format!("#!/bin/sh\necho \"{label} ${{APPIMAGE:-cleared}}\" > {marker}\n")).unwrap();
    }

    #[cfg(unix)]
    fn run_appimage_helper(package: &Path, target: &Path) {
        let script = appimage_helper_script(finished_pid(), package, target);
        let status = command("sh")
            .args(["-c", script.as_str()])
            .env("APPIMAGE", "/stale")
            .env("APPDIR", "/stale")
            .env("ARGV0", "stale")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    fn wait_for_marker(marker: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match fs::read_to_string(marker) {
                Ok(text) if !text.is_empty() => return text,
                _ => {
                    assert!(Instant::now() < deadline, "nothing was relaunched");
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    #[cfg(unix)]
    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_appimage_helper_swaps_the_file_and_relaunches_it_with_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("it's here");
        fs::create_dir_all(&root).unwrap();
        let (target, package, marker) = (root.join("factory.AppImage"), root.join("new.AppImage"), root.join("marker"));
        write_fake_appimage(&target, "old", &marker);
        make_executable(&target);
        write_fake_appimage(&package, "new", &marker);

        run_appimage_helper(&package, &target);

        assert_eq!(wait_for_marker(&marker), "new cleared\n");
        assert_eq!(fs::read(&target).unwrap(), fs::read(&package).unwrap());
        assert_eq!(file_names(&root), ["factory.AppImage", "marker", "new.AppImage"], "no temp file left");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_appimage_swap_keeps_and_relaunches_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let (target, marker) = (dir.path().join("factory.AppImage"), dir.path().join("marker"));
        write_fake_appimage(&target, "old", &marker);
        make_executable(&target);

        run_appimage_helper(&dir.path().join("missing.AppImage"), &target);

        assert_eq!(wait_for_marker(&marker), "old cleared\n");
        assert_eq!(file_names(dir.path()), ["factory.AppImage", "marker"], "no temp file left");
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
        let found = parse_release(&json, "0.2.0", &InstallKind::Msi).unwrap().unwrap();
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
        let found = parse_release(&release("0.3.0", &["factory-0.3.0-x64.msi"]), "0.2.0", &InstallKind::Msi).unwrap().unwrap();
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
            assert_eq!(parse_release(&release("v0.3.0", &assets), "0.2.0", &InstallKind::Msi).unwrap(), None, "{assets:?}");
        }
    }

    #[test]
    fn an_older_equal_or_unparseable_tag_is_no_update() {
        for tag in ["v0.1.0", "v0.2.0", "latest"] {
            let json = release(tag, &[&format!("factory-{}-x64.msi", tag.trim_start_matches('v'))]);
            assert_eq!(parse_release(&json, "0.2.0", &InstallKind::Msi).unwrap(), None, "{tag}");
        }
    }

    #[test]
    fn unreadable_output_is_an_error() {
        assert!(parse_release("not json", "0.2.0", &InstallKind::Msi).is_err());
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

    /// Windows only, because these paths only have a parent where backslashes separate.
    #[cfg(windows)]
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
        for kind in all_kinds() {
            let asset = format!("dist/{}", asset_name("$version", &kind));
            assert!(workflow.contains(&asset), "{asset}");
        }
        for runner in ["runs-on: windows-latest", "runs-on: macos-latest", "runs-on: ubuntu-22.04"] {
            assert!(workflow.contains(runner), "{runner}");
        }
        assert!(workflow.contains("needs: [windows, macos, linux]"));
        assert!(!workflow.contains("continue-on-error"));
        assert!(workflow.contains("contents: write"));
        assert!(workflow.contains("cargo test --locked"));
        assert!(workflow.contains("cargo build --release --locked"));
        assert!(workflow.contains(
            r#"gh release create "v$version" "${assets[@]}" --draft --target "$GITHUB_SHA" --title "factory v$version" --notes "Release notes pending.""#
        ));
    }

    #[test]
    fn the_packaging_scripts_build_the_assets_the_updater_looks_for() {
        let macos = include_str!("../packaging/macos.sh");
        assert!(macos.contains(&asset_name("$version", &InstallKind::MacApp(PathBuf::new()))));
        assert!(macos.contains("Factory.app"));
        assert!(macos.contains("$app/Contents/MacOS/factory"));
        assert!(macos.contains("$app/Contents/Resources/factory.icns"));
        assert!(macos.contains("codesign --force --deep --sign -"));
        assert!(macos.contains("ln -s /Applications"));
        assert!(macos.contains("-format UDZO"));

        let appimage = include_str!("../packaging/appimage.sh");
        assert!(appimage.contains(&asset_name("$version", &InstallKind::AppImage(PathBuf::new()))));
        assert!(appimage.contains("$APPIMAGETOOL_VERSION"));
        assert!(include_str!("../.github/workflows/release.yml").contains("APPIMAGETOOL_VERSION: \"13\""));

        let deb = include_str!("../packaging/deb.sh");
        assert!(deb.contains(&asset_name("$version", &InstallKind::Deb)));
        assert!(deb.contains("--root-owner-group"));
        assert!(deb.contains("usr/bin/factory"));
        assert!(deb.contains("usr/share/applications/factory.desktop"));
        assert!(deb.contains("usr/share/icons/hicolor/256x256/apps/factory.png"));
    }

    #[test]
    fn the_package_metadata_matches_the_app() {
        let plist = include_str!("../packaging/Info.plist");
        assert!(plist.contains("<string>factory</string>"));
        assert!(plist.contains("@VERSION@"));
        assert!(plist.contains("NSHighResolutionCapable"));

        let desktop = include_str!("../packaging/factory.desktop");
        assert!(desktop.contains("Exec=factory"));
        assert!(desktop.contains("Icon=factory"));
        assert!(desktop.contains("StartupWMClass=factory"));

        let control = include_str!("../packaging/deb-control");
        for package in ["git", "make", "libc6", "libgl1", "libegl1", "libxkbcommon0", "libx11-6", "libxcursor1", "libxi6", "libxrandr2", "libwayland-client0"] {
            assert!(control.contains(package), "{package}");
        }
    }

    #[test]
    fn the_appimage_doesnt_change_what_git_make_and_claude_load() {
        assert!(!include_str!("../packaging/AppRun").contains("LD_LIBRARY_PATH"));
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
