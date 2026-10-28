//! KDE conversation values and the Android v2 messaging adapter.
use super::{failure, Context};
use crate::{plugins::sms::SmsMessage, protocol::types::Packet};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{collections::HashSet, sync::Arc};
use zbus::{
    interface,
    zvariant::{OwnedValue, Type, Value},
};

#[derive(Clone, serde::Serialize, serde::Deserialize, Type, Value, OwnedValue)]
pub struct Address {
    pub address: String,
}
#[derive(Clone, serde::Serialize, serde::Deserialize, Type, Value, OwnedValue)]
pub struct Attachment {
    pub part: i64,
    pub mime: String,
    pub thumbnail: String,
    pub identifier: String,
}
#[derive(Clone, serde::Serialize, serde::Deserialize, Type, Value, OwnedValue)]
pub struct Message {
    pub event: i32,
    pub body: String,
    pub addresses: Vec<Address>,
    pub date: i64,
    pub kind: i32,
    pub read: i32,
    pub thread: i64,
    pub uid: i32,
    pub sim: i64,
    pub attachments: Vec<Attachment>,
}

impl TryFrom<&SmsMessage> for Message {
    type Error = zbus::fdo::Error;
    fn try_from(m: &SmsMessage) -> Result<Self, Self::Error> {
        let original = m.uid.or(m.message_id).unwrap_or(0);
        let uid = i32::try_from(original).map_err(|_| {
            failure(format!(
            "Message ID {original} exceeds the KDE conversation interface's signed 32-bit range"
        ))
        })?;
        let attachments = m
            .attachments
            .iter()
            .flatten()
            .map(|a| {
                let string = |key, old| {
                    a.get(key)
                        .or_else(|| a.get(old))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                Attachment {
                    part: a
                        .get("part_id")
                        .or_else(|| a.get("partID"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(-1),
                    mime: string("mime_type", "mimeType"),
                    thumbnail: string("encoded_thumbnail", "encodedThumbnail"),
                    identifier: string("unique_identifier", "uniqueIdentifier"),
                }
            })
            .collect();
        Ok(Self {
            event: m.event.unwrap_or(0),
            body: m.body.clone().unwrap_or_default(),
            addresses: m
                .addresses
                .iter()
                .filter_map(|a| a.address.clone().map(|address| Address { address }))
                .collect(),
            date: m.date.unwrap_or(0),
            kind: m.message_type.unwrap_or(1),
            read: m.read.unwrap_or(0),
            thread: m.thread_id.unwrap_or(0),
            uid,
            sim: m.subscription_id.unwrap_or(-1) as i64,
            attachments,
        })
    }
}

/// Matches SMSPlugin.kt: addresses, messageBody, attachments and implemented subID.
pub fn send_packet(
    addresses: Vec<String>,
    text: &str,
    attachments: Vec<serde_json::Value>,
    sim: i64,
) -> zbus::fdo::Result<Packet> {
    let mut seen = HashSet::new();
    let addresses: Vec<_> = addresses
        .into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty() && seen.insert(a.clone()))
        .map(|address| serde_json::json!({"address":address}))
        .collect();
    if addresses.is_empty() || (text.trim().is_empty() && attachments.is_empty()) {
        return Err(failure("Recipient and message or attachment required"));
    }
    let mut body = serde_json::json!({"version":2, "addresses":addresses,"messageBody":text});
    if !attachments.is_empty() {
        body["attachments"] = attachments.into();
    }
    if sim >= 0 {
        body["subID"] = sim.into();
    }
    Ok(Packet::new("kdeconnect.sms.request".into(), body))
}

pub fn history_packet(thread: i64, count: usize) -> Packet {
    Packet::new(
        "kdeconnect.sms.request_conversation".into(),
        serde_json::json!({"threadID":thread, "rangeStartTimestamp":-1,"numberToRequest":count}),
    )
}
pub fn attachment_packet(part: i64, identifier: &str) -> Packet {
    crate::plugins::sms::attachment_request_packet(part, identifier)
}

async fn encode_attachments(paths: Vec<OwnedValue>) -> zbus::fdo::Result<Vec<serde_json::Value>> {
    let mut attachments = Vec::new();
    let mut size = 0usize;
    if paths.len() > 16 {
        return Err(failure("Too many MMS attachments"));
    }
    for value in paths {
        let path = String::try_from(value).map_err(failure)?;
        let path = std::path::Path::new(&path);
        if !path.is_absolute() {
            return Err(failure("An MMS attachment must be an absolute local path"));
        }
        if !tokio::fs::metadata(path).await.map_err(failure)?.is_file() {
            return Err(failure("MMS attachment is not a regular file"));
        }
        let file = tokio::fs::File::open(path).await.map_err(failure)?;
        if !file.metadata().await.map_err(failure)?.is_file() {
            return Err(failure("MMS attachment is not a regular file"));
        }
        use tokio::io::AsyncReadExt;
        let mut data = Vec::new();
        file.take((8 * 1024 * 1024 + 1) as u64)
            .read_to_end(&mut data)
            .await
            .map_err(failure)?;
        size += data.len();
        if size > 8 * 1024 * 1024 {
            return Err(failure("MMS attachments exceed 8 MiB; compress them first"));
        }
        let mime = mime_type(path);
        attachments.push(serde_json::json!({"fileName":path.file_name().unwrap_or_default().to_string_lossy(), "base64EncodedFile":STANDARD.encode(data),"mimeType":mime}));
    }
    Ok(attachments)
}
fn mime_type(path: &std::path::Path) -> &'static str {
    path.extension()
        .and_then(|s| s.to_str())
        .and_then(crate::utils::compat::media_type)
        .unwrap_or("application/octet-stream")
}

pub(crate) struct Conversations {
    pub context: Arc<Context>,
    pub device: String,
}
#[interface(name = "org.kde.kdeconnect.device.conversations")]
impl Conversations {
    #[zbus(name = "activeConversations")]
    async fn active(&self) -> zbus::fdo::Result<Vec<OwnedValue>> {
        let mut result = Vec::new();
        for thread in self.context.app.plugins.sms.get_threads(&self.device) {
            if let Some(message) = self
                .context
                .app
                .plugins
                .sms
                .get_thread(&self.device, thread.thread_id)
                .last()
            {
                // As with live signals, a thread whose message D-Bus cannot carry is skipped.
                let Ok(message) = Message::try_from(message) else {
                    continue;
                };
                result.push(OwnedValue::try_from(Value::from(message)).map_err(failure)?);
            }
        }
        Ok(result)
    }
    #[zbus(name = "requestAllConversationThreads")]
    async fn all(&self) -> zbus::fdo::Result<()> {
        self.context.require_device(&self.device).await?;
        self.context
            .app
            .plugins
            .sms
            .begin_conversation_refresh(&self.device);
        self.context
            .send(
                &self.device,
                Packet::new(
                    "kdeconnect.sms.request_conversations".into(),
                    serde_json::json!({}),
                ),
            )
            .await
    }
    #[zbus(name = "requestConversation")]
    async fn history(&self, thread: i64, start: i32, end: i32) -> zbus::fdo::Result<()> {
        if start < 0 || end < start || end > 500 {
            return Err(failure("Invalid history range (maximum 500 messages)"));
        }
        {
            let mut watched = self.context.watched_threads.lock().await;
            if watched
                .iter()
                .filter(|(device, _)| device == &self.device)
                .count()
                >= crate::plugins::sms::MAX_SMS_THREADS_PER_DEVICE
                && !watched.contains(&(self.device.clone(), thread))
            {
                return Err(failure("Too many watched conversations"));
            }
            watched.insert((self.device.clone(), thread));
        }
        let cached = self
            .context
            .app
            .plugins
            .sms
            .get_thread(&self.device, thread);
        for m in cached
            .iter()
            .rev()
            .skip(start as usize)
            .take((end - start) as usize)
        {
            self.context
                .message_signal(&self.device, "conversationUpdated", m)
                .await
                .map_err(failure)?;
        }
        // Complete the cache replay, including empty ranges; later phone batches
        // produce their own completion after every message has been published.
        self.context
            .emit(
                &self.device,
                "org.kde.kdeconnect.device.conversations",
                "conversationLoaded",
                &(thread, cached.len() as u64),
            )
            .await
            .map_err(failure)?;
        self.context
            .send(&self.device, history_packet(thread, end as usize))
            .await
    }
    #[zbus(name = "sendWithoutConversation")]
    async fn new_message(
        &self,
        addresses: Vec<OwnedValue>,
        text: &str,
        paths: Vec<OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let addresses = addresses
            .into_iter()
            .map(|a| {
                String::try_from(a.try_clone().map_err(failure)?)
                    .or_else(|_| Address::try_from(a).map(|a| a.address))
                    .map_err(failure)
            })
            .collect::<zbus::fdo::Result<Vec<_>>>()?;
        let files = encode_attachments(paths).await?;
        self.context
            .send(&self.device, send_packet(addresses, text, files, -1)?)
            .await
    }
    #[zbus(name = "replyToConversation")]
    async fn reply(
        &self,
        thread: i64,
        text: &str,
        paths: Vec<OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let messages = self
            .context
            .app
            .plugins
            .sms
            .get_thread(&self.device, thread);
        let reference = messages
            .iter()
            .rev()
            .find(|m| m.message_type == Some(2) && !m.addresses.is_empty())
            .or_else(|| messages.last())
            .ok_or_else(|| failure("Conversation must be loaded before replying"))?;
        let addresses = reference
            .addresses
            .iter()
            .filter_map(|a| a.address.clone())
            .collect();
        self.context
            .send(
                &self.device,
                send_packet(
                    addresses,
                    text,
                    encode_attachments(paths).await?,
                    reference.subscription_id.unwrap_or(-1) as i64,
                )?,
            )
            .await
    }
    #[zbus(name = "requestAttachmentFile")]
    async fn attachment(&self, part: i64, identifier: &str) -> zbus::fdo::Result<()> {
        if part < 0 || identifier.is_empty() || identifier.len() > 1024 {
            return Err(failure("Invalid attachment identity"));
        }
        // Cached copies open while the phone is away, but never once it is revoked.
        if !self
            .context
            .app
            .pairing_handler
            .is_paired(&self.device)
            .await
        {
            return Err(failure("Phone is not paired"));
        }
        if let Some(path) = self
            .context
            .app
            .plugins
            .sms
            .attachment_path(&self.device, identifier)
        {
            if tokio::fs::try_exists(&path).await.map_err(failure)? {
                return self
                    .context
                    .emit(
                        &self.device,
                        "org.kde.kdeconnect.device.conversations",
                        "attachmentReceived",
                        &(path.to_string_lossy().as_ref(), identifier),
                    )
                    .await
                    .map_err(failure);
            }
        }
        self.context.require_device(&self.device).await?;
        self.context
            .app
            .plugins
            .sms
            .request_attachment(&self.device, part, identifier)
            .map_err(failure)
    }
    /// Mainstream extension: drop a cached copy that failed to decode and fetch the file
    /// from the phone again. `requestAttachmentFile` keeps returning a cached copy.
    #[zbus(name = "refreshAttachmentFile")]
    async fn refresh_attachment(&self, part: i64, identifier: &str) -> zbus::fdo::Result<()> {
        if part < 0 || identifier.is_empty() || identifier.len() > 1024 {
            return Err(failure("Invalid attachment identity"));
        }
        self.context.require_device(&self.device).await?;
        if let Some(path) = self
            .context
            .app
            .plugins
            .sms
            .attachment_path(&self.device, identifier)
        {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(failure(error)),
            }
        }
        self.context
            .app
            .plugins
            .sms
            .request_attachment(&self.device, part, identifier)
            .map_err(failure)
    }
    #[zbus(signal, name = "conversationCreated")]
    pub async fn created(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        message: OwnedValue,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "conversationUpdated")]
    pub async fn updated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        message: OwnedValue,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "conversationError")]
    pub async fn failed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        thread: i64,
        error: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "conversationLoaded")]
    pub async fn loaded(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        thread: i64,
        count: u64,
    ) -> zbus::Result<()>;
    /// A new list is starting, not a claim that unlisted histories were deleted.
    #[zbus(signal, name = "conversationListReset")]
    pub async fn list_reset(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal, name = "conversationRemoved")]
    pub async fn removed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        thread: i64,
    ) -> zbus::Result<()>;
    #[zbus(name = "attachmentState")]
    fn attachment_state(&self, identifier: &str) -> (String, String) {
        self.context
            .app
            .plugins
            .sms
            .attachment_state(&self.device, identifier)
    }
    #[zbus(signal, name = "attachmentFailed")]
    pub async fn attachment_failed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        identifier: &str,
        error: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal, name = "attachmentReceived")]
    pub async fn received(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        path: &str,
        identifier: &str,
    ) -> zbus::Result<()>;
}

pub(crate) struct Sms {
    pub context: Arc<Context>,
    pub device: String,
}
#[interface(name = "org.kde.kdeconnect.device.sms")]
impl Sms {
    #[zbus(name = "requestAllConversations")]
    async fn all(&self) -> zbus::fdo::Result<()> {
        self.context
            .send(
                &self.device,
                Packet::new(
                    "kdeconnect.sms.request_conversations".into(),
                    serde_json::json!({}),
                ),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn android_v2_metadata_survives_the_dbus_boundary() {
        // SMSHelper.kt@f6a699d: Message.toJsonObject and Attachment.toJson.
        let message:SmsMessage=serde_json::from_value(serde_json::json!({"thread_id":42,"_id":75,"sub_id":3,"body":"voice note","date":1700000001000i64,"type":1,"read":false,
            "addresses":[{"address":"+15550000001"}],"attachments":[{"part_id":12,"mime_type":"audio/amr","encoded_thumbnail":"","unique_identifier":"voice-note.amr"}]})).unwrap();
        let wire = Message::try_from(&message).unwrap();
        assert_eq!((wire.thread, wire.uid, wire.sim, wire.read), (42, 75, 3, 0));
        assert_eq!(wire.attachments[0].mime, "audio/amr");
        assert_eq!(wire.attachments[0].identifier, "voice-note.amr");
        assert_eq!(Message::SIGNATURE.to_string(), "(isa(s)xiixixa(xsss))");
        assert_eq!(Address::SIGNATURE.to_string(), "(s)");
    }
    #[tokio::test]
    async fn attachment_only_mms_contains_actual_bytes_and_mime() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("voice.m4a");
        tokio::fs::write(&path, b"test-voice-content")
            .await
            .unwrap();
        let file = OwnedValue::from(zbus::zvariant::Str::from(path.to_str().unwrap()));
        let encoded = encode_attachments(vec![file]).await.unwrap();
        let packet = send_packet(vec!["+15550000001".into()], "", encoded, 7).unwrap();
        assert_eq!(packet.body["version"], 2);
        assert_eq!(packet.body["addresses"][0]["address"], "+15550000001");
        assert_eq!(packet.body["subID"], 7);
        assert!(packet.body.get("phoneNumbers").is_none());
        assert_eq!(packet.body["attachments"][0]["fileName"], "voice.m4a");
        assert_eq!(packet.body["attachments"][0]["mimeType"], "audio/mp4");
        assert_eq!(
            STANDARD
                .decode(
                    packet.body["attachments"][0]["base64EncodedFile"]
                        .as_str()
                        .unwrap()
                )
                .unwrap(),
            b"test-voice-content"
        );
    }
    #[test]
    fn request_packets_use_androids_receiver_keys() {
        let packet = history_packet(52, 100);
        assert_eq!(
            packet.body,
            serde_json::json!({"threadID":52,"rangeStartTimestamp":-1,"numberToRequest":100})
        );
        assert_eq!(
            attachment_packet(12, "voice.amr").body,
            serde_json::json!({"part_id":12,"unique_identifier":"voice.amr"})
        );
    }
    #[tokio::test]
    async fn invalid_outbound_requests_are_rejected_before_sending() {
        assert!(send_packet(vec![], "hello", vec![], -1).is_err());
        assert!(send_packet(vec!["+15550000001".into()], "", vec![], -1).is_err());
        assert!(
            encode_attachments(vec![OwnedValue::from(zbus::zvariant::Str::from(
                "relative.jpg"
            ))])
            .await
            .is_err()
        );
    }
    #[test]
    fn message_ids_at_the_abi_boundary_are_checked_without_aliasing_zero() {
        for id in [i32::MIN as i64, 0, i32::MAX as i64] {
            let message = SmsMessage {
                uid: Some(id),
                ..Default::default()
            };
            assert_eq!(Message::try_from(&message).unwrap().uid as i64, id);
        }
        for id in [i32::MIN as i64 - 1, i32::MAX as i64 + 1, i64::MAX] {
            let message = SmsMessage {
                uid: Some(id),
                ..Default::default()
            };
            let error = Message::try_from(&message).err().unwrap().to_string();
            assert!(error.contains(&id.to_string()));
            assert!(error.contains("32-bit"));
        }
    }
}
