//! Grok's session store under `<GROK_HOME>` (DESIGN.md §6.4): one directory per session,
//! `sessions/<urlencoded cwd>/<id>/`, holding `summary.json` and `updates.jsonl`, and
//! `active_sessions.json`, the live TUIs.

use crate::agents::decode::or_default;
use crate::model::{Error, ErrorCode, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Summary {
    pub info: SummaryInfo,
    /// Set by `_x.ai/session/rename` (with `title_is_manual: true`) or generated.
    #[serde(default, deserialize_with = "or_default")]
    pub generated_title: Option<String>,
    /// `headless` for `-p`; null for the TUI and ACP.
    #[serde(default, deserialize_with = "or_default")]
    pub session_kind: Option<String>,
    #[serde(default, deserialize_with = "or_default")]
    updated_at: Option<String>,
}

impl Summary {
    /// `updated_at`, empty when absent: the `ls` order and cursor.
    pub fn at(&self) -> &str {
        self.updated_at.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Deserialize)]
pub struct SummaryInfo {
    pub id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub cwd: Option<String>,
}

/// One row of `active_sessions.json`: a live TUI (DESIGN.md §6.4).
#[derive(Debug, Deserialize)]
pub struct Active {
    pub session_id: String,
    pub pid: u32,
}

pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// The session directory, found by id across the cwd groups.
fn find(home: &Path, id: &str) -> Option<PathBuf> {
    std::fs::read_dir(sessions_dir(home))
        .ok()?
        .flatten()
        .map(|d| d.path().join(id))
        .find(|p| p.is_dir())
}

pub fn require(home: &Path, id: &str) -> Result<PathBuf> {
    find(home, id).ok_or_else(|| {
        Error::new(
            ErrorCode::Precondition,
            format!(
                "no Grok session {id} under {}",
                sessions_dir(home).display()
            ),
        )
    })
}

/// `summary.json` of the session directory `dir`.
pub fn summary(dir: &Path) -> Result<Summary> {
    let path = dir.join("summary.json");
    let text = std::fs::read_to_string(&path).map_err(|e| Error::transport(path.display(), e))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::new(ErrorCode::Precondition, format!("{}: {e}", path.display())))
}

/// Every session directory with a `summary.json`, and the summary; one that cannot be read
/// is left out with a warning.
pub fn summaries(home: &Path) -> Vec<(PathBuf, Summary)> {
    let groups = std::fs::read_dir(sessions_dir(home)).into_iter().flatten();
    let dirs = groups.flatten().flat_map(|group| {
        std::fs::read_dir(group.path())
            .into_iter()
            .flatten()
            .flatten()
    });
    dirs.map(|d| d.path())
        .filter(|dir| dir.join("summary.json").is_file())
        .filter_map(|dir| match summary(&dir) {
            Ok(s) => Some((dir, s)),
            Err(e) => {
                tracing::warn!("{e}");
                None
            }
        })
        .collect()
}

/// Rows of `active_sessions.json` (absent: none).
pub fn active(home: &Path) -> Result<Vec<Active>> {
    let path = home.join("active_sessions.json");
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| {
            Error::new(
                ErrorCode::CapMissing,
                format!(
                    "cannot check whether a Grok TUI holds the session: {}: {e}",
                    path.display()
                ),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(Error::transport(path.display(), e)),
    }
}
