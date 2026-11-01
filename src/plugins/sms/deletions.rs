//! Desktop-only history cutoffs, retained across phone syncs and daemon restarts.
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(super) struct Deletions {
    pub entries: HashMap<String, HashMap<i64, i64>>,
    path: Option<PathBuf>,
    load_error: Option<String>,
}

impl Deletions {
    pub fn load(path: PathBuf) -> Self {
        let mut state = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(entries) => state.entries = entries,
                Err(error) => state.load_error = Some(error.to_string()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => state.load_error = Some(error.to_string()),
        }
        if let Some(error) = &state.load_error {
            tracing::error!(%error, "Could not load desktop message deletions; history sync is paused");
        }
        state
    }

    pub fn allows(&self, device: &str, thread: i64, date: i64) -> bool {
        self.load_error.is_none()
            && self
                .entries
                .get(device)
                .and_then(|threads| threads.get(&thread))
                .is_none_or(|before| date > *before)
    }

    pub fn forget(&mut self, device: &str, threads: &[i64], before: i64) -> std::io::Result<()> {
        if let Some(error) = &self.load_error {
            return Err(std::io::Error::other(error.clone()));
        }
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| std::io::Error::other("History storage unavailable"))?;
        let mut next = self.entries.clone();
        let cutoffs = next.entry(device.into()).or_default();
        for thread in threads {
            cutoffs
                .entry(*thread)
                .and_modify(|value| *value = (*value).max(before))
                .or_insert(before);
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        // NamedTempFile is private, and persist atomically replaces the snapshot.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &next)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        self.entries = next;
        Ok(())
    }
}

impl super::SmsPlugin {
    pub fn deletion_cutoffs(&self, device: &str) -> HashMap<i64, i64> {
        self.deletions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get(device)
            .cloned()
            .unwrap_or_default()
    }

    /// Forget local histories only. No packet is sent to the phone.
    pub async fn forget_conversations(
        &self,
        device: &str,
        ids: Vec<i64>,
    ) -> crate::utils::Result<i64> {
        if ids.is_empty()
            || ids.len() > super::MAX_SMS_THREADS_PER_DEVICE
            || ids.iter().any(|id| *id < 0)
        {
            return Err(crate::utils::Error::InvalidRequest(
                "Invalid conversation list".into(),
            ));
        }
        let plugin = self.clone();
        let owner = device.to_owned();
        let (before, attachments) = tokio::task::spawn_blocking(move || {
            let mut deletions = plugin.deletions.write().unwrap_or_else(|e| e.into_inner());
            let mut threads = plugin.threads.write().unwrap_or_else(|e| e.into_inner());
            let selected: std::collections::HashSet<_> = ids.into_iter().collect();
            let before = threads
                .iter()
                .filter(|((device, thread), _)| device == &owner && selected.contains(thread))
                .flat_map(|(_, messages)| messages.iter().filter_map(|message| message.date))
                .fold(super::now_ms(), i64::max);
            deletions.forget(
                &owner,
                &selected.iter().copied().collect::<Vec<_>>(),
                before,
            )?;
            let identifiers =
                |messages: &[super::SmsMessage]| -> std::collections::HashSet<String> {
                    messages
                        .iter()
                        .flat_map(|message| message.attachments.iter().flatten())
                        .filter_map(|attachment| {
                            attachment
                                .get("unique_identifier")
                                .or_else(|| attachment.get("uniqueIdentifier"))
                        })
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                };
            let mut removed = std::collections::HashSet::new();
            threads.retain(|(device, thread), messages| {
                if device == &owner && selected.contains(thread) {
                    removed.extend(identifiers(messages));
                    false
                } else {
                    true
                }
            });
            // An attachment shared by another retained conversation remains available.
            for ((device, _), messages) in threads.iter() {
                if device == &owner {
                    for identifier in identifiers(messages) {
                        removed.remove(&identifier);
                    }
                }
            }
            drop(threads);
            if let Some(listed) = plugin
                .listed_threads
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&owner)
            {
                listed.retain(|thread| !selected.contains(thread));
            }
            Ok::<_, std::io::Error>((before, removed))
        })
        .await
        .map_err(|error| crate::utils::Error::InvalidRequest(error.to_string()))??;
        if let Some(runtime) = &self.runtime {
            runtime.attachments.forget(device, &attachments).await;
            let folder = runtime.cache.join(super::attachment_filename(device));
            for identifier in attachments {
                for path in [
                    folder.join(super::attachment_slot(&identifier)),
                    folder.join(super::attachment_filename(&identifier)),
                ] {
                    let result = if path.is_dir() {
                        tokio::fs::remove_dir_all(&path).await
                    } else {
                        tokio::fs::remove_file(&path).await
                    };
                    if let Err(error) = result {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(%error, "Could not remove a deleted conversation's cached attachment");
                        }
                    }
                }
            }
        }
        Ok(before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_survives_restart_without_hiding_new_messages_or_other_phones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deleted.json");
        let mut state = Deletions::load(path.clone());
        state.forget("one", &[2, 4], 100).unwrap();
        let mut state = Deletions::load(path);
        assert!(!state.allows("one", 2, 100));
        assert!(state.allows("one", 2, 101));
        assert!(state.allows("two", 2, 50));
        assert!(state.allows("one", 3, 50));
        state.forget("one", &[2], 90).unwrap();
        assert!(!state.allows("one", 2, 99));
    }

    #[test]
    fn failed_save_and_unreadable_state_never_report_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deleted.json");
        let mut state = Deletions::load(path.clone());
        std::fs::create_dir(&path).unwrap();
        assert!(state.forget("one", &[2], 100).is_err());
        assert!(state.entries.is_empty());
        let state = Deletions::load(path);
        assert!(!state.allows("one", 2, 200));
    }
}
