use super::Context;
use crate::{
    plugins::{telephony::TelephonyInfo, PluginEvent},
    utils::compat,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::sync::Arc;

pub(crate) struct CallPopup {
    pub server_id: u32,
    pub picture: Option<std::path::PathBuf>,
}
impl Context {
    pub(crate) async fn call_event(
        self: &Arc<Self>,
        device: &str,
        info: TelephonyInfo,
    ) -> zbus::Result<()> {
        self.apply_call_event(device, info, true).await
    }
    pub(crate) async fn apply_call_event(
        self: &Arc<Self>,
        device: &str,
        info: TelephonyInfo,
        popup: bool,
    ) -> zbus::Result<()> {
        let id = format!("call:{device}");
        let cancel = info.is_cancel || info.event == "talking";
        if cancel {
            self.notification_event(super::notifications::cancelled(device, id))
                .await?;
            self.calls.lock().await.remove(device);
            return Ok(());
        }
        if !["ringing", "missedCall"].contains(&info.event.as_str()) {
            return Ok(());
        }
        let name = info
            .contact_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .or(info.phone_number.as_deref())
            .unwrap_or("Unknown");
        let event = if info.event == "ringing" {
            "callReceived"
        } else {
            "missedCall"
        };
        self.emit_at(
            &format!("{}/telephony", Self::path(device)),
            "org.kde.kdeconnect.device.telephony",
            "callReceived",
            &(event, info.phone_number.as_deref().unwrap_or(""), name),
        )
        .await?;
        // History recovered after a backlog must not ring for an ended call.
        if !popup {
            return Ok(());
        }
        let mut picture = None;
        if let Some(data) = info.phone_thumbnail.and_then(|s| {
            if s.len() <= 700000 {
                STANDARD.decode(s).ok()
            } else {
                None
            }
        }) {
            if data.starts_with(b"\xff\xd8\xff") || data.starts_with(b"\x89PNG\r\n\x1a\n") {
                let path = crate::plugins::caller_picture_path(&self.app.settings.data_dir, device);
                let folder = path.parent().unwrap_or(&self.app.settings.data_dir);
                if let Err(error) = compat::private_directory(folder).await {
                    tracing::warn!(%error,"Could not cache call picture");
                } else if tokio::fs::write(&path, &data).await.is_ok()
                    && compat::private_file(&path).await.is_ok()
                {
                    picture = Some(path)
                }
            }
        }
        self.calls.lock().await.insert(
            device.into(),
            CallPopup {
                server_id: 0,
                picture,
            },
        );
        let device_name = self
            .app
            .registry
            .get(&device.to_string())
            .await
            .map(|d| d.name)
            .unwrap_or_else(|_| "Phone".into());
        self.notification_event(PluginEvent::Notification {
            device_id: device.into(),
            id,
            app_name: "Phone".into(),
            title: device_name,
            text: format!(
                "{} call from {name}",
                if info.event == "ringing" {
                    "Incoming"
                } else {
                    "Missed"
                }
            ),
            ticker: None,
            is_cancel: false,
            silent: false,
            actions: if info.event == "ringing" {
                Some(vec!["Mute Call".into()])
            } else {
                None
            },
            conversation: None,
            group_name: None,
            reply_id: None,
            icon_hash: None,
            icon_url: None,
        })
        .await?;
        let server_id = self
            .record(device, &format!("call:{device}"))
            .map(|r| r.server_id)
            .unwrap_or(0);
        if let Some(call) = self.calls.lock().await.get_mut(device) {
            call.server_id = server_id;
        }
        Ok(())
    }
}
