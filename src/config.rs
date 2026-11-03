//! The project registry. It lives outside every repo on purpose: the agent works inside a
//! repo, so nothing it edits there can change what the pipeline enforces.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub default_branch: String,
    /// Runs inside the worktree before anything else, e.g. `npm ci`.
    pub setup_command: String,
    /// For projects with no release workflow: runs once the draft exists, with the tag in `FACTORY_TAG`.
    pub release_command: String,
    pub model: String,
    pub permission_mode: String,
    pub allowed_tools: String,
    pub max_attempts: u32,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: PathBuf::new(),
            default_branch: "main".to_string(),
            setup_command: String::new(),
            release_command: String::new(),
            model: String::new(),
            permission_mode: "acceptEdits".to_string(),
            allowed_tools: String::new(),
            max_attempts: 3,
        }
    }
}

impl Project {
    pub fn detect(path: &Path) -> Self {
        Self {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            path: path.to_path_buf(),
            default_branch: git::default_branch(path),
            ..Default::default()
        }
    }

    /// Filesystem-safe form of the name, for worktree and cache directories.
    pub fn slug(&self) -> String {
        self.name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect()
    }
}

/// Repos to add for a picked folder: the folder itself, or else the repos directly inside it.
pub fn discover(folder: &Path) -> Vec<Project> {
    if git::is_repo(folder) {
        return vec![Project::detect(folder)];
    }
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    let mut found: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").exists())
        .map(|path| Project::detect(&path))
        .collect();
    found.sort_by_key(|project| project.name.to_lowercase());
    found
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Overrides where the `claude` binary is looked up.
    pub claude_path: Option<PathBuf>,
    pub projects: Vec<Project>,
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn locate() -> Self {
        let env_dir = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let home = env_dir("USERPROFILE").or_else(|| env_dir("HOME")).unwrap_or_default();
        let config = env_dir("APPDATA").unwrap_or_else(|| home.join(".config"));
        let data = env_dir("LOCALAPPDATA").unwrap_or_else(|| home.join(".local").join("share"));
        Self {
            config_file: config.join("factory").join("config.toml"),
            data_dir: data.join("factory"),
        }
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.data_dir.join("runs")
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("config.toml");
        let config = Config {
            claude_path: None,
            projects: vec![Project {
                name: "demo".into(),
                model: "haiku".into(),
                ..Default::default()
            }],
        };
        config.save(&file).unwrap();
        assert_eq!(Config::load(&file).unwrap().projects, config.projects);
    }

    #[test]
    fn missing_config_is_empty_and_corrupt_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        assert!(Config::load(&file).unwrap().projects.is_empty());
        fs::write(&file, "projects = 3").unwrap();
        assert!(Config::load(&file).is_err());
    }

    #[test]
    fn settings_from_older_versions_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        fs::write(&file, "[[projects]]\nname = \"x\"\ntest_command = \"cargo test\"\npush_target = \"branch\"\n").unwrap();
        assert_eq!(Config::load(&file).unwrap().projects[0].name, "x");
    }
}
