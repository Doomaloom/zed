use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, Global};
use project::Project;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::WalkthroughJson;

pub const MAX_WALKTHROUGH_HISTORY: usize = 10;

/// A walkthrough that was shown before, as it was originally given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Seconds since the Unix epoch.
    pub shown_at: u64,
    pub walkthrough: WalkthroughJson,
}

impl HistoryEntry {
    pub(crate) fn now(walkthrough: WalkthroughJson) -> Self {
        let shown_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        Self {
            shown_at,
            walkthrough,
        }
    }
}

struct HistoryDirectoryOverride(PathBuf);

impl Global for HistoryDirectoryOverride {}

/// Changes where walkthrough histories are stored, instead of the
/// `walkthroughs` folder in Zed's data directory. Tests use this so they do
/// not write into the real data directory.
pub fn set_history_directory(directory: PathBuf, cx: &mut App) {
    cx.set_global(HistoryDirectoryOverride(directory));
}

fn history_directory(cx: &App) -> PathBuf {
    cx.try_global::<HistoryDirectoryOverride>().map_or_else(
        || paths::data_dir().join("walkthroughs"),
        |directory| directory.0.clone(),
    )
}

/// The file holding the history of walkthroughs shown for `project`, named by
/// a hash of its sorted visible worktree paths. `None` for projects that
/// cannot have a stable local file.
pub(crate) fn history_file(project: &Project, cx: &App) -> Option<PathBuf> {
    if !project.is_local() {
        return None;
    }
    let mut roots = project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return None;
    }
    roots.sort();
    let mut hasher = Sha256::new();
    for root in &roots {
        hasher.update(root.as_bytes());
        hasher.update([0]);
    }
    let key = hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(history_directory(cx).join(format!("{key}.json")))
}

pub(crate) async fn load_entries(fs: &dyn Fs, path: &Path) -> Result<Vec<HistoryEntry>> {
    if !fs.is_file(path).await {
        return Ok(Vec::new());
    }
    let text = fs
        .load(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut entries = serde_json::from_str::<Vec<HistoryEntry>>(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    entries.truncate(MAX_WALKTHROUGH_HISTORY);
    Ok(entries)
}

pub(crate) async fn save_entries(fs: &dyn Fs, path: &Path, entries: &[HistoryEntry]) -> Result<()> {
    if let Some(directory) = path.parent() {
        fs.create_dir(directory)
            .await
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    let text = serde_json::to_string_pretty(entries)?;
    fs.atomic_write(path.to_path_buf(), text)
        .await
        .with_context(|| format!("writing {}", path.display()))
}
