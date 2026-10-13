use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use futures::{FutureExt as _, future::Shared};
use gpui::{AppContext as _, Context, Task};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedCodexTab {
    pub id: Uuid,
    pub title: String,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedTabs {
    version: u32,
    worktrees: BTreeMap<PathBuf, Vec<SavedCodexTab>>,
}

impl Default for SavedTabs {
    fn default() -> Self {
        Self {
            version: 1,
            worktrees: BTreeMap::new(),
        }
    }
}

impl SavedTabs {
    fn load(path: &Path) -> Result<Self> {
        let content = match std::fs::read(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let saved: Self = serde_json::from_slice(&content)?;
        anyhow::ensure!(
            saved.version == 1,
            "Unsupported ACP tab file version {}",
            saved.version
        );
        let mut ids = HashSet::new();
        let mut sessions = HashSet::new();
        for (directory, tabs) in &saved.worktrees {
            anyhow::ensure!(
                directory.is_absolute(),
                "ACP worktree path must be absolute"
            );
            for tab in tabs {
                anyhow::ensure!(!tab.session_id.is_empty(), "Empty ACP session ID");
                validate_title(&tab.title)?;
                anyhow::ensure!(ids.insert(tab.id), "Duplicate ACP tab ID");
                anyhow::ensure!(sessions.insert(&tab.session_id), "Duplicate ACP session ID");
            }
        }
        Ok(saved)
    }

    fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

pub(crate) fn validate_title(title: &str) -> Result<()> {
    anyhow::ensure!(!title.trim().is_empty(), "ACP tab title must not be empty");
    anyhow::ensure!(
        !title.chars().any(char::is_control),
        "ACP tab title must not contain control characters"
    );
    Ok(())
}

pub(crate) struct CodexAcpPersistence {
    path: PathBuf,
    saved: SavedTabs,
    disabled: bool,
    quitting: bool,
    error: Arc<Mutex<Option<String>>>,
    write_task: Option<Shared<Task<()>>>,
}

impl CodexAcpPersistence {
    pub(crate) fn new(path: PathBuf) -> Self {
        let result = SavedTabs::load(&path)
            .with_context(|| format!("Could not load ACP tabs from {}", path.display()));
        let (saved, disabled, error) = match result {
            Ok(saved) => (saved, false, None),
            Err(error) => {
                log::error!("{error:#}");
                (
                    SavedTabs::default(),
                    true,
                    Some(format!(
                        "{error:#}. ACP tab persistence is disabled until restart."
                    )),
                )
            }
        };
        Self {
            path,
            saved,
            disabled,
            quitting: false,
            error: Arc::new(Mutex::new(error)),
            write_task: None,
        }
    }

    pub(crate) fn tabs(&self, directory: &Path) -> Vec<SavedCodexTab> {
        self.saved
            .worktrees
            .get(directory)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    pub(crate) fn replace(
        &mut self,
        directory: PathBuf,
        mut tabs: Vec<SavedCodexTab>,
        cx: &mut Context<Self>,
    ) {
        if self.disabled || self.quitting {
            return;
        }
        if let Some(previous) = self.saved.worktrees.get(&directory) {
            tabs.sort_by_key(|tab| {
                previous
                    .iter()
                    .position(|saved| saved.id == tab.id)
                    .unwrap_or(usize::MAX)
            });
        }
        if self.tabs(&directory) == tabs && self.error().is_none() {
            return;
        }
        if tabs.is_empty() {
            self.saved.worktrees.remove(&directory);
        } else {
            self.saved.worktrees.insert(directory, tabs);
        }
        self.persist(cx);
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let previous = self.write_task.take();
        let snapshot = self.saved.clone();
        let path = self.path.clone();
        let error = self.error.clone();
        let task = cx
            .background_spawn(async move {
                if let Some(previous) = previous {
                    previous.await;
                }
                match snapshot.write(&path) {
                    Ok(()) => *error.lock() = None,
                    Err(failure) => {
                        let message =
                            format!("Could not save ACP tabs to {}: {failure:#}", path.display());
                        log::error!("{message}");
                        *error.lock() = Some(message);
                    }
                }
            })
            .shared();
        self.write_task = Some(task.clone());
        cx.spawn(async move |this, cx| {
            task.await;
            if let Err(error) = this.update(cx, |_, cx| cx.notify()) {
                log::debug!("ACP tab store released after save: {error:#}");
            }
        })
        .detach();
    }

    pub(crate) fn finish(&mut self) -> Option<Shared<Task<()>>> {
        self.quitting = true;
        self.write_task.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn tab(session: &str) -> SavedCodexTab {
        SavedCodexTab {
            id: Uuid::new_v4(),
            title: format!("Codex {session}"),
            session_id: session.into(),
        }
    }

    #[test]
    fn rejects_corrupt_versions_and_duplicate_sessions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("tabs.json");
        assert_eq!(SavedTabs::load(&path)?, SavedTabs::default());
        for content in [
            "broken".to_owned(),
            serde_json::json!({"version": 2, "worktrees": {}}).to_string(),
            serde_json::json!({"version": 1, "worktrees": {"/a": [tab("same"), tab("same")]}})
                .to_string(),
        ] {
            std::fs::write(&path, &content)?;
            let store = CodexAcpPersistence::new(path.clone());
            assert!(store.disabled);
            assert!(store.error().is_some());
            assert_eq!(std::fs::read_to_string(&path)?, content);
        }
        Ok(())
    }

    #[gpui::test]
    async fn queued_updates_close_and_quit_preserve_the_latest_snapshot(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("tabs.json");
        let store = cx.new(|_| CodexAcpPersistence::new(path.clone()));
        let first = tab("first");
        let second = tab("second");
        store.update(cx, |store, cx| {
            store.replace("/first".into(), vec![first.clone()], cx);
            store.replace("/second".into(), vec![second.clone()], cx);
            store.replace("/first".into(), Vec::new(), cx);
        });
        let _quit = store.update(cx, |_, cx| {
            cx.on_app_quit(|store, _| {
                let pending = store.finish();
                async move {
                    if let Some(pending) = pending {
                        pending.await;
                    }
                }
            })
        });
        cx.update(|cx| cx.shutdown());
        store.update(cx, |store, cx| {
            store.replace("/second".into(), Vec::new(), cx)
        });
        cx.run_until_parked();
        let restored = SavedTabs::load(&path).expect("saved snapshot");
        assert!(!restored.worktrees.contains_key(Path::new("/first")));
        assert_eq!(
            restored.worktrees.get(Path::new("/second")),
            Some(&vec![second])
        );
    }

    #[gpui::test]
    async fn failed_write_is_visible_and_can_be_retried(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let blocker = directory.path().join("parent");
        let path = blocker.join("tabs.json");
        let store = cx.new(|_| CodexAcpPersistence::new(path.clone()));
        std::fs::write(&blocker, "not a directory").expect("create blocker");
        let saved = tab("retry");
        store.update(cx, |store, cx| {
            store.replace("/arena".into(), vec![saved.clone()], cx)
        });
        store
            .read_with(cx, |store, _| store.write_task.clone())
            .expect("pending write")
            .await;
        assert!(store.read_with(cx, |store, _| store.error()).is_some());
        std::fs::remove_file(&blocker).expect("remove test blocker");
        store.update(cx, |store, cx| {
            store.replace("/arena".into(), vec![saved.clone()], cx)
        });
        store
            .read_with(cx, |store, _| store.write_task.clone())
            .expect("retry write")
            .await;
        assert!(store.read_with(cx, |store, _| store.error()).is_none());
        assert_eq!(
            SavedTabs::load(&path)
                .expect("saved tabs")
                .worktrees
                .get(Path::new("/arena")),
            Some(&vec![saved])
        );
    }
}
