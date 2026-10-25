//! SMS plugin
//!
//! Single Responsibility: Handle kdeconnect.sms packets.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};

use tracing::{info, trace};

use crate::protocol::types::Packet;
use crate::utils::errors::Result;

use super::plugin::Plugin;
mod attachments;
pub use attachments::{request_packet as attachment_request_packet, AttachmentResult};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SmsAddress {
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SmsMessage {
    #[serde(rename = "messageID")]
    pub message_id: Option<i64>,
    #[serde(rename = "threadID", alias = "thread_id", default)]
    pub thread_id: Option<i64>,
    #[serde(default)]
    pub addresses: Vec<SmsAddress>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub date: Option<i64>,
    #[serde(rename = "type", default)]
    pub message_type: Option<i32>,
    #[serde(default, deserialize_with = "read_flag")]
    pub read: Option<i32>,
    #[serde(rename = "uID", alias = "_id")]
    pub uid: Option<i64>,
    #[serde(default)]
    pub event: Option<i32>,
    #[serde(rename = "subscriptionID", alias = "sub_id")]
    pub subscription_id: Option<i32>,
    #[serde(rename = "attachments")]
    pub attachments: Option<Vec<serde_json::Value>>,
}

type ThreadKey = (String, i64);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SmsThread {
    pub thread_id: i64,
    pub read_count: usize,
    pub addresses: Vec<SmsAddress>,
    pub name: Option<String>,
    pub snippet: Option<String>,
}

type SmsThreadStore = HashMap<ThreadKey, Vec<SmsMessage>>;

/// Caps on peer-fed SMS state (2026-09-02 audit, B5): thread ids are
/// peer-controlled, so both the thread count per device and the messages
/// per thread are bounded, oldest out.
pub const MAX_SMS_THREADS_PER_DEVICE: usize = 200;
pub const MAX_SMS_MESSAGES_PER_THREAD: usize = 500;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmsMessagePayload {
    #[serde(default)]
    pub version: Option<i32>,
    #[serde(default)]
    pub messages: Vec<SmsMessage>,
}

#[derive(Clone)]
pub struct SmsPlugin {
    threads: Arc<StdRwLock<SmsThreadStore>>,
    runtime: Option<SmsRuntime>,
}

#[derive(Clone)]
struct SmsRuntime {
    events: Arc<super::PluginEventBroadcaster>,
    connections: Arc<crate::protocol::ConnectionManager>,
    certificates: Arc<crate::protocol::CertificateManager>,
    cache: PathBuf,
    attachments: Arc<attachments::AttachmentQueue>,
}

/// Cancellation and timeouts must not leave partial private media behind.
struct PartialAttachment(PathBuf);
impl Drop for PartialAttachment {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// Android serializes an integer today; older peers also used a boolean.
fn read_flag<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<i32>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Number(i32),
        Boolean(bool),
    }
    Ok(
        <Option<Flag> as serde::Deserialize>::deserialize(d)?.map(|v| match v {
            Flag::Number(n) => n,
            Flag::Boolean(b) => i32::from(b),
        }),
    )
}

pub fn attachment_filename(identifier: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(identifier.as_bytes()))
}

/// The cache name for an attachment: the phone's identifier made safe for a
/// file name, with an extension from its type when the identifier has none,
/// so saved and opened copies keep a name and type (`PART_….jpg`, as KDE
/// Connect names them). An identifier that had to be changed gets part of
/// its digest added so two identifiers never share a name.
pub fn attachment_name(identifier: &str, mime: Option<&str>) -> String {
    let (stem, extension) = match identifier.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && is_extension(ext) => {
            (stem, Some(ext.to_ascii_lowercase()))
        }
        _ => (identifier, None),
    };
    let safe: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    let stem = if safe.is_empty() {
        attachment_filename(identifier)
    } else if safe != stem {
        format!("{safe}-{}", &attachment_filename(identifier)[..12])
    } else {
        safe
    };
    match extension.or_else(|| {
        mime.and_then(crate::utils::compat::media_extension)
            .map(str::to_string)
    }) {
        Some(extension) => format!("{stem}.{extension}"),
        None => stem,
    }
}

fn is_extension(value: &str) -> bool {
    (1..=5).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A copy of the attachment already cached under another name: without the
/// extension its type gives, with one, or under the digest attachments were
/// named by before.
fn cached_attachment(directory: &std::path::Path, identifier: &str) -> Option<PathBuf> {
    let legacy = directory.join(attachment_filename(identifier));
    if legacy.is_file() {
        return Some(legacy);
    }
    let bare = attachment_name(identifier, None);
    std::fs::read_dir(directory)
        .ok()?
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == bare
                || name
                    .strip_prefix(&bare)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .is_some_and(is_extension)
        })
        .map(|entry| entry.path())
}

impl Default for SmsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SmsPlugin {
    pub fn new() -> Self {
        Self {
            threads: Arc::new(StdRwLock::new(HashMap::new())),
            runtime: None,
        }
    }

    pub fn with_runtime(
        mut self,
        events: Arc<super::PluginEventBroadcaster>,
        connections: Arc<crate::protocol::ConnectionManager>,
        certificates: Arc<crate::protocol::CertificateManager>,
        cache: PathBuf,
    ) -> Self {
        self.runtime = Some(SmsRuntime {
            events,
            connections,
            certificates,
            cache,
            attachments: Arc::new(attachments::AttachmentQueue::new()),
        });
        self
    }

    pub fn attachment_path(&self, device: &str, identifier: &str) -> Option<PathBuf> {
        let runtime = self.runtime.as_ref()?;
        let directory = runtime.cache.join(attachment_filename(device));
        let mime = self.attachment_mime(device, identifier);
        let path = directory.join(attachment_name(identifier, mime.as_deref()));
        if path.is_file() {
            return Some(path);
        }
        Some(cached_attachment(&directory, identifier).unwrap_or(path))
    }

    /// The type the phone gave an attachment, from the messages it was sent with.
    fn attachment_mime(&self, device: &str, identifier: &str) -> Option<String> {
        let field = |attachment: &serde_json::Value, snake: &str, camel: &str| {
            attachment
                .get(snake)
                .or_else(|| attachment.get(camel))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let threads = self.threads.read().unwrap_or_else(|e| e.into_inner());
        threads
            .iter()
            .filter(|((owner, _), _)| owner == device)
            .flat_map(|(_, messages)| messages.iter())
            .flat_map(|message| message.attachments.iter().flatten())
            .find(|attachment| {
                field(attachment, "unique_identifier", "uniqueIdentifier").as_deref()
                    == Some(identifier)
            })
            .and_then(|attachment| field(attachment, "mime_type", "mimeType"))
    }

    async fn receive_attachment(&self, device: &str, packet: Packet) -> Result<PathBuf> {
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            crate::utils::errors::Error::InvalidRequest("SMS payload transport unavailable".into())
        })?;
        let identifier = packet
            .body
            .get("filename")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty() && v.len() <= 1024)
            .ok_or_else(|| {
                crate::utils::errors::Error::InvalidRequest("Missing attachment filename".into())
            })?;
        let size = packet
            .payload_size
            .and_then(crate::protocol::types::PayloadSize::known)
            .filter(|n| *n <= 100 * 1024 * 1024)
            .ok_or_else(|| {
                crate::utils::errors::Error::InvalidRequest("Invalid attachment size".into())
            })?;
        let mut info: crate::protocol::payload_transfer::PayloadTransferInfo =
            serde_json::from_value(packet.payload_transfer_info.ok_or_else(|| {
                crate::utils::errors::Error::InvalidRequest("Missing payload endpoint".into())
            })?)
            .map_err(|e| crate::utils::errors::Error::InvalidRequest(e.to_string()))?;
        let peer = runtime
            .connections
            .get_peer_addr(&device.to_string())
            .await
            .ok_or_else(|| {
                crate::utils::errors::Error::InvalidRequest("Phone is disconnected".into())
            })?;
        // Always bind payload retrieval to the authenticated phone's address.
        info.ip = Some(peer.ip().to_string());
        let target = self.attachment_path(device, identifier).ok_or_else(|| {
            crate::utils::errors::Error::InvalidRequest("SMS cache unavailable".into())
        })?;
        if let Some(parent) = target.parent() {
            crate::utils::compat::private_directory(parent).await?;
        }
        let temporary = target.with_extension(uuid::Uuid::new_v4().simple().to_string());
        let _partial = PartialAttachment(temporary.clone());
        let transfer = crate::protocol::payload_transfer::PayloadTransfer::new(
            runtime.certificates.clone(),
            device.to_string(),
        );
        let result = transfer.receive_file(&info, size, &temporary).await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
        crate::utils::compat::private_file(&temporary).await?;
        tokio::fs::rename(&temporary, &target).await?;
        if let Some(parent) = target.parent() {
            Self::trim_attachment_cache(parent, &target).await?;
        }
        Ok(target)
    }

    async fn trim_attachment_cache(
        directory: &std::path::Path,
        keep: &std::path::Path,
    ) -> Result<()> {
        // Bounded per-phone disk use; original files can be fetched again.
        let mut entries = tokio::fs::read_dir(directory).await?;
        let mut files = Vec::new();
        let mut bytes = 0u64;
        while let Some(entry) = entries.next_entry().await? {
            // Downloads in progress carry a 32-character uuid as their extension.
            let name = entry.file_name();
            if name
                .to_string_lossy()
                .rsplit_once('.')
                .is_some_and(|(_, ext)| ext.len() == 32)
            {
                continue;
            }
            let metadata = entry.metadata().await?;
            if metadata.is_file() {
                bytes += metadata.len();
                files.push((
                    metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                    entry.path(),
                    metadata.len(),
                ));
            }
        }
        files.sort_by_key(|f| f.0);
        let mut count = files.len();
        for (_, path, size) in files {
            if bytes <= 512 * 1024 * 1024 && count <= 1024 {
                break;
            }
            if path != keep && tokio::fs::remove_file(path).await.is_ok() {
                bytes -= size;
                count -= 1;
            }
        }
        Ok(())
    }

    pub fn thread_ids(&self, device: &str) -> std::collections::HashSet<i64> {
        self.threads
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter_map(|(id, thread)| (id == device).then_some(*thread))
            .collect()
    }
    pub fn latest_message(&self, device: &str, thread: i64) -> Option<SmsMessage> {
        self.threads
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device.into(), thread))
            .and_then(|messages| messages.last().cloned())
    }

    #[allow(clippy::expect_used)]
    pub fn get_threads(&self, device_id: &str) -> Vec<SmsThread> {
        let threads = self.threads.read().unwrap_or_else(|e| e.into_inner());
        threads
            .iter()
            .filter(|((did, _), _)| did == device_id)
            .map(|((_, thread_id), msgs)| {
                let read_count = msgs.len();
                let addresses = msgs
                    .first()
                    .map(|m| m.addresses.clone())
                    .unwrap_or_default();
                let snippet = msgs.last().and_then(|m| m.body.clone());
                SmsThread {
                    thread_id: *thread_id,
                    read_count,
                    addresses,
                    name: None,
                    snippet,
                }
            })
            .collect()
    }

    #[allow(clippy::expect_used)]
    pub fn get_thread(&self, device_id: &str, thread_id: i64) -> Vec<SmsMessage> {
        let threads = self.threads.read().unwrap_or_else(|e| e.into_inner());
        threads
            .get(&(device_id.to_string(), thread_id))
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Plugin for SmsPlugin {
    fn name(&self) -> &str {
        "sms"
    }

    fn incoming_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.sms.messages".to_string(),
            "kdeconnect.sms.attachment_file".to_string(),
        ]
    }

    fn outgoing_capabilities(&self) -> Vec<String> {
        vec![
            "kdeconnect.sms.request".to_string(),
            "kdeconnect.sms.request_conversations".to_string(),
            "kdeconnect.sms.request_conversation".to_string(),
            "kdeconnect.sms.request_attachment".to_string(),
        ]
    }

    fn on_connected(&self, _device_id: &str) -> Vec<Packet> {
        vec![Packet::new(
            "kdeconnect.sms.request_conversations".to_string(),
            serde_json::json!({}),
        )]
    }

    async fn on_disconnected(&self, device_id: &str) {
        if let Some(runtime) = &self.runtime {
            runtime.attachments.cancel_device(device_id);
        }
        if let Ok(mut threads) = self.threads.write() {
            threads.retain(|(did, _), _| did != device_id);
        }
    }

    async fn handle_packet(&self, device_id: &str, packet: Packet) -> Result<Option<Vec<Packet>>> {
        if packet.packet_type == "kdeconnect.sms.attachment_file" {
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                crate::utils::Error::InvalidRequest("SMS payload transport unavailable".into())
            })?;
            runtime.attachments.deliver(device_id, packet)?;
            return Ok(None);
        }
        let messages: Vec<SmsMessage> = match packet.body_as("messages payload") {
            Ok(msgs) => msgs,
            Err(_) => {
                let payload: SmsMessagePayload = packet.body_as("messages payload")?;
                payload.messages
            }
        };

        info!(
            device_id = %device_id,
            count = messages.len(),
            event = "sms_batch_received",
            "Received SMS message batch"
        );

        let mut changed = Vec::new();
        if let Ok(mut threads) = self.threads.write() {
            for mut msg in messages {
                trace!(
                    thread_id = msg.thread_id.unwrap_or(0),
                    event = "sms_received",
                    "Received individual SMS message"
                );
                let key = (device_id.to_string(), msg.thread_id.unwrap_or(0));
                if !threads.contains_key(&key) {
                    // New thread: evict this device's oldest thread (by its
                    // newest message date) once the per-device cap is hit.
                    let device_threads = threads.iter().filter(|(k, _)| k.0 == device_id).count();
                    if device_threads >= MAX_SMS_THREADS_PER_DEVICE {
                        if let Some(oldest) = threads
                            .iter()
                            .filter(|(k, _)| k.0 == device_id)
                            .min_by_key(|(_, msgs)| {
                                msgs.iter().filter_map(|m| m.date).max().unwrap_or(0)
                            })
                            .map(|(k, _)| k.clone())
                        {
                            threads.remove(&oldest);
                        }
                    }
                }
                let created = !threads.contains_key(&key);
                let thread = threads.entry(key).or_default();
                let same = |old: &SmsMessage| match (
                    old.uid.or(old.message_id),
                    msg.uid.or(msg.message_id),
                ) {
                    (Some(a), Some(b)) => a == b,
                    _ => {
                        old.date == msg.date
                            && old.body == msg.body
                            && old.message_type == msg.message_type
                    }
                };
                if let Some(index) = thread.iter().position(same) {
                    // An omitted field in a status/enrichment update is not a
                    // request to erase the original body or attachment identity.
                    // Explicit empty bodies/attachment lists still replace their values.
                    let old = &thread[index];
                    msg.body = msg.body.or_else(|| old.body.clone());
                    msg.attachments = msg.attachments.or_else(|| old.attachments.clone());
                    if msg.addresses.is_empty() {
                        msg.addresses = old.addresses.clone();
                    }
                    msg.date = msg.date.or(old.date);
                    msg.message_type = msg.message_type.or(old.message_type);
                    msg.event = msg.event.or(old.event);
                    msg.subscription_id = msg.subscription_id.or(old.subscription_id);
                    msg.read = msg.read.or(old.read);
                    thread[index] = msg.clone();
                } else {
                    thread.push(msg.clone());
                }
                thread.sort_by_key(|m| (m.date.unwrap_or(0), m.uid.or(m.message_id).unwrap_or(0)));
                changed.push((msg, created));
                if thread.len() > MAX_SMS_MESSAGES_PER_THREAD {
                    let excess = thread.len() - MAX_SMS_MESSAGES_PER_THREAD;
                    thread.drain(..excess);
                }
            }
        }

        if let Some(runtime) = &self.runtime {
            let created_threads = changed
                .iter()
                .filter(|(_, created)| *created)
                .filter_map(|(message, _)| message.thread_id)
                .collect();
            runtime
                .events
                .broadcast(super::PluginEvent::SmsMessagesReceived {
                    device_id: device_id.into(),
                    messages: changed.into_iter().map(|(message, _)| message).collect(),
                    created_threads,
                });
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn attachment_names_keep_the_identifier_and_gain_an_extension_from_the_type() {
        assert_eq!(
            attachment_name("PART_1700000000000", Some("image/jpeg")),
            "PART_1700000000000.jpg"
        );
        assert_eq!(
            attachment_name("PART_1700000000000", Some("audio/amr")),
            "PART_1700000000000.amr"
        );
        assert_eq!(
            attachment_name("voice-note.AMR", Some("audio/mp4")),
            "voice-note.amr"
        );
        assert_eq!(
            attachment_name("PART_17", Some("application/x-unknown")),
            "PART_17"
        );
        assert_eq!(attachment_name("PART_17", None), "PART_17");
    }

    #[test]
    fn unsafe_attachment_identifiers_cannot_escape_or_collide() {
        for identifier in ["../../etc/passwd", ".hidden", ".", "a/b.jpg"] {
            let name = attachment_name(identifier, None);
            assert!(!name.contains('/'), "{identifier} -> {name}");
            assert!(!name.starts_with('.'), "{identifier} -> {name}");
            assert!(!name.is_empty());
        }
        assert_ne!(attachment_name("a b", None), attachment_name("a_b", None));
        assert!(attachment_name(&"x".repeat(400), None).len() < 160);
    }

    #[tokio::test]
    async fn test_sms_plugin() {
        let plugin = SmsPlugin::new();
        assert_eq!(plugin.name(), "sms");
    }

    #[tokio::test]
    async fn test_handle_sms_messages_stores_data() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": 1,
                "threadID": 100,
                "addresses": [{"address": "+1234567890"}],
                "body": "Hello",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0
            }]}),
        );
        plugin
            .handle_packet("test", packet)
            .await
            .expect("Value expected to be present");

        let thread = plugin.get_thread("test", 100);
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].body.as_deref(), Some("Hello"));
    }

    #[tokio::test]
    async fn media_updates_and_replays_preserve_record_identity() {
        let plugin = SmsPlugin::new();
        let packet = |messages: serde_json::Value| {
            Packet::new(
                "kdeconnect.sms.messages".into(),
                serde_json::json!({"version": 2, "messages": messages}),
            )
        };
        // Real phone structure: a caption and media can be distinct MMS records
        // with different dates. Never deduplicate different IDs by proximity.
        for mime in [
            "image/jpeg",
            "image/gif",
            "audio/amr",
            "video/mp4",
            "application/pdf",
        ] {
            let messages = serde_json::json!([
                {"_id": 1075, "thread_id": 63, "date": 1792633343000i64, "type": 1,
                 "event": 3, "body": "caption", "addresses": [{"address": "+15555550123"}], "attachments": []},
                {"_id": 1076, "thread_id": 63, "date": 1792633344000i64, "type": 1,
                 "event": 2, "body": "", "addresses": [{"address": "+15555550123"}],
                 "attachments": [{"part_id": 1439, "mime_type": mime, "unique_identifier": "part-1439", "encoded_thumbnail": ""}]}
            ]);
            for _ in 0..3 {
                plugin
                    .handle_packet("phone", packet(messages.clone()))
                    .await
                    .unwrap();
            }
            plugin
                .handle_packet(
                    "phone",
                    packet(serde_json::json!([
                        {"_id": 1076, "thread_id": 63, "read": 1}
                    ])),
                )
                .await
                .unwrap();
            let stored = plugin.get_thread("phone", 63);
            assert_eq!(stored.len(), 2);
            assert_eq!(stored[0].body.as_deref(), Some("caption"));
            assert_eq!(stored[1].uid, Some(1076));
            assert_eq!(stored[1].date, Some(1792633344000));
            assert_eq!(stored[1].read, Some(1));
            assert_eq!(stored[1].event, Some(2));
            assert_eq!(
                stored[1].addresses[0].address.as_deref(),
                Some("+15555550123")
            );
            let variant = crate::dbus::conversations::Message::try_from(&stored[1]).unwrap();
            assert_eq!(variant.attachments[0].mime, mime);
            assert_eq!(variant.attachments[0].part, 1439);
            assert_eq!(variant.attachments[0].identifier, "part-1439");
        }
        // An authoritative empty list is different from an omitted list.
        plugin
            .handle_packet(
                "phone",
                packet(serde_json::json!([
                    {"_id": 1076, "thread_id": 63, "attachments": [], "body": "removed"}
                ])),
            )
            .await
            .unwrap();
        assert!(plugin.get_thread("phone", 63)[1]
            .attachments
            .as_ref()
            .unwrap()
            .is_empty());
        assert_eq!(
            plugin.get_thread("phone", 63)[1].body.as_deref(),
            Some("removed")
        );
        plugin
            .handle_packet(
                "other-phone",
                packet(serde_json::json!([
                    {"_id": 1075, "thread_id": 63, "date": 1792633343000i64, "body": "other phone"}
                ])),
            )
            .await
            .unwrap();
        assert_eq!(plugin.get_thread("other-phone", 63).len(), 1);
        assert_eq!(plugin.get_thread("phone", 63).len(), 2);
    }

    #[tokio::test]
    async fn test_empty_threads() {
        let plugin = SmsPlugin::new();
        assert!(plugin.get_threads("test").is_empty());
        assert!(plugin.get_thread("test", 999).is_empty());
    }

    #[tokio::test]
    async fn test_handle_malformed_sms_packet() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": "not an array"}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_sms_missing_fields() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{"body": "incomplete"}]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sms_outgoing_capabilities() {
        let plugin = SmsPlugin::new();
        assert!(plugin
            .outgoing_capabilities()
            .contains(&"kdeconnect.sms.request".to_string()));
    }

    fn sms_packet(thread_id: i64, message_id: i64) -> Packet {
        Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": message_id,
                "threadID": thread_id,
                "addresses": [{"address": "+1234567890"}],
                "body": format!("m{message_id}"),
                "date": 1700000000000i64 + message_id,
                "type": 1,
                "read": 0
            }]}),
        )
    }

    /// B5 (2026-09-02 audit): thread ids are peer-controlled and the
    /// store grew without bound for the life of a connection. Thread count
    /// per device and messages per thread are both capped, oldest out.
    #[tokio::test]
    async fn test_sms_threads_per_device_are_capped() {
        let plugin = SmsPlugin::new();
        for t in 0..(MAX_SMS_THREADS_PER_DEVICE as i64 + 25) {
            plugin
                .handle_packet("phone1", sms_packet(t, t))
                .await
                .expect("handle");
        }
        let threads = plugin.get_threads("phone1");
        assert_eq!(
            threads.len(),
            MAX_SMS_THREADS_PER_DEVICE,
            "thread count must be capped"
        );
        assert!(
            !plugin
                .get_thread("phone1", MAX_SMS_THREADS_PER_DEVICE as i64 + 24)
                .is_empty(),
            "the newest thread must survive eviction"
        );
    }

    #[tokio::test]
    async fn test_sms_messages_per_thread_are_capped() {
        let plugin = SmsPlugin::new();
        for m in 0..(MAX_SMS_MESSAGES_PER_THREAD as i64 + 25) {
            plugin
                .handle_packet("phone1", sms_packet(7, m))
                .await
                .expect("handle");
        }
        let msgs = plugin.get_thread("phone1", 7);
        assert_eq!(
            msgs.len(),
            MAX_SMS_MESSAGES_PER_THREAD,
            "messages per thread must be capped"
        );
        assert_eq!(
            msgs.last().and_then(|m| m.body.as_deref()),
            Some(format!("m{}", MAX_SMS_MESSAGES_PER_THREAD as i64 + 24).as_str()),
            "the newest message must survive eviction"
        );
    }

    #[tokio::test]
    async fn test_on_disconnected_cleans_up() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": 1,
                "threadID": 100,
                "addresses": [{"address": "+1234567890"}],
                "body": "Hello",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0
            }]}),
        );
        plugin
            .handle_packet("phone-1", packet)
            .await
            .expect("Value expected to be present");
        assert!(!plugin.get_threads("phone-1").is_empty());

        plugin.on_disconnected("phone-1").await;
        assert!(plugin.get_threads("phone-1").is_empty());
    }

    #[tokio::test]
    async fn test_on_disconnected_keeps_other_devices() {
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "messageID": 1,
                "threadID": 100,
                "addresses": [{"address": "+1234567890"}],
                "body": "Hello",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0
            }]}),
        );
        plugin
            .handle_packet("phone-1", packet.clone())
            .await
            .expect("Value expected to be present");
        plugin
            .handle_packet("phone-2", packet)
            .await
            .expect("Value expected to be present");

        plugin.on_disconnected("phone-1").await;
        assert!(plugin.get_threads("phone-1").is_empty());
        assert!(!plugin.get_threads("phone-2").is_empty());
    }

    // =====================================================================
    // TESTS FROM PROTOCOL REFERENCE (kdeconnect-android SMSHelper.kt)
    // These tests use the ACTUAL format from the Android implementation
    // =====================================================================

    #[tokio::test]
    async fn test_handle_sms_with_addresses_array_from_android() {
        // Upstream wire literal — see fixture provenance
        // tests/fixtures/upstream-wire/provenance.yaml: sms/message_batch.json
        // cited against kdeconnect-android SMSHelper.kt:911-933
        // (Message.toJSONObject() emits the {addresses, body, date, type,
        // read, threadID, uID, event, subscriptionID} shape).
        let plugin = SmsPlugin::new();

        let body: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/upstream-wire/sms/message_batch.json"),
            )
            .expect("sms/message_batch.json"),
        )
        .expect("sms/message_batch.json parses");

        let packet = Packet::new("kdeconnect.sms.messages".to_string(), body);
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle addresses array from Android"
        );

        let thread = plugin.get_thread("test", 100);
        assert_eq!(thread.len(), 1, "Should have stored the message");
        assert_eq!(thread[0].body.as_deref(), Some("Hello from Android"));
    }

    #[tokio::test]
    async fn test_handle_sms_with_read_as_int() {
        // kdeconnect-android sends "read" as int, not bool
        // See SMSHelper.kt line 921: json.put(READ, read) where read is Int
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [{"address": "+1234567890"}],
                "body": "Test message",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0,  // int from Android, not bool
                "threadID": 100,
                "uID": 1,
                "event": 0
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle read as int from Android"
        );
    }

    #[tokio::test]
    async fn test_handle_sms_multiple_addresses() {
        // Group messages can have multiple addresses in the array
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [
                    {"address": "+1234567890"},
                    {"address": "+0987654321"}
                ],
                "body": "Group message",
                "date": 1700000000000i64,
                "type": 2,
                "read": 1,
                "threadID": 200,
                "uID": 2,
                "event": 0
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle multiple addresses"
        );

        let thread = plugin.get_thread("test", 200);
        assert_eq!(thread.len(), 1);
    }

    #[tokio::test]
    async fn test_handle_sms_with_event_flags() {
        // Android sends event flags - should not cause parse error
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [{"address": "+1234567890"}],
                "body": "Incoming SMS",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0,
                "threadID": 300,
                "uID": 3,
                "event": 1,  // EVENT_SINGLE_PART_SMS
                "subscriptionID": 1
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(result.is_ok(), "SMS plugin should handle event flags");
    }

    #[tokio::test]
    async fn test_handle_sms_without_optional_fields() {
        // Minimum required fields from Android's toJSONObject
        let plugin = SmsPlugin::new();
        let packet = Packet::new(
            "kdeconnect.sms.messages".to_string(),
            serde_json::json!({"messages": [{
                "addresses": [{"address": "+1234567890"}],
                "body": "Minimal message",
                "date": 1700000000000i64,
                "type": 1,
                "read": 0,
                "threadID": 400,
                "uID": 4,
                "event": 0
                // subscriptionID is optional in some cases
            }]}),
        );
        let result = plugin.handle_packet("test", packet).await;
        assert!(
            result.is_ok(),
            "SMS plugin should handle minimal message format"
        );
    }
}
