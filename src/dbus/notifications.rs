use super::{failure, Context};
use crate::{
    plugins::{telephony::TelephonyPlugin, PluginEvent},
    protocol::types::Packet,
    utils::compat::{component, file_uri, notification_key},
};
use std::{collections::HashMap, sync::Arc};
use zbus::{
    interface,
    zvariant::{OwnedValue, Str},
};

/// How long a reconnected phone's notifications must stop arriving before
/// the ones it did not send again count as dismissed.
pub(super) const NOTIFICATION_SYNC_QUIET: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a reconnected phone may take to start sending its notifications
/// again before it counts as having none. It answers only after the rest of
/// its connect-time traffic, which can take more than half a minute.
pub(super) const NOTIFICATION_RESEND_WAIT: std::time::Duration =
    std::time::Duration::from_secs(120);

/// Where a reconnected phone is in sending its active notifications again.
#[derive(Clone, Copy)]
pub(super) struct NotificationSync {
    connected: std::time::Instant,
    /// The last notification since the resend began, which is with the first
    /// silent one: the phone marks what it sends again as silent.
    last: Option<std::time::Instant>,
}

impl NotificationSync {
    pub(super) fn new() -> Self {
        Self {
            connected: std::time::Instant::now(),
            last: None,
        }
    }

    pub(super) fn heard(&mut self, silent: bool) {
        if silent || self.last.is_some() {
            self.last = Some(std::time::Instant::now());
        }
    }

    /// When what the phone has not sent again counts as dismissed.
    pub(super) fn due(&self) -> std::time::Instant {
        match self.last {
            Some(last) => last + NOTIFICATION_SYNC_QUIET,
            None => self.connected + NOTIFICATION_RESEND_WAIT,
        }
    }
}

/// The event that withdraws a notification from the desktop.
pub(crate) fn cancelled(device: &str, id: String) -> PluginEvent {
    PluginEvent::Notification {
        device_id: device.into(),
        id,
        app_name: String::new(),
        title: String::new(),
        text: String::new(),
        ticker: None,
        is_cancel: true,
        silent: true,
        actions: None,
        conversation: None,
        group_name: None,
        reply_id: None,
        icon_hash: None,
        icon_url: None,
    }
}

#[derive(Clone)]
pub(crate) struct Record {
    pub device: String,
    pub id: String,
    pub app: String,
    pub title: String,
    pub text: String,
    pub ticker: String,
    pub reply: Option<String>,
    pub actions: Vec<String>,
    pub silent: bool,
    pub dismissable: bool,
    pub server_id: u32,
    pub fingerprint: String,
    pub serial: u64,
}
impl Context {
    pub(crate) fn record(&self, device: &str, id: &str) -> Option<Record> {
        self.notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device.into(), id.into()))
            .cloned()
    }
    pub(crate) async fn resynchronize_notifications(self: &Arc<Self>) -> zbus::Result<()> {
        // A missed unpair must never re-export stale core cache entries.
        let paired: std::collections::HashSet<_> = self
            .app
            .pairing_handler
            .paired_devices()
            .await
            .into_iter()
            .collect();
        let entries: Vec<_> = self
            .app
            .plugins
            .notification
            .active_notifications()
            .into_iter()
            .filter(|entry| paired.contains(&entry.device_id))
            .collect();
        let active: std::collections::HashSet<_> = entries
            .iter()
            .map(|n| (n.device_id.clone(), n.id.clone()))
            .collect();
        let stale: Vec<_> = self
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter(|key| !key.1.starts_with("call:") && !active.contains(*key))
            .cloned()
            .collect();
        for (device_id, id) in stale {
            self.notification_event(cancelled(&device_id, id)).await?;
        }
        for n in entries {
            self.notification_event(PluginEvent::Notification {
                device_id: n.device_id,
                id: n.id,
                app_name: n.app_name,
                title: n.title,
                text: n.text,
                ticker: None,
                is_cancel: false,
                silent: n.silent,
                actions: n.actions,
                conversation: n.conversation,
                group_name: n.group_name,
                reply_id: n.reply_id,
                icon_hash: n.icon_hash,
                icon_url: n.icon_url,
            })
            .await?;
        }
        Ok(())
    }
    /// Withdraw what a reconnected phone dismissed while it was away: it sends
    /// every active notification again when asked at connect.
    pub(crate) async fn settle_notifications(self: &Arc<Self>) -> zbus::Result<()> {
        let settled: Vec<String> = {
            let mut syncs = self.notification_syncs.lock().await;
            let quiet: Vec<String> = syncs
                .iter()
                .filter(|(_, sync)| sync.due() <= std::time::Instant::now())
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
            let stale: Vec<String> = self
                .notifications
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .filter(|record| record.device == device && !record.id.starts_with("call:"))
                .filter(|record| {
                    self.app
                        .plugins
                        .notification
                        .active_notification(&device, &record.id)
                        .is_none()
                })
                .map(|record| record.id.clone())
                .collect();
            for id in stale {
                self.notification_event(cancelled(&device, id)).await?;
            }
        }
        Ok(())
    }
    pub(crate) async fn notification_event(
        self: &Arc<Self>,
        event: PluginEvent,
    ) -> zbus::Result<()> {
        let PluginEvent::Notification {
            device_id,
            id,
            app_name,
            title,
            text,
            ticker,
            is_cancel,
            silent,
            actions,
            reply_id,
            icon_hash,
            ..
        } = event
        else {
            return Ok(());
        };
        if id.is_empty() || id.len() > 2048 {
            return Ok(());
        }
        let key = (device_id.clone(), id.clone());
        let path = format!(
            "{}/notifications/{}",
            Self::path(&device_id),
            component(&id)
        );
        if is_cancel {
            let removed = self
                .notifications
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            if let Some(record) = removed {
                self.close_notification(record.server_id).await;
            }
            let _ = self
                .bus
                .object_server()
                .remove::<NotificationItem, _>(path.as_str())
                .await;
            self.emit_at(
                &format!("{}/notifications", Self::path(&device_id)),
                "org.kde.kdeconnect.device.notifications",
                "notificationRemoved",
                &(component(&id),),
            )
            .await?;
            return Ok(());
        }
        let previous = self.record(&device_id, &id);
        let image = icon_hash
            .as_ref()
            .and_then(|h| self.app.plugins.notification.icon_path(&device_id, h));
        let fingerprint = serde_json::to_string(&(
            &app_name,
            &title,
            &text,
            &actions,
            &reply_id,
            &icon_hash,
            image.is_some(),
        ))
        .unwrap_or_default();
        let notification_key = notification_key(&device_id, &id);
        let dismissable = !id.starts_with("call:")
            && self
                .app
                .plugins
                .notification
                .active_notification(&device_id, &id)
                .map(|n| n.dismissable)
                .unwrap_or(true);
        let record = Record {
            device: device_id.clone(),
            id: id.clone(),
            app: app_name,
            title,
            text,
            ticker: ticker.unwrap_or_default(),
            reply: reply_id,
            actions: actions.unwrap_or_default(),
            silent,
            dismissable,
            server_id: previous.as_ref().map(|p| p.server_id).unwrap_or(0),
            fingerprint,
            serial: self
                .sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        };
        // Commit state and register the object before publishing the desktop alert.
        self.notifications
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), record.clone());
        if previous.is_none() {
            self.bus
                .object_server()
                .at(
                    path.as_str(),
                    NotificationItem {
                        context: self.clone(),
                        device: device_id.clone(),
                        id: id.clone(),
                    },
                )
                .await?;
        }
        let evicted = {
            let mut records = self
                .notifications
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if records.len() > 2048 {
                let oldest = records
                    .iter()
                    .min_by_key(|(_, r)| r.serial)
                    .map(|(k, _)| k.clone());
                oldest.and_then(|key| records.remove(&key))
            } else {
                None
            }
        };
        if let Some(old) = evicted {
            let old_path = format!(
                "{}/notifications/{}",
                Self::path(&old.device),
                component(&old.id)
            );
            self.close_notification(old.server_id).await;
            let _ = self
                .bus
                .object_server()
                .remove::<NotificationItem, _>(old_path.as_str())
                .await;
        }
        self.emit_at(
            &format!("{}/notifications", Self::path(&device_id)),
            "org.kde.kdeconnect.device.notifications",
            "notificationPosted",
            &(component(&id),),
        )
        .await?;
        if previous
            .as_ref()
            .is_some_and(|p| p.fingerprint == record.fingerprint && p.server_id != 0)
        {
            return Ok(());
        }
        let mut hints = HashMap::new();
        hints.insert(
            "x-kdeconnect-source-device",
            OwnedValue::from(Str::from(component(&device_id))),
        );
        hints.insert("x-kde-appname", OwnedValue::from(Str::from("kdeconnect")));
        hints.insert("desktop-entry", OwnedValue::from(Str::from("rust-connect")));
        hints.insert(
            "x-mainstream-notification-key",
            OwnedValue::from(Str::from(notification_key.as_str())),
        );
        hints.insert(
            "x-kde-display-appname",
            OwnedValue::from(Str::from(record.app.as_str())),
        );
        if silent {
            hints.insert("suppress-sound", OwnedValue::from(true));
        }
        if let Some(hash) = icon_hash.as_deref() {
            hints.insert(
                "x-mainstream-image-role",
                OwnedValue::from(Str::from("notification-image")),
            );
            hints.insert("x-mainstream-icon-hash", OwnedValue::from(Str::from(hash)));
        }
        if let Some(image) = image {
            hints.insert("image-path", OwnedValue::from(Str::from(file_uri(&image))));
        }
        let mut actions = Vec::new();
        if record.reply.is_some() {
            actions.extend(["inline-reply".to_string(), "Reply".to_string()]);
        }
        for action in &record.actions {
            actions.extend([action.clone(), action.clone()]);
        }
        let call_picture = self
            .calls
            .lock()
            .await
            .get(&device_id)
            .and_then(|c| c.picture.clone());
        let call = id.starts_with("call:");
        if call {
            hints.insert(
                "x-kde-eventId",
                OwnedValue::from(Str::from(if record.actions.is_empty() {
                    "missedCall"
                } else {
                    "callReceived"
                })),
            );
            if let Some(path) = call_picture {
                hints.insert("image-path", OwnedValue::from(Str::from(file_uri(&path))));
                hints.insert(
                    "x-mainstream-image-role",
                    OwnedValue::from(Str::from("contact")),
                );
            }
        }
        match self
            .show_notification(&record, actions, hints, if call { 0 } else { 5000 })
            .await
        {
            Ok(server_id) => {
                if let Some(stored) = self
                    .notifications
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&key)
                {
                    stored.server_id = server_id;
                }
            }
            Err(error) => tracing::warn!(error=%error,"Could not publish phone notification"),
        }
        Ok(())
    }
    pub(crate) async fn show_notification(
        &self,
        record: &Record,
        actions: Vec<String>,
        hints: HashMap<&str, OwnedValue>,
        timeout: i32,
    ) -> zbus::Result<u32> {
        let proxy = zbus::Proxy::new(
            &self.bus,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await?;
        let escaped = record
            .text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            proxy.call(
                "Notify",
                &(
                    &record.app,
                    record.server_id,
                    "",
                    &record.title,
                    escaped,
                    actions,
                    hints,
                    timeout,
                ),
            ),
        )
        .await
        .map_err(|_| zbus::Error::Failure("Notification server timed out".into()))?
    }
    pub(crate) async fn close_notification(&self, id: u32) {
        if id == 0 {
            return;
        }
        if let Ok(proxy) = zbus::Proxy::new(
            &self.bus,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                proxy.call::<_, _, ()>("CloseNotification", &(id,)),
            )
            .await;
        }
    }
    pub(crate) async fn notification_action(
        &self,
        device: &str,
        id: &str,
        action: &str,
    ) -> zbus::fdo::Result<()> {
        let record = self
            .record(device, id)
            .ok_or_else(|| failure("Notification is no longer active"))?;
        if id.starts_with("call:") && action == "Mute Call" {
            return self
                .send(device, TelephonyPlugin::mute_request_packet())
                .await;
        }
        if !record.actions.iter().any(|a| a == action) {
            return Err(failure("Unknown phone notification action"));
        }
        self.send(
            device,
            Packet::new(
                "kdeconnect.notification.action".into(),
                serde_json::json!({"key":id,"action":action}),
            ),
        )
        .await
    }
    pub(crate) async fn notification_reply(
        &self,
        device: &str,
        id: &str,
        text: &str,
    ) -> zbus::fdo::Result<()> {
        if text.trim().is_empty() {
            return Err(failure("Reply is empty"));
        }
        let reply = self
            .record(device, id)
            .or_else(|| {
                self.notifications
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .find(|r| r.device == device && r.reply.as_deref() == Some(id))
                    .cloned()
            })
            .and_then(|r| r.reply)
            .ok_or_else(|| failure("Notification cannot receive replies"))?;
        self.send(
            device,
            Packet::new(
                "kdeconnect.notification.reply".into(),
                serde_json::json!({"requestReplyId":reply,"message":text}),
            ),
        )
        .await
    }
    pub(crate) async fn desktop_action(&self, id: u32, action: &str) -> zbus::fdo::Result<()> {
        let call = self
            .calls
            .lock()
            .await
            .iter()
            .find(|(_, call)| call.server_id == id)
            .map(|(device, _)| device.clone());
        if let Some(device) = call {
            if action == "mute" || action == "Mute Call" {
                self.send(&device, TelephonyPlugin::mute_request_packet())
                    .await?;
            }
            return Ok(());
        }
        let record = self
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|r| r.server_id == id)
            .cloned();
        if let Some(record) = record {
            return self
                .notification_action(&record.device, &record.id, action)
                .await;
        }
        Ok(())
    }
    /// A reply typed into the desktop's copy of a phone notification.
    pub(crate) async fn desktop_replied(&self, id: u32, text: &str) -> zbus::fdo::Result<()> {
        let record = self
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|r| r.server_id == id)
            .cloned();
        match record {
            Some(record) => {
                self.notification_reply(&record.device, &record.id, text)
                    .await
            }
            None => Ok(()),
        }
    }
    pub(crate) async fn desktop_closed(&self, id: u32, reason: u32) -> zbus::fdo::Result<()> {
        // Expiry (1) and programmatic closure (3) must never delete a phone alert.
        if reason != 2 {
            return Ok(());
        }
        let record = self
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|r| r.server_id == id)
            .cloned();
        if let Some(record) = record {
            if record.dismissable {
                self.send(
                    &record.device,
                    Packet::new(
                        "kdeconnect.notification.request".into(),
                        serde_json::json!({"cancel":record.id}),
                    ),
                )
                .await?;
                self.app
                    .plugins
                    .notification
                    .dismiss(&record.device, &record.id);
            }
        }
        Ok(())
    }
}

pub struct Notifications {
    pub context: Arc<Context>,
    pub device: String,
}
#[interface(name = "org.kde.kdeconnect.device.notifications")]
impl Notifications {
    #[zbus(name = "activeNotifications")]
    fn active(&self) -> Vec<String> {
        self.context
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|r| r.device == self.device)
            .map(|r| component(&r.id))
            .collect()
    }
    #[zbus(name = "sendAction")]
    async fn action(&self, id: &str, action: &str) -> zbus::fdo::Result<()> {
        self.context
            .notification_action(&self.device, id, action)
            .await
    }
    #[zbus(name = "sendReply")]
    async fn reply(&self, id: &str, text: &str) -> zbus::fdo::Result<()> {
        self.context
            .notification_reply(&self.device, id, text)
            .await
    }
    #[zbus(signal, name = "notificationPosted")]
    pub async fn posted(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "notificationRemoved")]
    pub async fn removed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: &str,
    ) -> zbus::Result<()>;
}
pub struct NotificationItem {
    pub context: Arc<Context>,
    pub device: String,
    pub id: String,
}
impl NotificationItem {
    fn record(&self) -> zbus::fdo::Result<Record> {
        self.context
            .record(&self.device, &self.id)
            .ok_or_else(|| failure("Notification is no longer active"))
    }
}
#[interface(name = "org.kde.kdeconnect.device.notifications.notification")]
impl NotificationItem {
    #[zbus(property, name = "internalId")]
    fn id(&self) -> &str {
        &self.id
    }
    #[zbus(property, name = "replyId")]
    fn reply_id(&self) -> zbus::fdo::Result<String> {
        Ok(self.record()?.reply.unwrap_or_default())
    }
    #[zbus(property, name = "appName")]
    fn app(&self) -> zbus::fdo::Result<String> {
        Ok(self.record()?.app)
    }
    #[zbus(property, name = "title")]
    fn title(&self) -> zbus::fdo::Result<String> {
        Ok(self.record()?.title)
    }
    #[zbus(property, name = "text")]
    fn text(&self) -> zbus::fdo::Result<String> {
        Ok(self.record()?.text)
    }
    #[zbus(property, name = "ticker")]
    fn ticker(&self) -> zbus::fdo::Result<String> {
        Ok(self.record()?.ticker)
    }
    #[zbus(property, name = "dismissable")]
    fn dismissable(&self) -> zbus::fdo::Result<bool> {
        Ok(self.record()?.dismissable)
    }
    #[zbus(property, name = "silent")]
    fn silent(&self) -> zbus::fdo::Result<bool> {
        Ok(self.record()?.silent)
    }
    #[zbus(name = "sendReply")]
    async fn reply(&self, text: &str) -> zbus::fdo::Result<()> {
        self.context
            .notification_reply(&self.device, &self.id, text)
            .await
    }
    #[zbus(name = "dismiss")]
    async fn dismiss(&self) -> zbus::fdo::Result<()> {
        if !self.record()?.dismissable {
            return Err(failure("Notification cannot be dismissed"));
        }
        self.context
            .send(
                &self.device,
                Packet::new(
                    "kdeconnect.notification.request".into(),
                    serde_json::json!({"cancel":self.id}),
                ),
            )
            .await?;
        self.context
            .app
            .plugins
            .notification
            .dismiss(&self.device, &self.id);
        Ok(())
    }
}
