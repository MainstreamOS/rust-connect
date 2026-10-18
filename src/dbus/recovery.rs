//! Rebuild shell-facing state when a broadcast receiver falls behind. The
//! journal/revision checks also make queued pre-recovery events idempotent.
use super::{compat, Context, ROOT};
use std::{collections::HashSet, sync::Arc};

impl Context {
    pub(crate) async fn reconcile_threads(&self, device: &str) -> zbus::Result<HashSet<i64>> {
        let current = self.app.plugins.sms.thread_ids(device);
        let previous = self
            .known_threads
            .lock()
            .await
            .insert(device.into(), current.clone())
            .unwrap_or_default();
        for removed in previous.difference(&current) {
            self.emit(
                device,
                "org.kde.kdeconnect.device.conversations",
                "conversationRemoved",
                &(*removed,),
            )
            .await?;
        }
        Ok(current)
    }
    async fn synchronize_messages(&self, device: &str) -> zbus::Result<()> {
        let known = self
            .known_threads
            .lock()
            .await
            .get(device)
            .cloned()
            .unwrap_or_default();
        let current = self.reconcile_threads(device).await?;
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
