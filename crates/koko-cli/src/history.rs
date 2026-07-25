//! Bounded, private interactive history with session admission controls.

use crate::continuation::normalize_continuations;
use crate::registry::COMMAND_REGISTRY;
use reedline::{
    CommandLineSearch, FileBackedHistory, History, HistoryItem, HistoryItemId, HistorySessionId,
    ReedlineError, ReedlineErrorVariants, SearchDirection, SearchQuery,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("cannot initialize history `{path}`: {source}")]
    Initialize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("history error: {0}")]
    Reedline(#[from] ReedlineError),
}

#[derive(Debug)]
struct SharedHistory {
    store: FileBackedHistory,
    path: Option<PathBuf>,
    enabled: bool,
    hard_disabled: bool,
    skip_next: bool,
    suppress_next: bool,
    clear_confirmed: bool,
}

/// Application-side control handle for the history owned by Reedline.
#[derive(Debug, Clone)]
pub struct HistoryController {
    shared: Arc<Mutex<SharedHistory>>,
}

impl HistoryController {
    pub fn open(
        path: Option<PathBuf>,
        capacity: usize,
        enabled: bool,
        hard_disabled: bool,
    ) -> Result<(Self, KokoHistory), HistoryError> {
        let (store, path) = if hard_disabled {
            (FileBackedHistory::new(capacity)?, None)
        } else if let Some(path) = path {
            prepare_private_file(&path).map_err(|source| HistoryError::Initialize {
                path: path.clone(),
                source,
            })?;
            (
                FileBackedHistory::with_file(capacity, path.clone())?,
                Some(path),
            )
        } else {
            (FileBackedHistory::new(capacity)?, None)
        };
        let shared = Arc::new(Mutex::new(SharedHistory {
            store,
            path,
            enabled: enabled && !hard_disabled,
            hard_disabled,
            skip_next: false,
            suppress_next: false,
            clear_confirmed: false,
        }));
        Ok((
            Self {
                shared: Arc::clone(&shared),
            },
            KokoHistory { shared },
        ))
    }

    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        let mut shared = self.lock();
        if !shared.hard_disabled {
            shared.enabled = enabled;
        }
        shared.enabled
    }

    pub fn skip_next(&self) {
        self.lock().skip_next = true;
    }

    /// Suppress one editor submission, including a meta command.
    pub fn suppress_next(&self) {
        self.lock().suppress_next = true;
    }

    pub fn confirm_clear(&self) {
        self.lock().clear_confirmed = true;
    }

    pub fn take_clear_confirmation(&self) -> bool {
        let mut shared = self.lock();
        std::mem::take(&mut shared.clear_confirmed)
    }

    pub fn newest(&self, count: Option<usize>) -> Result<Vec<String>, HistoryError> {
        let shared = self.lock();
        let mut query = SearchQuery::everything(SearchDirection::Backward, None);
        query.limit = count.map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        Ok(shared
            .store
            .search(query)?
            .into_iter()
            .map(|item| item.command_line)
            .collect())
    }

    pub fn clear(&self) -> Result<(), HistoryError> {
        let mut shared = self.lock();
        if shared.path.as_ref().is_none_or(|path| !path.exists()) {
            return Ok(());
        }
        shared.store.clear()?;
        Ok(())
    }

    pub fn sync(&self) -> std::io::Result<()> {
        self.lock().store.sync()
    }

    fn lock(&self) -> MutexGuard<'_, SharedHistory> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Reedline history proxy sharing admission state with the session runner.
#[derive(Debug, Clone)]
pub struct KokoHistory {
    shared: Arc<Mutex<SharedHistory>>,
}

impl KokoHistory {
    fn lock(&self) -> reedline::Result<MutexGuard<'_, SharedHistory>> {
        self.shared.lock().map_err(|_| {
            ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                "Koko history lock is poisoned",
            ))
        })
    }
}

impl History for KokoHistory {
    fn save(&mut self, mut item: HistoryItem) -> reedline::Result<HistoryItem> {
        let mut shared = self.lock()?;
        let normalized = normalize_entry(&item.command_line);
        item.command_line = normalized;
        let is_cypher = !item.command_line.trim_start().starts_with(':');
        let sensitive = is_sensitive_meta_command(&item.command_line);
        let suppress = shared.suppress_next
            || shared.hard_disabled
            || !shared.enabled
            || sensitive
            || (is_cypher && shared.skip_next);
        shared.suppress_next = false;
        if is_cypher && shared.skip_next {
            shared.skip_next = false;
        }
        if suppress || item.command_line.is_empty() {
            item.id = None;
            return Ok(item);
        }
        shared.store.save(item)
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.lock()?.store.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.lock()?.store.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        // FileBackedHistory supplies the cursor semantics used by Reedline. Its
        // prefix traversal remains case-sensitive; reverse substring search gets
        // a Unicode-aware case-insensitive fallback when the direct lookup misses.
        let fallback = match query.filter.command_line.as_ref() {
            Some(CommandLineSearch::Substring(needle)) => Some((
                needle.to_lowercase(),
                query.direction,
                query.limit,
                query.filter.session,
            )),
            _ => None,
        };
        let shared = self.lock()?;
        let direct = shared.store.search(query)?;
        if !direct.is_empty() || fallback.is_none() {
            return Ok(direct);
        }
        let (needle, direction, limit, session) = fallback.expect("checked fallback");
        let mut all = shared
            .store
            .search(SearchQuery::everything(direction, session))?;
        all.retain(|item| item.command_line.to_lowercase().contains(&needle));
        if let Some(limit) = limit.and_then(|value| usize::try_from(value).ok()) {
            all.truncate(limit);
        }
        Ok(all)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.lock()?.store.update(id, updater)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        self.lock()?.store.clear()
    }

    fn delete(&mut self, item: HistoryItemId) -> reedline::Result<()> {
        self.lock()?.store.delete(item)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .store
            .sync()
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .store
            .session()
    }
}

fn normalize_entry(input: &str) -> String {
    let (input, _) = normalize_continuations(input);
    input
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn is_sensitive_meta_command(input: &str) -> bool {
    let Some(name) = input
        .trim_start()
        .strip_prefix(':')
        .and_then(|command| command.split_whitespace().next())
    else {
        return false;
    };
    COMMAND_REGISTRY
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(name))
        .is_none_or(|spec| spec.sensitive || !spec.record_history)
}

fn prepare_private_file(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(metadata) = std::fs::symlink_metadata(parent) {
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(
                "history directory must not be a symlink",
            ));
        }
    }
    std::fs::create_dir_all(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(std::io::Error::other("history path must be a regular file"));
        }
    } else {
        create_private_file(path)?;
    }
    set_private_permissions(parent, path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map(|_| ())
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map(|_| ())
}

#[cfg(unix)]
fn set_private_permissions(parent: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_parent: &Path, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_filters_sensitive_entries() {
        let (control, mut history) = HistoryController::open(None, 10, true, false).unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 1;  \n"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line(":param secret \"x\""))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 1;"))
            .unwrap();
        assert_eq!(control.newest(None).unwrap(), vec!["RETURN 1;"]);
    }

    #[test]
    fn continuation_markers_are_removed_before_history_admission() {
        let (control, mut history) = HistoryController::open(None, 10, true, false).unwrap();
        history
            .save(HistoryItem::from_command_line(
                "MATCH (n) \\\nWHERE n.id = 1 \\\nRETURN n",
            ))
            .unwrap();
        assert_eq!(
            control.newest(None).unwrap(),
            vec!["MATCH (n)\nWHERE n.id = 1\nRETURN n"]
        );
    }

    #[test]
    fn skip_applies_to_next_cypher_not_meta_command() {
        let (control, mut history) = HistoryController::open(None, 10, true, false).unwrap();
        control.skip_next();
        history
            .save(HistoryItem::from_command_line(":status"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 1"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 2"))
            .unwrap();
        assert_eq!(control.newest(None).unwrap(), vec!["RETURN 2", ":status"]);
    }

    #[test]
    fn reverse_search_is_unicode_case_insensitive() {
        let (_control, mut history) = HistoryController::open(None, 10, true, false).unwrap();
        history
            .save(HistoryItem::from_command_line("MATCH (p:Person) RETURN p"))
            .unwrap();
        let matches = history
            .search(SearchQuery::all_that_contain_rev("person".to_string()))
            .unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn hard_disable_never_creates_or_records_history() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/history");
        let (control, mut history) =
            HistoryController::open(Some(path.clone()), 10, true, true).unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 1"))
            .unwrap();
        control.sync().unwrap();
        assert!(!control.enabled());
        assert!(control.newest(None).unwrap().is_empty());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn persistent_history_is_bounded_private_and_multiline_safe() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/history");
        let (control, mut history) =
            HistoryController::open(Some(path.clone()), 2, true, false).unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 1\n  AS one"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 2"))
            .unwrap();
        history
            .save(HistoryItem::from_command_line("RETURN 3"))
            .unwrap();
        control.sync().unwrap();
        drop(history);
        drop(control);

        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (control, _history) = HistoryController::open(Some(path), 2, true, false).unwrap();
        assert_eq!(control.newest(None).unwrap(), vec!["RETURN 3", "RETURN 2"]);
    }
}
