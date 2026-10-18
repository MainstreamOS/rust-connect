use super::{failure, Context};
use crate::{device::Device, protocol::types::Packet, utils::compat::component};
use std::sync::Arc;
use zbus::interface;

pub struct Daemon(pub Arc<Context>);
#[interface(name = "org.kde.kdeconnect.daemon")]
impl Daemon {
    #[zbus(name = "devices")]
    async fn devices(&self, only_reachable: bool, only_paired: bool) -> Vec<String> {
        let mut devices = Vec::new();
        for device in self.0.app.registry.list().await {
            if (!only_reachable || device.is_connected())
                && (!only_paired || self.0.app.pairing_handler.is_paired(&device.id).await)
            {
                devices.push(component(&device.id));
            }
        }
        devices.sort();
        devices
    }
    #[zbus(name = "selfId")]
    fn self_id(&self) -> String {
        self.0
            .app
            .connection_manager
            .get_identity()
            .map(|i| i.device_id)
            .unwrap_or_default()
    }
    #[zbus(signal, name = "deviceAdded")]
    pub async fn added(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "deviceRemoved")]
    pub async fn removed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "deviceVisibilityChanged")]
    pub async fn visibility(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: &str,
        reachable: bool,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "deviceListChanged")]
    pub async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

pub struct DeviceApi {
    pub context: Arc<Context>,
    pub device: String,
}
impl DeviceApi {
    async fn value(&self) -> zbus::fdo::Result<Device> {
        self.context
            .app
            .registry
            .get(&self.device)
            .await
            .map_err(failure)
    }
}
#[interface(name = "org.kde.kdeconnect.device")]
impl DeviceApi {
    #[zbus(property, name = "name")]
    async fn name(&self) -> zbus::fdo::Result<String> {
        Ok(self.value().await?.name)
    }
    #[zbus(property, name = "type")]
    async fn kind(&self) -> zbus::fdo::Result<String> {
        Ok(format!("{:?}", self.value().await?.device_type).to_ascii_lowercase())
    }
    #[zbus(property, name = "isReachable")]
    async fn reachable(&self) -> bool {
        self.value().await.is_ok_and(|d| d.is_connected())
    }
    #[zbus(property, name = "isPaired")]
    async fn paired(&self) -> bool {
        self.context
            .app
            .pairing_handler
            .is_paired(&self.device)
            .await
    }
    #[zbus(name = "loadedPlugins")]
    async fn plugins(&self) -> Vec<String> {
        let Ok(device) = self.value().await else {
            return vec![];
        };
        if !self.paired().await {
            return vec![];
        }
        // A plugin is usable only if the phone advertises a matching direction.
        let pairs = [
            ("battery", "kdeconnect.battery"),
            ("sms", "kdeconnect.sms.messages"),
            ("contacts", "kdeconnect.contacts.response_vcards"),
            ("telephony", "kdeconnect.telephony"),
            ("notifications", "kdeconnect.notification"),
            ("sftp", "kdeconnect.sftp"),
            ("share", "kdeconnect.share.request"),
        ];
        pairs
            .into_iter()
            .filter(|(_, cap)| {
                device
                    .incoming_capabilities
                    .iter()
                    .chain(&device.outgoing_capabilities)
                    .any(|c| c == cap)
            })
            .map(|(plugin, _)| format!("kdeconnect_{plugin}"))
            .collect()
    }
    #[zbus(name = "hasPlugin")]
    async fn has_plugin(&self, plugin: &str) -> bool {
        self.plugins()
            .await
            .iter()
            .any(|p| p == plugin || p.strip_prefix("kdeconnect_") == Some(plugin))
    }
    #[zbus(name = "requestPair")]
    async fn pair(&self) -> zbus::fdo::Result<()> {
        self.value().await?;
        if self.paired().await {
            return Ok(());
        }
        if let Some(cert) = self
            .context
            .app
            .connection_manager
            .get_peer_certificate(&self.device)
            .await
        {
            self.context
                .app
                .pairing_handler
                .set_pending_peer_cert(&self.device, cert)
                .await;
        }
        let pairing = &self.context.app.pairing_handler;
        if pairing.has_incoming_request(&self.device).await {
            if !pairing.has_identity_anchor(&self.device).await {
                return Err(failure("No peer certificate"));
            }
            self.context
                .app
                .connection_manager
                .send_packet(&self.device, &Packet::pair_response(true))
                .await
                .map_err(failure)?;
            pairing
                .accept_pairing(&self.device)
                .await
                .map_err(failure)?;
            let app = self.context.app.clone();
            let device = self.device.clone();
            tokio::spawn(async move {
                app.send_plugin_init_packets(&device).await;
            });
        } else {
            let timestamp = pairing
                .initiate_pairing(&self.device)
                .await
                .map_err(failure)?;
            self.context
                .app
                .connection_manager
                .send_packet(
                    &self.device,
                    &Packet::pair_request_with_timestamp(timestamp),
                )
                .await
                .map_err(failure)?;
        }
        Ok(())
    }
    #[zbus(name = "unpair")]
    async fn unpair(&self) -> zbus::fdo::Result<()> {
        let app = &self.context.app;
        // Local revocation must succeed even if the phone has vanished.
        if app.connection_manager.is_connected(&self.device).await {
            let _ = app
                .connection_manager
                .send_packet(&self.device, &Packet::pair_response(false))
                .await;
            if let Some(generation) = app.connection_manager.get_generation(&self.device).await {
                let _ = app
                    .connection_manager
                    .disconnect(&self.device, generation)
                    .await;
            }
        }
        app.plugins.sftp.cleanup_device(&self.device).await;
        app.plugin_registry.notify_disconnected(&self.device).await;
        if app.pairing_handler.is_paired(&self.device).await {
            app.pairing_handler
                .unpair(&self.device)
                .await
                .map_err(failure)?;
        }
        Ok(())
    }
    #[zbus(signal, name = "reachableChanged")]
    pub async fn reachable_event(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        reachable: bool,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "pairingChanged")]
    pub async fn paired_event(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        paired: bool,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "pluginsChanged")]
    pub async fn plugins_event(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

pub struct Battery {
    pub context: Arc<Context>,
    pub device: String,
}
#[interface(name = "org.kde.kdeconnect.device.battery")]
impl Battery {
    #[zbus(property, name = "charge")]
    fn charge(&self) -> i32 {
        self.context
            .app
            .plugins
            .battery
            .get_battery(&self.device)
            .map(|b| b.current_charge)
            .unwrap_or(-1)
    }
    #[zbus(property, name = "hasBattery")]
    fn available(&self) -> bool {
        self.context
            .app
            .plugins
            .battery
            .get_battery(&self.device)
            .is_some()
    }
    #[zbus(property, name = "isCharging")]
    fn charging(&self) -> bool {
        self.context
            .app
            .plugins
            .battery
            .get_battery(&self.device)
            .is_some_and(|b| b.is_charging)
    }
    #[zbus(signal, name = "refreshed")]
    pub async fn refreshed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        charging: bool,
        charge: i32,
    ) -> zbus::Result<()>;
}

pub struct Contacts {
    pub context: Arc<Context>,
    pub device: String,
}
#[interface(name = "org.kde.kdeconnect.device.contacts")]
impl Contacts {
    #[zbus(name = "synchronizeRemoteWithLocal")]
    async fn synchronize(&self) -> zbus::fdo::Result<()> {
        self.context
            .send(
                &self.device,
                self.context
                    .app
                    .plugins
                    .contacts
                    .request_all_uids_timestamps(),
            )
            .await
    }
    #[zbus(signal, name = "localCacheSynchronized")]
    pub async fn synchronized(emitter: &zbus::object_server::SignalEmitter<'_>)
        -> zbus::Result<()>;
}

pub struct Telephony;
#[interface(name = "org.kde.kdeconnect.device.telephony")]
impl Telephony {
    #[zbus(signal, name = "callReceived")]
    pub async fn received(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        event: &str,
        number: &str,
        name: &str,
    ) -> zbus::Result<()>;
}
