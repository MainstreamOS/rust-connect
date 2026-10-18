use super::{failure, Context};
use crate::{
    plugins::sftp::{MountState, SftpConnectionInfo},
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

fn mount_root(info: &SftpConnectionInfo) -> PathBuf {
    let mut root = PathBuf::from(&info.path);
    for path in &info.multi_paths {
        while !Path::new(path).starts_with(&root) {
            if !root.pop() {
                return PathBuf::from("/");
            }
        }
    }
    root
}

/// Return LOCAL roots: mounting /storage/emulated/0 puts its contents at the
/// mountpoint, so appending /storage/emulated/0 again would hide every file.
pub fn directory_roots(info: &SftpConnectionInfo, mount: &Path) -> BTreeMap<String, OwnedValue> {
    let root = mount_root(info);
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
#[interface(name = "org.kde.kdeconnect.device.sftp")]
impl Sftp {
    #[zbus(name = "mountAndWait")]
    async fn mount(&self) -> zbus::fdo::Result<bool> {
        self.context.require_device(&self.device).await?;
        if self.is_mounted() {
            return Ok(true);
        }
        let plugin = &self.context.app.plugins.sftp;
        if plugin.get_connection(&self.device).is_none() {
            self.context
                .send(&self.device, plugin.request_sftp(&self.device))
                .await?;
            for _ in 0..50 {
                if plugin.get_connection(&self.device).is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        let mut info = plugin
            .get_connection(&self.device)
            .ok_or_else(|| failure("Phone did not provide file-browsing credentials"))?;
        info.path = mount_root(&info).to_string_lossy().into_owned();
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
