//! Session D-Bus compatibility for the Mainstream Phone tab and Dynamic Island.
//! Protocol details and the acceptance contract live in docs/mainstream-shell-compatibility.md.
mod calls;
pub mod conversations;
mod devices;
pub mod files;
mod notifications;
mod recovery;

use crate::{
    app::AppState,
    device::DeviceEvent,
    plugins::{sms::SmsMessage, PluginEvent},
    protocol::{
        pairing::{PairState, PAIR_ACCEPT_TIMEOUT_SECS, PAIR_REQUEST_TIMEOUT_SECS},
        types::Packet,
    },
    utils::compat,
};
use futures::StreamExt;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc, RwLock},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zbus::{
    zvariant::{OwnedValue, Value},
    Connection,
};

pub(crate) fn failure(error: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}
const ROOT: &str = "/modules/kdeconnect";
const SERVICE: &str = "org.kde.kdeconnect";

pub(crate) struct Context {
    app: Arc<AppState>,
    bus: Connection,
    contacts_root: PathBuf,
    registered: Mutex<HashSet<String>>,
    trusted: Mutex<HashSet<String>>,
    notifications: RwLock<HashMap<(String, String), notifications::Record>>,
    calls: Mutex<HashMap<String, calls::CallPopup>>,
    sequence: AtomicU64,
    contact_revisions: Mutex<HashMap<String, u64>>,
    watched_threads: Mutex<HashSet<(String, i64)>>,
    known_threads: Mutex<HashMap<String, HashSet<i64>>>,
    /// When each device last sent messages, until `settle_threads` checks it.
    thread_syncs: Mutex<HashMap<String, std::time::Instant>>,
    call_revisions: Mutex<HashMap<String, u64>>,
    attachment_revisions: Mutex<HashMap<(String, String), u64>>,
}
impl Context {
    pub(crate) fn path(device: &str) -> String {
        format!("{ROOT}/devices/{}", compat::component(device))
    }
    pub(crate) async fn require_device(&self, device: &str) -> zbus::fdo::Result<()> {
        if !self
            .app
            .pairing_handler
            .is_paired(&device.to_string())
            .await
        {
            return Err(failure("Phone is not paired"));
        }
        if !self
            .app
            .connection_manager
            .is_connected(&device.to_string())
            .await
        {
            return Err(failure("Phone is disconnected"));
        }
        Ok(())
    }
    pub(crate) async fn send(&self, device: &str, packet: Packet) -> zbus::fdo::Result<()> {
        self.require_device(device).await?;
        self.app
            .connection_manager
            .send_packet(&device.to_string(), &packet)
            .await
            .map_err(failure)
    }
    pub(crate) async fn emit<T: serde::Serialize + zbus::zvariant::DynamicType>(
        &self,
        device: &str,
        interface: &str,
        signal: &str,
        body: &T,
    ) -> zbus::Result<()> {
        self.emit_at(&Self::path(device), interface, signal, body)
            .await
    }
    pub(crate) async fn emit_at<T: serde::Serialize + zbus::zvariant::DynamicType>(
        &self,
        path: &str,
        interface: &str,
        signal: &str,
        body: &T,
    ) -> zbus::Result<()> {
        self.bus
            .emit_signal(None::<&str>, path, interface, signal, body)
            .await
    }
    pub(crate) async fn message_signal(
        &self,
        device: &str,
        signal: &str,
        message: &SmsMessage,
    ) -> zbus::Result<()> {
        let message = match conversations::Message::try_from(message) {
            Ok(message) => message,
            Err(error) => {
                self.emit(
                    device,
                    "org.kde.kdeconnect.device.conversations",
                    "conversationError",
                    &(message.thread_id.unwrap_or(0), error.to_string()),
                )
                .await?;
                return Err(zbus::Error::Failure(error.to_string()));
            }
        };
        let value = OwnedValue::try_from(Value::from(message))?;
        self.emit(
            device,
            "org.kde.kdeconnect.device.conversations",
            signal,
            &(value,),
        )
        .await
    }
    async fn register(self: &Arc<Self>, device: &str) -> zbus::Result<bool> {
        let mut registered = self.registered.lock().await;
        if registered.contains(device) {
            return Ok(false);
        }
        let path = Self::path(device);
        let server = self.bus.object_server();
        server
            .at(
                path.as_str(),
                devices::DeviceApi {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(
                path.as_str(),
                conversations::Conversations {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(
                format!("{path}/sms").as_str(),
                conversations::Sms {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(
                format!("{path}/battery").as_str(),
                devices::Battery {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(
                format!("{path}/contacts").as_str(),
                devices::Contacts {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(format!("{path}/telephony").as_str(), devices::Telephony)
            .await?;
        server
            .at(
                format!("{path}/share").as_str(),
                files::Share {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(
                format!("{path}/sftp").as_str(),
                files::Sftp {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        server
            .at(
                format!("{path}/notifications").as_str(),
                notifications::Notifications {
                    context: self.clone(),
                    device: device.into(),
                },
            )
            .await?;
        registered.insert(device.into());
        Ok(true)
    }
    async fn synchronize_devices(self: &Arc<Self>) -> zbus::Result<()> {
        let mut changed = false;
        let devices = self.app.registry.list().await;
        let present: HashSet<_> = devices.iter().map(|d| d.id.as_str()).collect();
        let removed: Vec<_> = self
            .registered
            .lock()
            .await
            .iter()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        for device in removed {
            self.forget_device(&device).await;
            self.unregister(&device).await;
            self.emit_at(
                ROOT,
                "org.kde.kdeconnect.daemon",
                "deviceRemoved",
                &(compat::component(&device),),
            )
            .await?;
            changed = true;
        }
        for device in devices {
            if self.app.pairing_handler.is_paired(&device.id).await {
                self.trusted.lock().await.insert(device.id.clone());
            }
            if self.register(&device.id).await? {
                changed = true;
                self.emit_at(
                    ROOT,
                    "org.kde.kdeconnect.daemon",
                    "deviceAdded",
                    &(compat::component(&device.id),),
                )
                .await?;
            }
        }
        if changed {
            self.emit_at(ROOT, "org.kde.kdeconnect.daemon", "deviceListChanged", &())
                .await
        } else {
            Ok(())
        }
    }
    async fn plugin_event(self: &Arc<Self>, event: PluginEvent) -> zbus::Result<()> {
        match event {
            PluginEvent::SmsMessagesReceived {
                device_id,
                messages,
                created_threads,
            } => {
                self.register(&device_id).await?;
                let mut created: HashSet<_> = created_threads.into_iter().collect();
                let mut completed = HashSet::new();
                for message in messages {
                    let thread = message.thread_id.unwrap_or(0);
                    completed.insert(thread);
                    let signal = if created.remove(&thread) {
                        "conversationCreated"
                    } else {
                        "conversationUpdated"
                    };
                    if let Err(error) = self.message_signal(&device_id, signal, &message).await {
                        tracing::warn!(%error, "Could not publish conversation message");
                    }
                }
                self.remember_threads(&device_id).await;
                for thread in completed {
                    let count = self.app.plugins.sms.get_thread(&device_id, thread).len() as u64;
                    self.emit(
                        &device_id,
                        "org.kde.kdeconnect.device.conversations",
                        "conversationLoaded",
                        &(thread, count),
                    )
                    .await?;
                }
            }
            PluginEvent::SmsAttachmentReceived { .. } | PluginEvent::SmsAttachmentFailed { .. } => {
                self.synchronize_attachments().await?;
            }
            PluginEvent::Battery {
                device_id,
                current_charge,
                is_charging,
            } => {
                self.emit_at(
                    &format!("{}/battery", Self::path(&device_id)),
                    "org.kde.kdeconnect.device.battery",
                    "refreshed",
                    &(is_charging, current_charge),
                )
                .await?
            }
            PluginEvent::Notification { ref device_id, .. } => {
                self.register(device_id).await?;
                self.notification_event(event).await?;
            }
            PluginEvent::TelephonyUpdate { device_id, .. } => {
                self.register(&device_id).await?;
                self.synchronize_calls(&device_id, false).await?;
            }
            PluginEvent::SftpUpdate {
                device_id, mounted, ..
            } => {
                self.emit_at(
                    &format!("{}/sftp", Self::path(&device_id)),
                    "org.kde.kdeconnect.device.sftp",
                    if mounted { "mounted" } else { "unmounted" },
                    &(),
                )
                .await?
            }
            _ => {}
        }
        Ok(())
    }
    async fn synchronize_contacts(&self) -> crate::utils::Result<()> {
        for device in self.app.registry.list().await {
            if !self.app.pairing_handler.is_paired(&device.id).await {
                continue;
            }
            let previous = self.contact_revisions.lock().await.get(&device.id).copied();
            let Some((revision, mut contacts)) = self
                .app
                .plugins
                .contacts
                .contacts_since(&device.id, previous)
            else {
                continue;
            };
            contacts.sort_by(|a, b| a.uid.cmp(&b.uid));
            let directory = self
                .contacts_root
                .join(format!("kdeconnect-{}", compat::component(&device.id)));
            compat::private_directory(&directory).await?;
            let mut wanted = HashSet::new();
            for contact in contacts {
                let filename = format!(
                    "rc_{}.vcf",
                    crate::plugins::sms::attachment_filename(&contact.uid)
                );
                let path = directory.join(&filename);
                wanted.insert(filename);
                let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
                tokio::fs::write(&temp, contact.vcard.as_bytes()).await?;
                compat::private_file(&temp).await?;
                tokio::fs::rename(temp, path).await?;
            }
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".vcf") && !wanted.contains(&name) {
                    tokio::fs::remove_file(entry.path()).await?;
                }
            }
            self.contact_revisions
                .lock()
                .await
                .insert(device.id.clone(), revision);
            self.emit_at(
                &format!("{}/contacts", Self::path(&device.id)),
                "org.kde.kdeconnect.device.contacts",
                "localCacheSynchronized",
                &(),
            )
            .await
            .map_err(|e| crate::utils::Error::Internal(e.to_string()))?;
        }
        Ok(())
    }
    async fn device_event(self: &Arc<Self>, event: DeviceEvent) -> zbus::Result<()> {
        let disconnected = matches!(
            &event,
            DeviceEvent::Disconnected { .. }
                | DeviceEvent::Unpaired { .. }
                | DeviceEvent::Removed { .. }
        );
        let removed = matches!(&event, DeviceEvent::Removed { .. });
        let revoked = removed || matches!(&event, DeviceEvent::Unpaired { .. });
        let device = match event {
            DeviceEvent::Discovered { device_id, .. }
            | DeviceEvent::StateChanged { device_id, .. }
            | DeviceEvent::Paired { device_id, .. }
            | DeviceEvent::Unpaired { device_id }
            | DeviceEvent::PairRequested { device_id, .. }
            | DeviceEvent::Connected { device_id, .. }
            | DeviceEvent::Disconnected { device_id, .. }
            | DeviceEvent::Removed { device_id } => device_id,
        };
        if disconnected {
            let info = crate::plugins::telephony::TelephonyInfo {
                event: "ringing".into(),
                phone_number: None,
                contact_name: None,
                phone_thumbnail: None,
                is_cancel: true,
            };
            self.call_event(&device, info).await?;
        }
        if revoked {
            self.forget_device(&device).await;
        }
        if removed {
            self.unregister(&device).await;
            self.emit_at(
                ROOT,
                "org.kde.kdeconnect.daemon",
                "deviceRemoved",
                &(compat::component(&device),),
            )
            .await?;
        }
        self.synchronize_devices().await?;
        self.emit_at(ROOT, "org.kde.kdeconnect.daemon", "deviceListChanged", &())
            .await?;
        if let Ok(value) = self.app.registry.get(&device).await {
            let paired = self.app.pairing_handler.is_paired(&device).await;
            self.emit(
                &device,
                "org.kde.kdeconnect.device",
                "reachableChanged",
                &(value.is_connected(),),
            )
            .await?;
            self.emit(&device, "org.kde.kdeconnect.device", "pluginsChanged", &())
                .await?;
            self.emit(
                &device,
                "org.kde.kdeconnect.device",
                "pairingChanged",
                &(paired,),
            )
            .await?;
            self.pair_state_changed(&device).await?;
            let properties = HashMap::from([
                ("isReachable", OwnedValue::from(value.is_connected())),
                (
                    "name",
                    OwnedValue::from(zbus::zvariant::Str::from(value.name.as_str())),
                ),
            ]);
            self.device_properties_changed(&device, properties).await?;
            self.emit_at(
                ROOT,
                "org.kde.kdeconnect.daemon",
                "deviceVisibilityChanged",
                &(compat::component(&device), value.is_connected()),
            )
            .await?;
        }
        Ok(())
    }

    /// Tells clients the pairing state and the properties that follow it, and
    /// tells them again if a pending request lapses unanswered.
    pub(crate) async fn pair_state_changed(self: &Arc<Self>, device: &str) -> zbus::Result<()> {
        let state = self.emit_pair_state(device).await?;
        let timeout = match state {
            PairState::Requested => PAIR_REQUEST_TIMEOUT_SECS,
            PairState::RequestedByPeer => PAIR_ACCEPT_TIMEOUT_SECS,
            _ => return Ok(()),
        };
        let context = self.clone();
        let device = device.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(timeout as u64 + 1)).await;
            if context.app.pairing_handler.pair_state(&device).await == PairState::NotPaired {
                let _ = context.emit_pair_state(&device).await;
            }
        });
        Ok(())
    }

    async fn emit_pair_state(&self, device: &str) -> zbus::Result<PairState> {
        let pairing = &self.app.pairing_handler;
        let id = device.to_string();
        let state = pairing.pair_state(&id).await;
        let number = devices::pair_state_number(state);
        self.emit(
            device,
            "org.kde.kdeconnect.device",
            "pairStateChanged",
            &(number,),
        )
        .await?;
        let key = pairing
            .get_verification_key(&id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let properties = HashMap::from([
            ("pairState", OwnedValue::from(number)),
            ("isPaired", OwnedValue::from(state == PairState::Paired)),
            (
                "isPairRequestedByPeer",
                OwnedValue::from(state == PairState::RequestedByPeer),
            ),
            (
                "verificationKey",
                OwnedValue::from(zbus::zvariant::Str::from(key)),
            ),
        ]);
        self.device_properties_changed(device, properties).await?;
        Ok(state)
    }

    async fn device_properties_changed(
        &self,
        device: &str,
        properties: HashMap<&str, OwnedValue>,
    ) -> zbus::Result<()> {
        self.emit_at(
            &Self::path(device),
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                "org.kde.kdeconnect.device",
                properties,
                Vec::<String>::new(),
            ),
        )
        .await
    }

    async fn forget_device(self: &Arc<Self>, device: &str) {
        let ids: Vec<_> = self
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|record| record.device == device)
            .map(|record| record.id.clone())
            .collect();
        for id in ids {
            let event = PluginEvent::Notification {
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
            };
            let _ = self.notification_event(event).await;
        }
        self.trusted.lock().await.remove(device);
        self.contact_revisions.lock().await.remove(device);
        self.watched_threads
            .lock()
            .await
            .retain(|(id, _)| id != device);
        self.known_threads.lock().await.remove(device);
        self.thread_syncs.lock().await.remove(device);
        self.call_revisions.lock().await.remove(device);
        self.attachment_revisions
            .lock()
            .await
            .retain(|(id, _), _| id != device);
        let directory = self
            .contacts_root
            .join(format!("kdeconnect-{}", compat::component(device)));
        if let Err(error) = tokio::fs::remove_dir_all(directory).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "Could not remove revoked contact cache");
            }
        }
    }
    async fn unregister(&self, device: &str) {
        let path = Self::path(device);
        let server = self.bus.object_server();
        let _ = server.remove::<devices::DeviceApi, _>(path.as_str()).await;
        let _ = server
            .remove::<conversations::Conversations, _>(path.as_str())
            .await;
        let _ = server
            .remove::<conversations::Sms, _>(format!("{path}/sms").as_str())
            .await;
        let _ = server
            .remove::<devices::Battery, _>(format!("{path}/battery").as_str())
            .await;
        let _ = server
            .remove::<devices::Contacts, _>(format!("{path}/contacts").as_str())
            .await;
        let _ = server
            .remove::<devices::Telephony, _>(format!("{path}/telephony").as_str())
            .await;
        let _ = server
            .remove::<files::Share, _>(format!("{path}/share").as_str())
            .await;
        let _ = server
            .remove::<files::Sftp, _>(format!("{path}/sftp").as_str())
            .await;
        let _ = server
            .remove::<notifications::Notifications, _>(format!("{path}/notifications").as_str())
            .await;
        self.registered.lock().await.remove(device);
    }

    async fn cleanup(&self) {
        let keys: Vec<_> = self
            .notifications
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for (device, id) in keys {
            let path = format!(
                "{}/notifications/{}",
                Self::path(&device),
                compat::component(&id)
            );
            let _ = self
                .bus
                .object_server()
                .remove::<notifications::NotificationItem, _>(path.as_str())
                .await;
        }
        let devices: Vec<_> = self.registered.lock().await.iter().cloned().collect();
        for device in devices {
            self.unregister(&device).await;
        }
        let _ = self
            .bus
            .object_server()
            .remove::<devices::Daemon, _>(ROOT)
            .await;
        let _ = self.bus.release_name(SERVICE).await;
        self.app
            .plugins
            .notification
            .use_compatibility_desktop(false);
    }
}

pub struct DBusService {
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl DBusService {
    pub async fn start(app: Arc<AppState>, shutdown: CancellationToken) -> zbus::Result<Self> {
        let bus = zbus::connection::Builder::session()?
            .name(SERVICE)?
            .build()
            .await?;
        let contacts = dirs::data_dir()
            .unwrap_or_else(|| app.settings.data_dir.clone())
            .join("kpeoplevcard");
        Self::on_connection(app, bus, contacts, shutdown).await
    }
    /// Also used by the private-bus integration harness, with isolated storage.
    pub async fn on_connection(
        app: Arc<AppState>,
        bus: Connection,
        contacts_root: PathBuf,
        shutdown: CancellationToken,
    ) -> zbus::Result<Self> {
        let mut devices = app.broadcaster.subscribe();
        let mut plugins = app.plugin_events.subscribe();
        let context = Arc::new(Context {
            app,
            bus,
            contacts_root,
            registered: Mutex::new(HashSet::new()),
            trusted: Mutex::new(HashSet::new()),
            notifications: RwLock::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(1),
            contact_revisions: Mutex::new(HashMap::new()),
            watched_threads: Mutex::new(HashSet::new()),
            known_threads: Mutex::new(HashMap::new()),
            thread_syncs: Mutex::new(HashMap::new()),
            call_revisions: Mutex::new(HashMap::new()),
            attachment_revisions: Mutex::new(HashMap::new()),
        });
        context
            .bus
            .object_server()
            .at(ROOT, devices::Daemon(context.clone()))
            .await?;
        context.synchronize_devices().await?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.Notifications")?
            .path("/org/freedesktop/Notifications")?
            .interface("org.freedesktop.Notifications")?
            .build();
        let mut desktop =
            zbus::MessageStream::for_match_rule(rule, &context.bus, Some(128)).await?;
        let owner_rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .path("/org/freedesktop/DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, "org.freedesktop.Notifications")?
            .build();
        let mut owners =
            zbus::MessageStream::for_match_rule(owner_rule, &context.bus, Some(16)).await?;
        context
            .app
            .plugins
            .notification
            .use_compatibility_desktop(true);
        let cancellation = shutdown.child_token();
        let stopped = cancellation.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = context.resynchronize(true).await {
                tracing::warn!(%error, "Could not restore active phone notifications");
            }
            let mut refresh = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                let result = tokio::select! {
                    _=stopped.cancelled()=>break,
                    owner=owners.next()=> {
                        if owner.is_none(){break}
                        for record in context.notifications.write().unwrap_or_else(|e|e.into_inner()).values_mut(){record.server_id=0;}
                        for call in context.calls.lock().await.values_mut(){call.server_id=0;}
                        Ok(())
                    }
                    message=desktop.next()=> {
                        match message {
                            Some(Ok(message))=> {
                                let name=message.header().member().map(|m|m.as_str().to_owned()).unwrap_or_default();
                                let outcome=if name=="ActionInvoked" {
                                    match message.body().deserialize::<(u32,String)>() {Ok((id,action))=>context.desktop_action(id,&action).await,Err(e)=>Err(failure(e))}
                                }else if name=="NotificationClosed" {
                                    match message.body().deserialize::<(u32,u32)>() {Ok((id,reason))=>context.desktop_closed(id,reason).await,Err(e)=>Err(failure(e))}
                                }else if name=="NotificationReplied" {
                                    match message.body().deserialize::<(u32,String)>() {Ok((id,text))=>context.desktop_replied(id,&text).await,Err(e)=>Err(failure(e))}
                                }else{Ok(())};
                                outcome.map_err(|e|zbus::Error::Failure(e.to_string()))
                            }
                            Some(Err(error))=>Err(error),None=>break,
                        }
                    }
                    event=plugins.recv()=>match event {
                        Ok(event)=>context.plugin_event(event).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count))=>{
                            tracing::warn!(count,"Resynchronizing desktop compatibility state after event backlog");
                            context.resynchronize(false).await
                        },
                        Err(_)=>break,
                    },
                    event = devices.recv() => match event {
                        Ok(event) => context.device_event(event).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => context.resynchronize(false).await,
                        Err(_) => break,
                    },
                    _=refresh.tick()=> {
                        if let Err(error)=context.synchronize_contacts().await {tracing::warn!(%error,"Contact cache synchronization failed");}
                        if let Err(error)=context.settle_threads().await {tracing::warn!(%error,"Conversation removal check failed");}
                        context.synchronize_devices().await
                    },
                };
                if let Err(error) = result {
                    tracing::warn!(%error,"Desktop compatibility event failed");
                }
            }
            context.cleanup().await;
        });
        Ok(Self {
            cancellation,
            task: Some(task),
        })
    }
    pub async fn stop(mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for DBusService {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
