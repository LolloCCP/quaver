//! Replaces this program with another version of it from the releases on GitHub.

use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::Value;

use crate::soloist::fetch;

const RELEASES: &str = "https://api.github.com/repos/LolloCCP/quaver/releases?per_page=15";
const FILE: &str = "quaver.exe";
/// Which release this is. What was built from source is none, and is left alone.
pub const CURRENT: Option<&str> = option_env!("QUAVER_VERSION");
/// Tells a new copy to let the old one end first.
const WAIT: &str = "--wait";

#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub tag: String,
    pub about: String,
}

/// What can be installed, newest first.
pub fn versions(data_dir: &Path) -> Result<Vec<Version>, String> {
    let file = data_dir.join("versions.json");
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    fetch(RELEASES, &file)?;
    let json = std::fs::read(&file).map_err(|e| e.to_string())?;
    Ok(versions_in(&serde_json::from_slice(&json).map_err(|e| e.to_string())?))
}

fn versions_in(releases: &Value) -> Vec<Version> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let installable = |r: &&Value| r["assets"].as_array().is_some_and(|a| a.iter().any(|a| a["name"] == FILE));
    releases
        .as_array()
        .into_iter()
        .flatten()
        .filter(installable)
        .map(|r| {
            let date = text(&r["published_at"]);
            let what = text(&r["body"]);
            let what = what.trim_start_matches('\u{feff}').lines().next().unwrap_or_default();
            Version { tag: text(&r["tag_name"]), about: [date.get(..10).unwrap_or_default(), what].join(" · ") }
        })
        // The tag becomes part of an address and a line of the settings.
        .filter(|v| !v.tag.is_empty() && v.tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
        .collect()
}

/// What to install without being asked. A version that was chosen is kept.
pub fn wanted<'a>(versions: &'a [Version], chosen: &str, current: Option<&str>) -> Option<&'a Version> {
    let current = current?;
    let wanted = if chosen.is_empty() { versions.first() } else { versions.iter().find(|v| v.tag == chosen) };
    wanted.filter(|v| v.tag != current)
}

/// Puts a version where this one is; it is used from the next start.
pub fn install(tag: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = exe.with_extension("new");
    fetch(&format!("https://github.com/LolloCCP/quaver/releases/download/{tag}/{FILE}"), &new)?;
    if !std::fs::read(&new).is_ok_and(|bytes| bytes.starts_with(b"MZ")) {
        return Err("The download is not a program".into());
    }
    swap(&exe, &new).map_err(|e| e.to_string())
}

/// A running program can't be overwritten, but it can be renamed. It is kept, in case the new one doesn't start.
fn swap(exe: &Path, new: &Path) -> io::Result<()> {
    let old: PathBuf = exe.with_extension("old.exe");
    // Replaced a second time, the file is no longer the one that runs, which is the old one.
    if std::fs::rename(exe, &old).is_err() {
        return std::fs::rename(new, exe);
    }
    std::fs::rename(new, exe).inspect_err(|_| drop(std::fs::rename(&old, exe)))
}

/// Starts the installed version, which waits until this one is gone.
pub fn restart() {
    let started = std::env::current_exe().and_then(|exe| Command::new(exe).arg(WAIT).stdin(Stdio::piped()).spawn());
    match started {
        // Its input ends when Windows closes the other end, together with this program.
        Ok(child) => std::mem::forget(child),
        Err(e) => log::error!("restart: {e}"),
    }
}

pub fn wait() {
    if std::env::args().any(|a| a == WAIT) {
        let _ = io::copy(&mut io::stdin(), &mut io::sink());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_what_can_be_installed_is_offered() {
        let release = |tag: &str, file: &str| json!({ "tag_name": tag, "published_at": "2026-11-26T08:00:00Z", "body": "What is new\n\nmore", "assets": [{ "name": file }] });
        let found = versions_in(&json!([release("v0.1.3", FILE), release("v0.1.2", "notes.txt"), release("../evil", FILE), release("a\nb", FILE), release("v0.1.1", FILE)]));
        assert_eq!(found.iter().map(|v| v.tag.as_str()).collect::<Vec<_>>(), ["v0.1.3", "v0.1.1"]);
        assert_eq!(found[0].about, "2026-11-26 · What is new");
        assert!(versions_in(&json!({ "message": "rate limit" })).is_empty());
    }

    #[test]
    fn the_newest_is_wanted_unless_another_was_chosen() {
        let all = ["3", "2", "1"].map(|t| Version { tag: t.into(), about: String::new() });
        let tag = |chosen, current| wanted(&all, chosen, current).map(|v| v.tag.as_str());
        assert_eq!(tag("", Some("1")), Some("3"));
        assert_eq!(tag("", Some("3")), None);
        assert_eq!(tag("2", Some("2")), None, "an older one that was chosen stays");
        assert_eq!(tag("2", Some("3")), Some("2"));
        assert_eq!(tag("gone", Some("2")), None);
        assert_eq!(tag("", None), None, "built from source");
        assert!(wanted(&[], "", Some("1")).is_none());
    }

    #[test]
    fn a_running_program_is_replaced_twice() {
        let dir = std::env::temp_dir().join(format!("quaver-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (exe, new) = (dir.join("a.exe"), dir.join("a.new"));
        let ping = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("ping.exe");
        std::fs::copy(ping, &exe).unwrap();
        let mut running = Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        for version in ["one", "two"] {
            std::fs::write(&new, version).unwrap();
            swap(&exe, &new).unwrap();
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), version);
        }
        running.kill().unwrap();
        running.wait().unwrap();
        assert!(swap(&exe, &new).is_err(), "nothing to install");
        assert!(exe.exists(), "and nothing lost");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
