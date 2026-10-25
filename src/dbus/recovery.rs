//! Rebuild shell-facing state when a broadcast receiver falls behind. The
//! journal/revision checks also make queued pre-recovery events idempotent.
use super::{compat, Context, ROOT};
use std::{collections::HashSet, sync::Arc};

/// How long a device's message batches must stop before missing threads
/// count as removed. The reconnect burst takes a second or two.
const THREAD_SYNC_QUIET: std::time::Duration = std::time::Duration::from_secs(15);

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
            .insert(device.into(), std::time::Instant::now());
        current
    }
    /// Report threads that are really gone, once a connected device has sent
    /// nothing for `THREAD_SYNC_QUIET`.
    pub(crate) async fn settle_threads(&self) -> zbus::Result<()> {
        let settled: Vec<String> = {
            let mut syncs = self.thread_syncs.lock().await;
            let quiet: Vec<String> = syncs
                .iter()
                .filter(|(_, last)| last.elapsed() >= THREAD_SYNC_QUIET)
                .map(|(device, _)| device.clone())
                .collect();
            for device in &quiet {
                syncs.remove(device);
            }
            quiet
        };
        for device in settled {
            if !self.app.connection_manager.is_connected(&device).await {
                continue;
            }
            let current = self.app.plugins.sms.thread_ids(&device);
            let removed = {
                let mut known = self.known_threads.lock().await;
                let previous = known.entry(device.clone()).or_default();
                let removed = removed_threads(previous, &current);
                for thread in &removed {
                    previous.remove(thread);
                }
                removed
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
    pub(crate) async fn resynchronize(self: &Arc<Self>, startup: bool) -> zbus::Result<()> {
        self.synchronize_devices().await?;
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
}
