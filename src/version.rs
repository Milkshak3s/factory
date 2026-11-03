//! Working out the next release version, and writing it into the project's manifest.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bump {
    #[default]
    Patch,
    Minor,
    Major,
}

impl Bump {
    pub fn label(self) -> &'static str {
        match self {
            Bump::Patch => "patch",
            Bump::Minor => "minor",
            Bump::Major => "major",
        }
    }
}

/// Bumps an `x.y.z` version, ignoring any leading `v`.
pub fn bumped(current: &str, bump: Bump) -> Result<String, String> {
    let invalid = || format!("`{current}` is not an x.y.z version");
    let numbers: Vec<u64> = current
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().map_err(|_| invalid()))
        .collect::<Result<_, _>>()?;
    let [major, minor, patch] = numbers.as_slice() else { return Err(invalid()) };
    Ok(match bump {
        Bump::Patch => format!("{major}.{minor}.{}", patch + 1),
        Bump::Minor => format!("{major}.{}.0", minor + 1),
        Bump::Major => format!("{}.0.0", major + 1),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Manifest {
    Cargo,
    Npm,
}

/// The version recorded in the project's manifest, if it has one Factory can update.
pub fn read_manifest(dir: &Path) -> Option<(Manifest, String)> {
    if let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) {
        return cargo_version_line(&text).map(|(_, version)| (Manifest::Cargo, version));
    }
    let text = fs::read_to_string(dir.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((Manifest::Npm, value["version"].as_str()?.to_string()))
}

/// Finds `version = "x"` in `[package]`. Workspace-inherited versions don't count.
fn cargo_version_line(text: &str) -> Option<(usize, String)> {
    let mut in_package = false;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package && line.starts_with("version") {
            let (key, value) = line.split_once('=')?;
            if key.trim() == "version" {
                return Some((index, value.trim().trim_matches('"').to_string()));
            }
        }
    }
    None
}

/// Rewrites the `[package]` version in place, keeping the rest of the file byte-for-byte.
pub fn set_cargo_version(dir: &Path, version: &str) -> Result<(), String> {
    let path = dir.join("Cargo.toml");
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let (target, _) = cargo_version_line(&text).ok_or("Cargo.toml has no [package] version")?;
    let mut output = String::with_capacity(text.len());
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index == target {
            let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
            output.push_str(&format!("version = \"{version}\"{ending}"));
        } else {
            output.push_str(line);
        }
    }
    fs::write(&path, output).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_each_part_and_resets_the_lower_ones() {
        assert_eq!(bumped("0.3.1", Bump::Patch).unwrap(), "0.3.2");
        assert_eq!(bumped("v0.3.1", Bump::Minor).unwrap(), "0.4.0");
        assert_eq!(bumped("0.3.1", Bump::Major).unwrap(), "1.0.0");
        assert!(bumped("nightly", Bump::Patch).is_err());
        assert!(bumped("1.2", Bump::Patch).is_err());
    }

    #[test]
    fn only_the_package_version_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[package]\r\nname = \"x\"\r\nversion = \"0.1.0\"\r\n\r\n[dependencies]\r\nfoo = { version = \"9.9.9\" }\r\n";
        fs::write(dir.path().join("Cargo.toml"), text).unwrap();
        assert_eq!(read_manifest(dir.path()), Some((Manifest::Cargo, "0.1.0".to_string())));

        set_cargo_version(dir.path(), "0.2.0").unwrap();
        let updated = fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert_eq!(updated, text.replace("0.1.0", "0.2.0"));
    }

    #[test]
    fn inherited_workspace_versions_are_not_treated_as_a_manifest_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\nversion.workspace = true\n").unwrap();
        assert_eq!(read_manifest(dir.path()), None);
    }

    #[test]
    fn reads_the_npm_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"x","version":"1.2.3"}"#).unwrap();
        assert_eq!(read_manifest(dir.path()), Some((Manifest::Npm, "1.2.3".to_string())));
    }
}
