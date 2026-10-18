//! Bounded, deduplicated MMS requests. Reserve a download slot BEFORE asking
//! Android to open its short-lived payload listener, rather than queueing open
//! listeners that may expire while other files download.
use super::{SmsPlugin, SmsRuntime};
use crate::{
    plugins::PluginEvent,
    protocol::types::Packet,
    utils::{Error, Result},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

type Key = (String, String);
const MAX_REQUESTS: usize = 64;
const MAX_RESULTS: usize = 256;

#[derive(Clone, Debug)]
pub struct AttachmentResult {
    pub device: String,
    pub identifier: String,
    pub revision: u64,
    pub path: Option<String>,
    pub error: Option<String>,
}
struct Pending {
    serial: u64,
    requested: bool,
    cancelled: CancellationToken,
    response: Option<oneshot::Sender<Packet>>,
}
pub(super) struct AttachmentQueue {
    slots: Arc<Semaphore>,
    capacity: Arc<Semaphore>,
    jobs: Mutex<HashMap<Key, Pending>>,
    results: Mutex<HashMap<Key, AttachmentResult>>,
    sequence: AtomicU64,
}
impl AttachmentQueue {
    pub(super) fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(4)),
            capacity: Arc::new(Semaphore::new(MAX_REQUESTS)),
            jobs: Mutex::new(HashMap::new()),
            results: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(1),
        }
    }
    pub(super) fn cancel_device(&self, device: &str) {
        for ((id, _), job) in self.jobs.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if id == device {
                job.cancelled.cancel();
            }
        }
    }
    pub(super) fn deliver(&self, device: &str, packet: Packet) -> Result<()> {
        let identifier = packet
            .body
            .get("filename")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::InvalidRequest("Missing attachment filename".into()))?;
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let job = jobs
            .get_mut(&(device.into(), identifier.into()))
            .filter(|job| job.requested && !job.cancelled.is_cancelled())
            .ok_or_else(|| Error::InvalidRequest("Unrequested SMS attachment".into()))?;
        // Duplicate responses cannot start a second write to the same file.
        if let Some(response) = job.response.take() {
            response
                .send(packet)
                .map_err(|_| Error::InvalidRequest("Attachment request expired".into()))?;
        }
        Ok(())
    }
    fn finish(
        &self,
        runtime: &SmsRuntime,
        key: Key,
        serial: u64,
        outcome: Result<std::path::PathBuf>,
    ) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        // An immediately reconnected phone may already have a NEW request.
        if !jobs.get(&key).is_some_and(|job| job.serial == serial) {
            return;
        }
        jobs.remove(&key);
        let (path, error) = match outcome {
            Ok(path) => (Some(path.to_string_lossy().into_owned()), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let result = AttachmentResult {
            device: key.0.clone(),
            identifier: key.1.clone(),
            revision: self.sequence.fetch_add(1, Ordering::Relaxed),
            path: path.clone(),
            error: error.clone(),
        };
        let mut results = self.results.lock().unwrap_or_else(|e| e.into_inner());
        results.insert(key.clone(), result);
        if results.len() > MAX_RESULTS {
            if let Some(oldest) = results
                .iter()
                .min_by_key(|(_, r)| r.revision)
                .map(|(key, _)| key.clone())
            {
                results.remove(&oldest);
            }
        }
        // Commit the result before publishing; lag recovery reads this journal.
        drop(results);
        drop(jobs);
        if let Some(file_path) = path {
            runtime
                .events
                .broadcast(PluginEvent::SmsAttachmentReceived {
                    device_id: key.0,
                    unique_identifier: key.1,
                    file_path,
                });
        } else if let Some(error) = error {
            tracing::warn!(%error,"SMS attachment request failed");
            runtime.events.broadcast(PluginEvent::SmsAttachmentFailed {
                device_id: key.0,
                unique_identifier: key.1,
                error,
            });
        }
    }
}

pub fn request_packet(part: i64, identifier: &str) -> Packet {
    Packet::new(
        "kdeconnect.sms.request_attachment".into(),
        serde_json::json!({"part_id":part,"unique_identifier":identifier}),
    )
}
impl SmsPlugin {
    /// Success means queued, not that a file has already downloaded. Completion
    /// or failure is published and retained separately; duplicates coalesce.
    pub fn request_attachment(&self, device: &str, part: i64, identifier: &str) -> Result<()> {
        if part < 0 || identifier.is_empty() || identifier.len() > 1024 {
            return Err(Error::InvalidRequest("Invalid attachment identity".into()));
        }
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("SMS payload transport unavailable".into()))?;
        let queue = &runtime.attachments;
        let key = (device.to_string(), identifier.to_string());
        let mut jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs
            .get(&key)
            .is_some_and(|job| !job.cancelled.is_cancelled())
        {
            return Ok(());
        }
        let capacity = queue.capacity.clone().try_acquire_owned().map_err(|_| {
            Error::InvalidRequest("SMS attachment queue is full; try again shortly".into())
        })?;
        let serial = queue.sequence.fetch_add(1, Ordering::Relaxed);
        let cancelled = CancellationToken::new();
        let (response, receiver) = oneshot::channel();
        jobs.insert(
            key.clone(),
            Pending {
                serial,
                requested: false,
                cancelled: cancelled.clone(),
                response: Some(response),
            },
        );
        drop(jobs);
        let plugin = self.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let _capacity = capacity;
            let work = plugin.download_requested(&runtime, &key, serial, part, receiver);
            let result = tokio::select! {
                biased;
                _=cancelled.cancelled()=>Err(Error::InvalidRequest("Phone disconnected before attachment download completed".into())),
                result=tokio::time::timeout(std::time::Duration::from_secs(300),work)=>result.unwrap_or_else(|_|Err(Error::ConnectionTimeout("SMS attachment download timed out".into()))),
            };
            runtime.attachments.finish(&runtime, key, serial, result);
        });
        Ok(())
    }
    async fn download_requested(
        &self,
        runtime: &SmsRuntime,
        key: &Key,
        serial: u64,
        part: i64,
        response: oneshot::Receiver<Packet>,
    ) -> Result<std::path::PathBuf> {
        let _slot: OwnedSemaphorePermit =
            runtime
                .attachments
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::InvalidRequest("SMS attachment queue stopped".into()))?;
        {
            let mut jobs = runtime
                .attachments
                .jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let job = jobs
                .get_mut(key)
                .filter(|job| job.serial == serial)
                .ok_or_else(|| Error::InvalidRequest("SMS attachment request expired".into()))?;
            job.requested = true;
        }
        runtime
            .connections
            .send_packet(&key.0, &request_packet(part, &key.1))
            .await?;
        let packet = tokio::time::timeout(std::time::Duration::from_secs(30), response)
            .await
            .map_err(|_| Error::ConnectionTimeout("Phone did not provide the attachment".into()))?
            .map_err(|_| Error::InvalidRequest("Attachment response was cancelled".into()))?;
        self.receive_attachment(&key.0, packet).await
    }
    pub fn attachment_results(&self) -> Vec<AttachmentResult> {
        self.runtime
            .as_ref()
            .map(|r| {
                r.attachments
                    .results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn attachment_state(&self, device: &str, identifier: &str) -> (String, String) {
        let Some(runtime) = &self.runtime else {
            return (
                "unavailable".into(),
                "SMS payload transport unavailable".into(),
            );
        };
        let key = (device.into(), identifier.into());
        let jobs = runtime
            .attachments
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get(&key) {
            return (
                if job.requested {
                    "downloading"
                } else {
                    "queued"
                }
                .into(),
                String::new(),
            );
        }
        let results = runtime
            .attachments
            .results
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match results.get(&key) {
            Some(result) if result.error.is_some() => {
                ("failed".into(), result.error.clone().unwrap_or_default())
            }
            Some(_) => ("ready".into(), String::new()),
            None => ("unknown".into(), String::new()),
        }
    }
}
