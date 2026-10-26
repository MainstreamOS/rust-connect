//! Rebuild shell-facing state when a broadcast receiver falls behind. The
//! journal/revision checks also make queued pre-recovery events idempotent.
use super::{compat, Context, ROOT};
use crate::protocol::types::Packet;
use std::{collections::HashSet, sync::Arc};

/// How long a device's message batches must stop before missing threads
/// count as removed. The reconnect burst takes a second or two.
const THREAD_SYNC_QUIET: std::time::Duration = std::time::Duration::from_secs(15);

/// A device whose messages are arriving, and how far confirming missing
/// threads has got.
pub(crate) struct ThreadSync {
    last: std::time::Instant,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Syncing,
    /// The full list was asked for again; nothing has come back yet.
    Confirming,
    /// The phone sent conversations after being asked again.
    Answered,
}

#[derive(Debug, PartialEq)]
enum Settled {
    Unchanged,
    /// Threads are missing, but the sync may have been cut short: a reconnect
    /// empties the phone's threads and one later batch can look complete.
    Confirm,
    Removed(Vec<i64>),
}

fn settle(previous: &HashSet<i64>, current: &HashSet<i64>, phase: Phase) -> Settled {
    let removed = removed_threads(previous, current);
    match phase {
        _ if removed.is_empty() => Settled::Unchanged,
        Phase::Syncing => Settled::Confirm,
        // Silence confirms nothing: the threads stay.
        Phase::Confirming => Settled::Unchanged,
        Phase::Answered => Settled::Removed(removed),
    }
}

/// Threads the shell knows that a settled sync no longer has. None while the
/// thread cap may have evicted some, since those can still exist on the phone.
fn removed_threads(previous: &HashSet<i64>, current: &HashSet<i64>) -> Vec<i64> {
    if current.len() >= crate::plugins::sms::MAX_SMS_THREADS_PER_DEVICE {
        return Vec::new();
    }
    let mut removed: Vec<i64> = previous.difference(current).copied().collect();
    removed.sort_unstable();
    removed
}

impl Context {
    /// Add the device's current threads to those the shell has been told
    /// about. A batch never removes one: after a reconnect the phone sends its
    /// conversations one packet at a time with no end marker, so a thread
    /// missing from a batch has usually just not arrived yet.
    pub(crate) async fn remember_threads(&self, device: &str) -> HashSet<i64> {
        let current = self.app.plugins.sms.thread_ids(device);
        self.known_threads
            .lock()
            .await
            .entry(device.into())
            .or_default()
            .extend(current.iter().copied());
        self.thread_syncs
            .lock()
            .await
            .entry(device.into())
            .and_modify(|sync| {
                sync.last = std::time::Instant::now();
                if sync.phase == Phase::Confirming {
                    sync.phase = Phase::Answered;
                }
            })
            .or_insert(ThreadSync {
                last: std::time::Instant::now(),
                phase: Phase::Syncing,
            });
        current
    }
    /// Report threads that are really gone, once a connected device has sent
    /// nothing for `THREAD_SYNC_QUIET`, and again after asking for its list.
    pub(crate) async fn settle_threads(&self) -> zbus::Result<()> {
        let settled: Vec<(String, Phase)> = {
            let mut syncs = self.thread_syncs.lock().await;
            let quiet: Vec<(String, Phase)> = syncs
                .iter()
                .filter(|(_, sync)| sync.last.elapsed() >= THREAD_SYNC_QUIET)
                .map(|(device, sync)| (device.clone(), sync.phase))
                .collect();
            for (device, _) in &quiet {
                syncs.remove(device);
            }
            quiet
        };
        for (device, phase) in settled {
            if !self.app.connection_manager.is_connected(&device).await {
                continue;
            }
            let current = self.app.plugins.sms.thread_ids(&device);
            let removed = {
                let mut known = self.known_threads.lock().await;
                let previous = known.entry(device.clone()).or_default();
                match settle(previous, &current, phase) {
                    Settled::Unchanged => continue,
                    Settled::Confirm => None,
                    Settled::Removed(removed) => {
                        for thread in &removed {
                            previous.remove(thread);
                        }
                        Some(removed)
                    }
                }
            };
            let Some(removed) = removed else {
                let request = Packet::new(
                    "kdeconnect.sms.request_conversations".to_string(),
                    serde_json::json!({}),
                );
                match self.send(&device, request).await {
                    Ok(()) => {
                        self.thread_syncs.lock().await.insert(
                            device.clone(),
                            ThreadSync {
                                last: std::time::Instant::now(),
                                phase: Phase::Confirming,
                            },
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Could not ask the phone to confirm its conversations");
                    }
                }
                continue;
            };
            for thread in removed {
                self.emit(
                    &device,
                    "org.kde.kdeconnect.device.conversations",
                    "conversationRemoved",
                    &(thread,),
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn synchronize_messages(&self, device: &str) -> zbus::Result<()> {
        let known = self
            .known_threads
            .lock()
            .await
            .get(device)
            .cloned()
            .unwrap_or_default();
        let current = self.remember_threads(device).await;
        let watched = self.watched_threads.lock().await.clone();
        for thread in current {
            let messages = if watched.contains(&(device.into(), thread)) {
                self.app.plugins.sms.get_thread(device, thread)
            } else {
                self.app
                    .plugins
                    .sms
                    .latest_message(device, thread)
                    .into_iter()
                    .collect()
            };
            for (index, message) in messages.iter().enumerate() {
                let signal = if index == 0 && !known.contains(&thread) {
                    "conversationCreated"
                } else {
                    "conversationUpdated"
                };
                if let Err(error) = self.message_signal(device, signal, message).await {
                    tracing::warn!(%error,"Could not recover conversation message");
                }
            }
            self.emit(
                device,
                "org.kde.kdeconnect.device.conversations",
                "conversationLoaded",
                &(thread, messages.len() as u64),
            )
            .await?;
            tokio::task::yield_now().await;
        }
        Ok(())
    }
    pub(crate) async fn synchronize_calls(
        self: &Arc<Self>,
        device: &str,
        startup: bool,
    ) -> zbus::Result<()> {
        let previous = self
            .call_revisions
            .lock()
            .await
            .get(device)
            .copied()
            .unwrap_or(0);
        let events = self.app.plugins.telephony.events_since(device, previous);
        let last = events.last().map(|(revision, _)| *revision);
        for (revision, info) in events {
            if !startup || Some(revision) == last {
                self.apply_call_event(device, info, Some(revision) == last)
                    .await?;
            }
            self.call_revisions
                .lock()
                .await
                .insert(device.into(), revision);
        }
        Ok(())
    }
    pub(crate) async fn synchronize_attachments(&self) -> zbus::Result<()> {
        let mut results = self.app.plugins.sms.attachment_results();
        results.sort_by_key(|result| result.revision);
        let keys: HashSet<_> = results
            .iter()
            .map(|r| (r.device.clone(), r.identifier.clone()))
            .collect();
        self.attachment_revisions
            .lock()
            .await
            .retain(|key, _| keys.contains(key));
        for result in results {
            if !self.app.pairing_handler.is_paired(&result.device).await {
                continue;
            }
            let key = (result.device.clone(), result.identifier.clone());
            if self
                .attachment_revisions
                .lock()
                .await
                .get(&key)
                .is_some_and(|revision| *revision >= result.revision)
            {
                continue;
            }
            if let Some(path) = result.path {
                self.emit(
                    &result.device,
                    "org.kde.kdeconnect.device.conversations",
                    "attachmentReceived",
                    &(path, result.identifier),
                )
                .await?;
            } else if let Some(error) = result.error {
                self.emit(
                    &result.device,
                    "org.kde.kdeconnect.device.conversations",
                    "attachmentFailed",
                    &(result.identifier, error),
                )
                .await?;
            }
            self.attachment_revisions
                .lock()
                .await
                .insert(key, result.revision);
        }
        Ok(())
    }
    /// When the next pending thread or notification settle is due.
    pub(crate) async fn next_settle(&self) -> Option<tokio::time::Instant> {
        let threads = self
            .thread_syncs
            .lock()
            .await
            .values()
            .map(|sync| sync.last + THREAD_SYNC_QUIET)
            .min();
        let notifications = self
            .notification_syncs
            .lock()
            .await
            .values()
            .map(|last| *last + super::notifications::NOTIFICATION_SYNC_QUIET)
            .min();
        threads
            .into_iter()
            .chain(notifications)
            .min()
            .map(tokio::time::Instant::from_std)
    }
    pub(crate) async fn resynchronize(self: &Arc<Self>, startup: bool) -> zbus::Result<()> {
        self.synchronize_devices().await?;
        if let Err(error) = self.synchronize_contacts().await {
            tracing::warn!(%error, "Contact cache synchronization failed");
        }
        for device in self.app.registry.list().await {
            let paired = self.app.pairing_handler.is_paired(&device.id).await;
            let connected = self.app.connection_manager.is_connected(&device.id).await;
            if !paired {
                let was_trusted = self.trusted.lock().await.contains(&device.id);
                if was_trusted {
                    self.forget_device(&device.id).await;
                }
                continue;
            }
            let active_call = self.calls.lock().await.contains_key(&device.id);
            if connected {
                self.synchronize_messages(&device.id).await?;
                self.synchronize_calls(&device.id, startup).await?;
                if let Some(battery) = self.app.plugins.battery.get_battery(&device.id) {
                    self.emit_at(
                        &format!("{}/battery", Self::path(&device.id)),
                        "org.kde.kdeconnect.device.battery",
                        "refreshed",
                        &(battery.is_charging, battery.current_charge),
                    )
                    .await?;
                }
                let mounted = matches!(
                    self.app.plugins.sftp.get_mount_status(&device.id).state,
                    crate::plugins::sftp::MountState::Mounted
                );
                self.emit_at(
                    &format!("{}/sftp", Self::path(&device.id)),
                    "org.kde.kdeconnect.device.sftp",
                    if mounted { "mounted" } else { "unmounted" },
                    &(),
                )
                .await?;
            } else if active_call {
                self.call_event(
                    &device.id,
                    crate::plugins::telephony::TelephonyInfo {
                        event: "ringing".into(),
                        phone_number: None,
                        contact_name: None,
                        phone_thumbnail: None,
                        is_cancel: true,
                    },
                )
                .await?;
            }
            self.emit_at(
                ROOT,
                "org.kde.kdeconnect.daemon",
                "deviceVisibilityChanged",
                &(compat::component(&device.id), connected),
            )
            .await?;
        }
        self.synchronize_attachments().await?;
        if let Err(error) = self.synchronize_contacts().await {
            tracing::warn!(%error,"Contact cache recovery failed; will retry");
        }
        self.resynchronize_notifications().await?;
        self.emit_at(ROOT, "org.kde.kdeconnect.daemon", "deviceListChanged", &())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_sync_reports_threads_the_phone_no_longer_has() {
        let previous = HashSet::from([1, 2, 3]);
        let current = HashSet::from([1, 3, 4]);
        assert_eq!(removed_threads(&previous, &current), vec![2]);
    }

    #[test]
    fn nothing_is_removed_while_the_thread_cap_may_have_evicted_some() {
        let limit = crate::plugins::sms::MAX_SMS_THREADS_PER_DEVICE as i64;
        let previous: HashSet<i64> = (0..limit + 10).collect();
        let current: HashSet<i64> = (10..limit + 10).collect();
        assert!(removed_threads(&previous, &current).is_empty());
    }

    #[test]
    fn missing_threads_are_removed_only_after_the_phone_answers_again() {
        let previous = HashSet::from([1, 2, 3]);
        let partial = HashSet::from([2]);
        assert_eq!(
            settle(&previous, &partial, Phase::Syncing),
            Settled::Confirm
        );
        assert_eq!(
            settle(&previous, &partial, Phase::Confirming),
            Settled::Unchanged
        );
        assert_eq!(
            settle(&previous, &partial, Phase::Answered),
            Settled::Removed(vec![1, 3])
        );
        assert_eq!(
            settle(&previous, &previous, Phase::Syncing),
            Settled::Unchanged
        );
    }
}
