use super::{failure, Context};
use crate::{
    device::Device,
    protocol::{pairing::PairState, types::Packet},
    utils::compat::component,
};
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
    /// KDE's PairState numbering: not paired, requested by us, requested by
    /// the phone, paired.
    #[zbus(property, name = "pairState")]
    async fn pair_state(&self) -> i32 {
        pair_state_number(
            self.context
                .app
                .pairing_handler
                .pair_state(&self.device)
                .await,
        )
    }
    #[zbus(property, name = "isPairRequestedByPeer")]
    async fn pair_requested_by_peer(&self) -> bool {
        self.context
            .app
            .pairing_handler
            .has_incoming_request(&self.device)
            .await
    }
    /// The code both screens show while a pairing request is pending, empty otherwise.
    #[zbus(property, name = "verificationKey")]
    async fn verification_key(&self) -> String {
        self.context
            .app
            .pairing_handler
            .get_verification_key(&self.device)
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }
    #[zbus(name = "requestPair")]
    async fn pair(&self) -> zbus::fdo::Result<()> {
        self.value().await?;
        if self.paired().await {
            return Ok(());
        }
        let pairing = &self.context.app.pairing_handler;
        if pairing.has_incoming_request(&self.device).await {
            self.accept_incoming().await?;
        } else {
            let timestamp = pairing
                .initiate_pairing(&self.device)
                .await
                .map_err(failure)?;
            // After initiate_pairing, whose expiry sweep drops staged certificates.
            self.stage_peer_certificate().await;
            if let Err(e) = self
                .context
                .app
                .connection_manager
                .send_packet(
                    &self.device,
                    &Packet::pair_request_with_timestamp(timestamp),
                )
                .await
            {
                let _ = pairing.reject_pairing(&self.device).await;
                return Err(failure(e));
            }
            self.context
                .pair_state_changed(&self.device)
                .await
                .map_err(failure)?;
        }
        Ok(())
    }
    /// Accept the phone's pending pairing request.
    #[zbus(name = "acceptPairing")]
    async fn accept_pairing(&self) -> zbus::fdo::Result<()> {
        self.value().await?;
        if !self
            .context
            .app
            .pairing_handler
            .has_incoming_request(&self.device)
            .await
        {
            return Err(failure("The phone is not asking to pair"));
        }
        self.accept_incoming().await
    }
    /// Decline the phone's pairing request, or withdraw ours.
    #[zbus(name = "cancelPairing")]
    async fn cancel_pairing(&self) -> zbus::fdo::Result<()> {
        let app = &self.context.app;
        if !matches!(
            app.pairing_handler.pair_state(&self.device).await,
            PairState::Requested | PairState::RequestedByPeer
        ) {
            return Ok(());
        }
        if app.connection_manager.is_connected(&self.device).await {
            let _ = app
                .connection_manager
                .send_packet(&self.device, &Packet::pair_response(false))
                .await;
        }
        let _ = app.pairing_handler.reject_pairing(&self.device).await;
        self.context
            .pair_state_changed(&self.device)
            .await
            .map_err(failure)
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
    #[zbus(signal, name = "pairStateChanged")]
    pub async fn pair_state_event(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        state: i32,
    ) -> zbus::Result<()>;
}

impl DeviceApi {
    /// The phone's certificate from its live link, so a request can be accepted
    /// and its verification key computed.
    async fn stage_peer_certificate(&self) {
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
    }
    async fn accept_incoming(&self) -> zbus::fdo::Result<()> {
        let accepted = self.context.app.accept_pairing_request(&self.device).await;
        if accepted.is_err() {
            // The failure withdrew the request; clients still show it.
            let _ = self.context.pair_state_changed(&self.device).await;
        }
        accepted.map_err(failure)
    }
}

pub(crate) fn pair_state_number(state: PairState) -> i32 {
    match state {
        PairState::NotPaired => 0,
        PairState::Requested => 1,
        PairState::RequestedByPeer => 2,
        PairState::Paired => 3,
    }
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
