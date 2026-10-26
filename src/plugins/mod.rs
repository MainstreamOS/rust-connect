//! Plugin System
//!
//! This module provides the plugin system for feature implementation.
//! Each plugin has a single responsibility (one feature).
//! - plugin: Plugin trait definition
//! - registry: Plugin storage and lookup by capability
//! - loader: Create and register all built-in plugins

pub mod battery;
pub mod clipboard;
pub mod connectivity;
pub mod contacts;
pub mod digitizer;
pub mod events;
pub mod findmyphone;
pub mod findthisdevice;
pub mod loader;
pub mod lock;
pub mod mousepad;
pub mod mpris;
pub mod notification;
pub mod pausemusic;
pub mod ping;
pub mod plugin;
pub mod presenter;
pub mod registry;
pub mod remotecommands;
pub mod remotekeyboard;
pub mod runcommand;
pub mod screensaver_inhibit;
pub mod sendnotifications;
pub mod sftp;
pub mod share;
pub mod shareinputdevices;
pub mod sms;
pub mod systemvolume;
pub mod telephony;
pub mod tool;

use std::sync::Arc;

pub use battery::BatteryPlugin;
pub use clipboard::ClipboardPlugin;
pub use connectivity::ConnectivityPlugin;
pub use contacts::ContactsPlugin;
pub use digitizer::DigitizerPlugin;
pub use events::{PluginEvent, PluginEventBroadcaster};
pub use findmyphone::FindMyPhonePlugin;
pub use findthisdevice::FindThisDevicePlugin;
pub use loader::{load_all, load_default_plugins};
pub use lock::LockPlugin;
pub use mpris::MprisPlugin;
pub use notification::NotificationPlugin;
pub use pausemusic::PausemusicPlugin;
pub use ping::PingPlugin;
pub use plugin::Plugin;
pub use presenter::PresenterPlugin;
pub use registry::PluginRegistry;
pub use remotecommands::RemoteCommandsPlugin;
pub use remotekeyboard::RemoteKeyboardPlugin;
pub use runcommand::RuncommandPlugin;
pub use screensaver_inhibit::ScreensaverInhibitPlugin;
pub use sendnotifications::SendNotificationsPlugin;
pub use sftp::SftpPlugin;
pub use share::SharePlugin;
pub use shareinputdevices::ShareInputDevicesPlugin;
pub use sms::SmsPlugin;
pub use systemvolume::SystemVolumePlugin;
pub use telephony::TelephonyPlugin;
pub use tool::{Tool, ToolParameter};

/// Typed accessor for all plugin instances.
/// Replaces individual plugin fields on AppState.
pub struct PluginAccess {
    pub share: Arc<SharePlugin>,
    pub battery: Arc<BatteryPlugin>,
    pub sms: Arc<SmsPlugin>,
    pub clipboard: Arc<ClipboardPlugin>,
    pub mpris: Arc<MprisPlugin>,
    pub notification: Arc<NotificationPlugin>,
    pub telephony: Arc<TelephonyPlugin>,
    pub pausemusic: Arc<PausemusicPlugin>,
    pub connectivity: Arc<ConnectivityPlugin>,
    pub sftp: Arc<SftpPlugin>,
    pub mousepad: Arc<crate::plugins::mousepad::MousepadPlugin>,
    pub lock: Arc<LockPlugin>,
    pub systemvolume: Arc<SystemVolumePlugin>,
    pub ping: Arc<PingPlugin>,
    pub findmyphone: Arc<FindMyPhonePlugin>,
    pub findthisdevice: Arc<FindThisDevicePlugin>,
    pub presenter: Arc<PresenterPlugin>,
    pub contacts: Arc<ContactsPlugin>,
    pub runcommand: Arc<RuncommandPlugin>,
    pub sendnotifications: Arc<SendNotificationsPlugin>,
    pub remotekeyboard: Arc<RemoteKeyboardPlugin>,
    pub digitizer: Arc<DigitizerPlugin>,
    pub screensaver_inhibit: Arc<ScreensaverInhibitPlugin>,
    pub remotecommands: Arc<RemoteCommandsPlugin>,
    pub shareinputdevices: Arc<ShareInputDevicesPlugin>,
}

impl PluginAccess {
    /// Returns all plugins as a vector of Arc<dyn Plugin>.
    pub fn all(&self) -> Vec<Arc<dyn Plugin>> {
        vec![
            self.ping.clone(),
            self.battery.clone(),
            self.notification.clone(),
            self.sms.clone(),
            self.clipboard.clone(),
            self.share.clone(),
            self.mpris.clone(),
            self.telephony.clone(),
            self.pausemusic.clone(),
            self.connectivity.clone(),
            self.sftp.clone(),
            self.mousepad.clone(),
            self.lock.clone(),
            self.systemvolume.clone(),
            self.findmyphone.clone(),
            self.findthisdevice.clone(),
            self.presenter.clone(),
            self.contacts.clone(),
            self.runcommand.clone(),
            self.sendnotifications.clone(),
            self.remotekeyboard.clone(),
            self.digitizer.clone(),
            self.screensaver_inhibit.clone(),
            self.remotecommands.clone(),
            self.shareinputdevices.clone(),
        ]
    }
}

/// Where a phone's caller picture is cached.
pub fn caller_picture_path(data_dir: &std::path::Path, device: &str) -> std::path::PathBuf {
    data_dir
        .join("call-images")
        .join(sms::attachment_filename(device))
}

/// Delete what a phone left on disk while it was trusted: its notification
/// icons, its MMS attachments (stopping any still downloading) and its caller
/// picture. Unpairing runs this whatever asked for it, desktop service or not.
pub async fn forget_device_files(
    sms: &SmsPlugin,
    notification: &NotificationPlugin,
    data_dir: &std::path::Path,
    device: &str,
) {
    notification.forget_icons(device);
    sms.forget_attachments(device).await;
    let _ = tokio::fs::remove_file(caller_picture_path(data_dir, device)).await;
}

/// The same files for every phone that is no longer paired, for leftovers an
/// unpair could not reach, such as one interrupted before it deleted them.
pub async fn forget_files_of_unpaired(
    plugins: &PluginAccess,
    data_dir: &std::path::Path,
    paired: &std::collections::HashSet<String>,
) {
    plugins.notification.forget_icons_except(paired);
    let kept: std::collections::HashSet<_> = paired
        .iter()
        .map(|id| sms::attachment_filename(id))
        .collect();
    plugins.sms.forget_attachments_except(&kept).await;
    let pictures = data_dir.join("call-images");
    if let Ok(mut entries) = tokio::fs::read_dir(&pictures).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !kept.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
}
