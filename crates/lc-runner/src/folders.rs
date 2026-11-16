//! Where a Runner keeps its things.
//!
//! A Runner keeps one folder of its own:
//!
//! ```text
//! <runner folder>\
//!     assets\        media copied from the Asset Manager (kept across restarts)
//!     show.json      the last show, to start without a Director
//!     transport.json, tasks.json
//!     node_id        this Runner's identity
//!     runner.log     its log (read back by the Producer's Log button)
//! ```
//!
//! The folder is, by priority:
//!
//! 1. `--cache-dir <folder>` on the command line;
//! 2. the `LCPLAY_RUNNER_DIR` environment variable;
//! 3. `runner.dir` in `lcplay.json`, the settings file next to the LC Play
//!    programs (a relative path is relative to that folder) — what an
//!    installer will write;
//! 4. [`RUNNER_FOLDER`] in the folder the LC Play programs are in (the
//!    install folder): `…\LC Play\lc-runner\assets\` on Windows;
//! 5. if that folder cannot be written (e.g. an install under
//!    `C:\Program Files`), the user's local data folder:
//!    `%LOCALAPPDATA%\LC Play\lc-runner` on Windows,
//!    `~/.local/share/lcplay/lc-runner` elsewhere.
//!
//! A Runner started with a `--name` other than the default keeps its own
//! show, identity and log in a sub-folder (`runner-<name>`), so that several
//! Runners can be tried on one machine; the media folder is shared.

use std::path::{Path, PathBuf};

/// The Runner's folder in the install folder. Elsewhere than Windows the
/// program itself is called `lc-runner` (no `.exe`), so the folder cannot be.
pub const RUNNER_FOLDER: &str = if cfg!(windows) {
    "lc-runner"
} else {
    "lc-runner-data"
};

/// The settings file next to the programs.
pub const SETTINGS_FILE: &str = "lcplay.json";

/// The Runner's folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerFolders {
    /// Show cache, identity and log.
    pub state: PathBuf,
    /// Downloaded media.
    pub assets: PathBuf,
    /// How the base folder was chosen, for the log.
    pub origin: &'static str,
}

/// Work out the Runner's folders (see the module docs) and create them.
pub fn runner_folders(cli: Option<PathBuf>, name: &str) -> RunnerFolders {
    let install = install_dir();
    let (base, origin) = if let Some(dir) = cli {
        (dir, "--cache-dir")
    } else if let Some(dir) = std::env::var_os("LCPLAY_RUNNER_DIR").filter(|v| !v.is_empty()) {
        (PathBuf::from(dir), "LCPLAY_RUNNER_DIR")
    } else if let Some(dir) = install.as_deref().and_then(settings_runner_dir) {
        (dir, SETTINGS_FILE)
    } else {
        match install
            .map(|i| i.join(RUNNER_FOLDER))
            .filter(|d| writable(d))
        {
            Some(dir) => (dir, "install folder"),
            None => (user_data_dir().join("lc-runner"), "user data folder"),
        }
    };
    // An explicit folder is the Runner's own whatever its name.
    let state = if origin == "--cache-dir" || name == "runner" || name.is_empty() {
        base.clone()
    } else {
        base.join(format!("runner-{}", safe(name)))
    };
    let folders = RunnerFolders {
        assets: base.join("assets"),
        state,
        origin,
    };
    let _ = std::fs::create_dir_all(&folders.assets);
    let _ = std::fs::create_dir_all(&folders.state);
    folders
}

/// The folder the LC Play programs are in.
pub fn install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// `runner.dir` from the settings file in `install`, if there is one.
fn settings_runner_dir(install: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(install.join(SETTINGS_FILE)).ok()?;
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lc-runner: ignoring {SETTINGS_FILE}: {e}");
            return None;
        }
    };
    let dir = value.get("runner")?.get("dir")?.as_str()?;
    (!dir.is_empty()).then(|| install.join(dir))
}

/// Whether `dir` can be created and written to.
fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The user's local application data folder for LC Play.
fn user_data_dir() -> PathBuf {
    let from = |var: &str| {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        if let Some(local) = from("LOCALAPPDATA") {
            return local.join("LC Play");
        }
    }
    if let Some(data) = from("XDG_DATA_HOME") {
        return data.join("lcplay");
    }
    if let Some(home) = from("HOME") {
        return home.join(".local").join("share").join("lcplay");
    }
    std::env::temp_dir().join("lcplay")
}

/// A name usable as a folder name.
fn safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_folder_holds_everything() {
        let dir = std::env::temp_dir().join(format!("lcplay-folders-{}", std::process::id()));
        let f = runner_folders(Some(dir.clone()), "left");
        assert_eq!(
            (f.state.clone(), f.assets.clone()),
            (dir.clone(), dir.join("assets"))
        );
        assert!(f.assets.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_paths_are_relative_to_the_install_folder() {
        let dir = std::env::temp_dir().join(format!("lcplay-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SETTINGS_FILE),
            r#"{ "runner": { "dir": "data/runner" } }"#,
        )
        .unwrap();
        assert_eq!(settings_runner_dir(&dir), Some(dir.join("data/runner")));
        std::fs::write(dir.join(SETTINGS_FILE), "not json").unwrap();
        assert_eq!(settings_runner_dir(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(safe("../left wall"), "___left_wall");
    }
}
