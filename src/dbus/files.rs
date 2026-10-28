use super::{failure, Context};
use crate::{
    plugins::{
        sftp::{MountState, SftpConnectionInfo},
        PluginEvent,
    },
    protocol::{payload_transfer::PayloadTransfer, types::Packet},
};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use zbus::{interface, zvariant::OwnedValue};

pub fn local_file(url: &str) -> zbus::fdo::Result<PathBuf> {
    let encoded = url
        .strip_prefix("file://")
        .ok_or_else(|| failure("Only local file URLs are accepted"))?;
    let encoded = encoded
        .strip_prefix("localhost/")
        .map(|s| format!("/{s}"))
        .unwrap_or_else(|| encoded.to_string());
    if !encoded.starts_with('/') {
        return Err(failure("Remote file URL authority is not supported"));
    }
    let mut bytes = Vec::new();
    let input = encoded.as_bytes();
    let mut n = 0;
    while n < input.len() {
        if input[n] == b'%' {
            let hex = input
                .get(n + 1..n + 3)
                .ok_or_else(|| failure("Invalid file URL escape"))?;
            let text = std::str::from_utf8(hex).map_err(failure)?;
            bytes.push(u8::from_str_radix(text, 16).map_err(failure)?);
            n += 3;
        } else {
            bytes.push(input[n]);
            n += 1
        }
    }
    if bytes.contains(&0) {
        return Err(failure("Invalid file URL"));
    }
    Ok(PathBuf::from(String::from_utf8(bytes).map_err(failure)?))
}

/// Return LOCAL roots: mounting /storage/emulated/0 puts its contents at the
/// mountpoint, so appending /storage/emulated/0 again would hide every file.
pub fn directory_roots(info: &SftpConnectionInfo, mount: &Path) -> BTreeMap<String, OwnedValue> {
    let root = info.mount_root();
    let paths = if info.multi_paths.is_empty() {
        vec![info.path.clone()]
    } else {
        info.multi_paths.clone()
    };
    paths
        .iter()
        .enumerate()
        .filter_map(|(index, remote)| {
            let relative = Path::new(remote).strip_prefix(&root).ok()?;
            if relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            {
                return None;
            }
            let local = if relative.as_os_str().is_empty() {
                mount.to_path_buf()
            } else {
                mount.join(relative)
            }
            .to_string_lossy()
            .into_owned();
            let label = info
                .path_names
                .get(index)
                .cloned()
                .unwrap_or_else(|| "Storage".into());
            Some((local, OwnedValue::from(zbus::zvariant::Str::from(label))))
        })
        .collect()
}

pub(crate) struct Share {
    pub context: Arc<Context>,
    pub device: String,
}
#[interface(name = "org.kde.kdeconnect.device.share")]
impl Share {
    #[zbus(name = "shareUrls")]
    async fn files(&self, urls: Vec<String>) -> zbus::fdo::Result<()> {
        self.context.require_device(&self.device).await?;
        for url in urls {
            let path = local_file(&url)?;
            let metadata = tokio::fs::metadata(&path).await.map_err(failure)?;
            if !metadata.is_file() {
                return Err(failure("Shared path is not a file"));
            }
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| failure("Invalid file name"))?;
            let transfer =
                PayloadTransfer::new(self.context.app.cert_manager.clone(), self.device.clone());
            let address = self
                .context
                .app
                .connection_manager
                .get_local_addr(&self.device)
                .await
                .ok_or_else(|| failure("Phone is disconnected"))?;
            let (info, mut task) = transfer
                .send_file(&path, address.ip())
                .await
                .map_err(failure)?;
            let packet = Packet::new(
                "kdeconnect.share.request".into(),
                serde_json::json!({"filename":filename}),
            )
            .with_payload_size(metadata.len())
            .with_payload_transfer_info(serde_json::to_value(info).map_err(failure)?);
            if let Err(error) = self.context.send(&self.device, packet).await {
                task.abort();
                return Err(error);
            }
            let bus = self.context.bus.clone();
            let object_path = Context::path(&self.device) + "/share";
            tokio::spawn(async move {
                let error = match tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    &mut task,
                )
                .await
                {
                    Ok(Ok(Ok(_))) => None,
                    Ok(Ok(Err(e))) => Some(e.to_string()),
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(e) => {
                        task.abort();
                        Some(e.to_string())
                    }
                };
                if let Some(error) = error {
                    tracing::warn!(%error,"Phone file transfer failed");
                    let _ = bus
                        .emit_signal(
                            None::<&str>,
                            object_path.as_str(),
                            "org.kde.kdeconnect.device.share",
                            "transferFailed",
                            &(error,),
                        )
                        .await;
                }
            });
        }
        Ok(())
    }
    #[zbus(signal, name = "transferFailed")]
    pub async fn failed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        error: &str,
    ) -> zbus::Result<()>;
}

pub(crate) struct Sftp {
    pub context: Arc<Context>,
    pub device: String,
}

const CREDENTIALS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn wait_for_sftp_credentials(
    mut events: tokio::sync::broadcast::Receiver<PluginEvent>,
    device: &str,
) -> zbus::fdo::Result<()> {
    loop {
        match events.recv().await {
            Ok(PluginEvent::SftpUpdate {
                device_id,
                available: true,
                mounted: false,
                ..
            }) if device_id == device => return Ok(()),
            Ok(PluginEvent::SftpError { device_id, error }) if device_id == device => {
                return Err(failure(error));
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(_) => return Err(failure("Phone file-browsing updates stopped")),
        }
    }
}

#[interface(name = "org.kde.kdeconnect.device.sftp")]
impl Sftp {
    #[zbus(name = "mountAndWait")]
    async fn mount(&self) -> zbus::fdo::Result<bool> {
        self.context.require_device(&self.device).await?;
        let plugin = &self.context.app.plugins.sftp;
        // Join a mount already under way: asking the phone again would send
        // fresh credentials, and those remount what is being mounted.
        let settle = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while matches!(
            plugin.get_mount_status(&self.device).state,
            MountState::Mounting
        ) && tokio::time::Instant::now() < settle
        {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        match plugin.get_mount_status(&self.device).state {
            MountState::Mounted => return Ok(true),
            MountState::Mounting => return Err(failure("Phone files are still mounting")),
            _ => {}
        }
        // Android may stop its SSH server while the main connection stays
        // alive. Cached credentials alone do not mean it is still listening.
        // Subscribe before requesting so even an immediate reply is observed.
        let updates = self.context.app.plugin_events.subscribe();
        self.context
            .send(&self.device, plugin.request_sftp(&self.device))
            .await?;
        tokio::time::timeout(
            // A reconnect can queue the reply behind contacts and SMS sync.
            // A live Android 1.37 phone took 16.6 s to answer this request.
            // The caller must allow this plus the 10 s sshfs mount budget.
            CREDENTIALS_TIMEOUT,
            wait_for_sftp_credentials(updates, &self.device),
        )
        .await
        .map_err(|_| failure("Phone did not refresh file-browsing credentials"))??;
        let info = plugin
            .get_connection(&self.device)
            .ok_or_else(|| failure("Phone did not provide file-browsing credentials"))?;
        let status = plugin
            .mount_device(&self.device, &info)
            .await
            .map_err(failure)?;
        match status.state {
            MountState::Mounted => Ok(true),
            MountState::Failed(e) => Err(failure(e)),
            _ => Ok(false),
        }
    }
    #[zbus(name = "isMounted")]
    fn is_mounted(&self) -> bool {
        matches!(
            self.context
                .app
                .plugins
                .sftp
                .get_mount_status(&self.device)
                .state,
            MountState::Mounted
        )
    }
    #[zbus(name = "mountPoint")]
    fn mount_point(&self) -> String {
        self.context
            .app
            .plugins
            .sftp
            .get_mount_status(&self.device)
            .mount_point
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    #[zbus(name = "getDirectories")]
    fn directories(&self) -> BTreeMap<String, OwnedValue> {
        self.context
            .app
            .plugins
            .sftp
            .get_connection(&self.device)
            .map(|info| directory_roots(&info, Path::new(&self.mount_point())))
            .unwrap_or_default()
    }
    #[zbus(name = "getMountError")]
    fn error(&self) -> String {
        match self
            .context
            .app
            .plugins
            .sftp
            .get_mount_status(&self.device)
            .state
        {
            MountState::Failed(e) => e,
            _ => String::new(),
        }
    }
    #[zbus(signal, name = "mounted")]
    pub async fn mounted(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal, name = "unmounted")]
    pub async fn unmounted(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn credentials(device: &str) -> PluginEvent {
        PluginEvent::SftpUpdate {
            device_id: device.into(),
            ip: "127.0.0.1".into(),
            port: 1739,
            user: "test".into(),
            path: "/".into(),
            available: true,
            mounted: false,
            mount_point: None,
        }
    }

    #[tokio::test]
    async fn fresh_credentials_waits_for_the_requested_phone() {
        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        let wait = wait_for_sftp_credentials(receiver, "phone");
        tokio::pin!(wait);
        sender.send(credentials("other-phone")).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        sender.send(credentials("phone")).unwrap();
        wait.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_credentials_recovers_from_a_lagged_event_stream() {
        let (sender, receiver) = tokio::sync::broadcast::channel(1);
        sender.send(credentials("other-phone")).unwrap();
        sender.send(credentials("phone")).unwrap();
        wait_for_sftp_credentials(receiver, "phone").await.unwrap();
    }

    #[tokio::test]
    async fn fresh_credentials_reports_a_closed_event_stream() {
        let (sender, receiver) = tokio::sync::broadcast::channel(1);
        drop(sender);
        assert!(wait_for_sftp_credentials(receiver, "phone").await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn credentials_can_arrive_after_a_reconnect_sync_burst() {
        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(17)).await;
            sender.send(credentials("phone")).unwrap();
        });
        tokio::time::timeout(
            CREDENTIALS_TIMEOUT,
            wait_for_sftp_credentials(receiver, "phone"),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn credentials_report_the_requested_phones_storage_error() {
        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        for device in ["other-phone", "phone"] {
            sender
                .send(PluginEvent::SftpError {
                    device_id: device.into(),
                    error: "Allow filesystem access on your phone".into(),
                })
                .unwrap();
        }
        let error = wait_for_sftp_credentials(receiver, "phone")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Allow filesystem access"));
    }
}
