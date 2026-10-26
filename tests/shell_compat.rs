//! Private-bus tests of the real Phone-tab/island contract. No real phone or
//! desktop bus is touched. Fixtures follow KDE Android f6a699d, not Rust types.
use futures::StreamExt;
use rust_connect::{
    app::AppState,
    config::settings::AppSettings,
    dbus::{conversations::Message, DBusService},
    device::{Device, DeviceType},
    plugins::plugin::Plugin,
    protocol::types::Packet,
};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use zbus::zvariant::OwnedValue;

struct Bus {
    child: Child,
    address: String,
}
impl Bus {
    fn new() -> Self {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().into(),
        }
    }
    async fn connect(&self, name: Option<&str>) -> zbus::Connection {
        let mut builder = zbus::connection::Builder::address(self.address.as_str()).unwrap();
        if let Some(name) = name {
            builder = builder.name(name).unwrap()
        }
        builder.build().await.unwrap()
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
type NotificationContent = (String, String, Vec<String>);

#[derive(Clone, Default)]
struct Desktop {
    counter: Arc<AtomicU32>,
    seen: Arc<Mutex<Vec<NotificationContent>>>,
    hints: Arc<Mutex<Vec<HashMap<String, OwnedValue>>>>,
    icons: Arc<Mutex<Vec<String>>>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    entered: Arc<AtomicU32>,
}
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Desktop {
    #[allow(clippy::too_many_arguments)] // The freedesktop Notify wire signature.
    #[zbus(name = "Notify")]
    async fn notify(
        &self,
        _app: &str,
        replaces: u32,
        icon: &str,
        title: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        _timeout: i32,
    ) -> u32 {
        self.entered.fetch_add(1, Ordering::Relaxed);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        self.hints.lock().unwrap().push(hints);
        self.icons.lock().unwrap().push(icon.into());
        self.seen
            .lock()
            .unwrap()
            .push((title.into(), body.into(), actions));
        if replaces != 0 {
            replaces
        } else {
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        }
    }
    #[zbus(name = "CloseNotification")]
    fn close(&self, _id: u32) {}
}
async fn until<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn state(directory: &std::path::Path) -> Arc<AppState> {
    let settings = AppSettings::new_with_data_dir(directory.into())
        .with_api_keys(vec!["private-bus-test".into()]);
    let app = Arc::new(AppState::new_without_input(settings).unwrap());
    // Never let preloaded fixture packets reach the real desktop bus.
    app.plugins.notification.use_compatibility_desktop(true);
    app.init_plugins().await;
    app.registry
        .add(Device::new(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "Test phone".into(),
            DeviceType::Phone,
            8,
        ))
        .await
        .unwrap();
    app
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_conversation_signals_and_variants_preserve_android_history() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let connection = bus.connect(Some("org.kde.kdeconnect")).await;
    let service = DBusService::on_connection(
        app.clone(),
        connection,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    let mut completed = proxy.receive_signal("conversationLoaded").await.unwrap();
    let mut updated = proxy.receive_signal("conversationUpdated").await.unwrap();
    let body: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/upstream-wire/sms/android_v2_batch.json"
    ))
    .unwrap();
    for _ in 0..2 {
        app.plugins
            .sms
            .handle_packet(
                "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Packet::new("kdeconnect.sms.messages".into(), body.clone()),
            )
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), completed.next())
        .await
        .unwrap()
        .unwrap();
    let values: Vec<OwnedValue> = proxy.call("activeConversations", &()).await.unwrap();
    assert_eq!(values.len(), 1);
    let message = Message::try_from(values.into_iter().next().unwrap()).unwrap();
    assert_eq!(message.uid, 75);
    assert_eq!(message.thread, 42);
    assert_eq!(message.attachments[0].mime, "audio/amr");
    assert_eq!(
        app.plugins
            .sms
            .get_thread("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", 42)
            .len(),
        2
    );
    let messages = app
        .plugins
        .sms
        .get_thread("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", 42);
    assert_eq!(messages[0].uid, Some(74));
    // A read-status update may omit the attachment metadata. The shell must
    // receive the complete original record, rather than a blank media message.
    app.plugins
        .sms
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.sms.messages".into(),
                serde_json::json!({"messages":[
                    {"thread_id":42,"_id":75,"read":1}
                ]}),
            ),
        )
        .await
        .unwrap();
    let enriched = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let signal = updated.next().await.unwrap();
            let (value,): (OwnedValue,) = signal.body().deserialize().unwrap();
            let message = Message::try_from(value).unwrap();
            if message.uid == 75 && message.read == 1 {
                break message;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(enriched.body, "A newer message");
    assert_eq!(enriched.date, 1700000002000);
    assert_eq!(enriched.attachments[0].identifier, "voice-note.amr");
    assert_eq!(enriched.addresses[0].address, "+15550000001");
    let failed: Result<(), _> = proxy
        .call(
            "sendWithoutConversation",
            &(Vec::<OwnedValue>::new(), "hello", Vec::<OwnedValue>::new()),
        )
        .await;
    assert!(failed.is_err());
    service.stop().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_notification_keys_are_legal_objects_and_reconnects_do_not_duplicate_popups() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let desktop = Desktop::default();
    let seen = desktop.seen.clone();
    let _desktop_bus = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", desktop)
        .unwrap()
        .build()
        .await
        .unwrap();
    let connection = bus.connect(Some("org.kde.kdeconnect")).await;
    let service = DBusService::on_connection(
        app.clone(),
        connection,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications",
        "org.kde.kdeconnect.device.notifications",
    )
    .await
    .unwrap();
    let raw = "0|org.thoughtcrime.securesms|42|null|10123";
    let body = serde_json::json!({"id":raw,"appName":"Signal","title":"Sender","text":"Hello <test>","actions":["Mark as read"],"requestReplyId":"reply-handle"});
    app.plugins
        .notification
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new("kdeconnect.notification".into(), body.clone()),
        )
        .await
        .unwrap();
    until(|| async { seen.lock().unwrap().len() == 1 }).await;
    app.plugins
        .notification
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new("kdeconnect.notification".into(), body),
        )
        .await
        .unwrap();
    // A subsequent different notification is the event-loop barrier for dedupe.
    app.plugins
        .notification
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.notification".into(),
                serde_json::json!({"id":"second","appName":"Mail","text":"Other"}),
            ),
        )
        .await
        .unwrap();
    until(|| async { seen.lock().unwrap().len() == 2 }).await;
    let keys: Vec<String> = proxy.call("activeNotifications", &()).await.unwrap();
    assert_eq!(keys.len(), 2);
    let key = keys.iter().find(|k| k.starts_with("rc_")).unwrap();
    let item = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        format!("/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications/{key}"),
        "org.kde.kdeconnect.device.notifications.notification",
    )
    .await
    .unwrap();
    assert_eq!(
        item.get_property::<String>("internalId").await.unwrap(),
        raw
    );
    assert_eq!(
        item.get_property::<String>("replyId").await.unwrap(),
        "reply-handle"
    );
    assert!(seen.lock().unwrap()[0].1.contains("&lt;test&gt;"));
    app.plugins
        .notification
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.notification".into(),
                serde_json::json!({"id":raw,"isCancel":true}),
            ),
        )
        .await
        .unwrap();
    until(|| async {
        proxy
            .call::<_, _, Vec<String>>("activeNotifications", &())
            .await
            .unwrap()
            .len()
            == 1
    })
    .await;
    service.stop().await;
}
#[test]
fn storage_roots_and_identifiers_match_the_shells_actual_path_usage() {
    use rust_connect::{
        dbus::files::{directory_roots, local_file},
        plugins::sftp::SftpConnectionInfo,
        utils::compat::component,
    };
    let info = SftpConnectionInfo {
        ip: "127.0.0.1".into(),
        port: 1234,
        user: "phone".into(),
        password: "redacted".into(),
        path: "/storage/emulated/0".into(),
        multi_paths: vec![
            "/storage/emulated/0".into(),
            "/storage/emulated/0/DCIM".into(),
        ],
        path_names: vec!["Storage".into(), "Camera".into()],
    };
    let paths = directory_roots(&info, std::path::Path::new("/home/user/mount"));
    assert!(paths.contains_key("/home/user/mount"));
    assert!(paths.contains_key("/home/user/mount/DCIM"));
    assert!(!paths.contains_key("/home/user/mount/storage/emulated/0"));
    let mut multi = info.clone();
    multi.multi_paths.push("/storage/1234-5678".into());
    let roots = directory_roots(&multi, std::path::Path::new("/home/user/mount"));
    assert!(roots.contains_key("/home/user/mount/emulated/0"));
    assert!(roots.contains_key("/home/user/mount/1234-5678"));
    assert_eq!(
        local_file("file:///home/user/My%20Photo.png")
            .unwrap()
            .to_str()
            .unwrap(),
        "/home/user/My Photo.png"
    );
    assert!(local_file("file://remote/path").is_err());
    for id in [
        "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "phone-1",
        "rc_70686f6e65",
        "0|app|42",
    ] {
        assert!(
            zbus::zvariant::OwnedObjectPath::try_from(format!("/devices/{}", component(id)))
                .is_ok()
        );
    }
    assert_ne!(component("phone-1"), component(&component("phone-1")));
}

async fn pair(app: &AppState, certificate: Vec<u8>) {
    app.pairing_handler
        .receive_pair_request(
            &"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            Some(chrono::Utc::now().timestamp()),
        )
        .await
        .unwrap();
    app.pairing_handler
        .set_pending_peer_cert(&"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(), certificate)
        .await;
    app.pairing_handler
        .accept_pairing(&"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contact_photos_are_published_atomically_and_removed_when_revoked() {
    use rust_connect::protocol::CertificateManager;
    use std::os::unix::fs::PermissionsExt;
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let peer_dir = tempfile::tempdir().unwrap();
    let certificates = CertificateManager::new(peer_dir.path().into());
    let (pem, _) = certificates
        .generate_certificate("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Phone")
        .unwrap();
    pair(
        &app,
        openssl::x509::X509::from_pem(&pem)
            .unwrap()
            .to_der()
            .unwrap(),
    )
    .await;
    let connection = bus.connect(Some("org.kde.kdeconnect")).await;
    let contacts = temp.path().join("contacts");
    let service = DBusService::on_connection(
        app.clone(),
        connection,
        contacts.clone(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let root = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect",
        "org.kde.kdeconnect.daemon",
    )
    .await
    .unwrap();
    // Pairing authority, even though the discovery record has never connected.
    let devices: Vec<String> = root.call("devices", &(false, true)).await.unwrap();
    assert_eq!(devices, vec!["phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
    let card = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Mom\r\nTEL;TYPE=CELL:+15550000001\r\nPHOTO;ENCODING=b;TYPE=JPEG:YWJj\r\n ZGVm\r\nEND:VCARD\r\n";
    app.plugins
        .contacts
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.contacts.response_uids_timestamps".into(),
                serde_json::json!({"uids":["../../contact"],"../../contact":"1"}),
            ),
        )
        .await
        .unwrap();
    assert!(app
        .plugins
        .contacts
        .synchronized_contacts("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .is_none());
    app.plugins
        .contacts
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.contacts.response_vcards".into(),
                serde_json::json!({"uids":["../../contact"],"../../contact":card}),
            ),
        )
        .await
        .unwrap();
    let folder = contacts.join("kdeconnect-phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let file = folder.join(format!(
        "rc_{}.vcf",
        rust_connect::plugins::sms::attachment_filename("../../contact")
    ));
    until(|| async { tokio::fs::try_exists(&file).await.unwrap_or(false) }).await;
    assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), card);
    assert_eq!(
        tokio::fs::metadata(&file)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        tokio::fs::metadata(&folder)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    app.plugins
        .contacts
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.contacts.response_uids_timestamps".into(),
                serde_json::json!({"uids":["../../contact"],"../../contact":"2"}),
            ),
        )
        .await
        .unwrap();
    // A changed card cannot complete synchronization using its OLD photo.
    assert!(app
        .plugins
        .contacts
        .synchronized_contacts("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .is_none());
    let updated = card.replace("Mom", "Mother");
    app.plugins
        .contacts
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.contacts.response_vcards".into(),
                serde_json::json!({"uids":["../../contact"],"../../contact":updated}),
            ),
        )
        .await
        .unwrap();
    until(|| async {
        tokio::fs::read_to_string(&file)
            .await
            .is_ok_and(|s| s == updated)
    })
    .await;
    app.plugins
        .contacts
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.contacts.response_uids_timestamps".into(),
                serde_json::json!({"uids":[]}),
            ),
        )
        .await
        .unwrap();
    until(|| async { !tokio::fs::try_exists(&file).await.unwrap_or(true) }).await;
    app.pairing_handler
        .unpair(&"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
        .await
        .unwrap();
    until(|| async { !tokio::fs::try_exists(&folder).await.unwrap_or(true) }).await;
    let devices: Vec<String> = root.call("devices", &(false, true)).await.unwrap();
    assert!(devices.is_empty());
    service.stop().await;
    let dbus = zbus::fdo::DBusProxy::new(&client).await.unwrap();
    assert!(!dbus
        .name_has_owner("org.kde.kdeconnect".try_into().unwrap())
        .await
        .unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_history_batch_keeps_every_message_and_completes() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    let mut completed = proxy.receive_signal("conversationLoaded").await.unwrap();
    let messages: Vec<_> = (0..400).rev().map(|id|serde_json::json!({"thread_id":9,"_id":id,"date":1700000000000i64+id,"body":format!("message {id}"),"type":1,"read":0})).collect();
    app.plugins
        .sms
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.sms.messages".into(),
                serde_json::json!({"version":2,"messages":messages}),
            ),
        )
        .await
        .unwrap();
    let signal = tokio::time::timeout(Duration::from_secs(10), completed.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(signal.body().deserialize::<(i64, u64)>().unwrap(), (9, 400));
    let stored = app
        .plugins
        .sms
        .get_thread("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", 9);
    assert_eq!(stored.len(), 400);
    assert_eq!(stored.first().unwrap().uid, Some(0));
    assert_eq!(stored.last().unwrap().uid, Some(399));
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dbus_sends_mms_replies_actions_and_call_mute_over_authenticated_tls() {
    tokio::time::timeout(Duration::from_secs(30), exercise_tls_roundtrip())
        .await
        .expect("D-Bus/TLS round trip must finish");
}

async fn exercise_tls_roundtrip() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let phone = loopback_phone(&app).await;
    let peer = phone.manager.clone();
    let cert = phone.certificates.clone();
    let desktop = Desktop::default();
    let seen = desktop.seen.clone();
    let hints = desktop.hints.clone();
    let desktop_bus = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", desktop)
        .unwrap()
        .build()
        .await
        .unwrap();
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let conversation = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    let file = temp.path().join("voice.m4a");
    tokio::fs::write(&file, b"actual-audio-bytes")
        .await
        .unwrap();
    let address = OwnedValue::try_from(zbus::zvariant::Value::from(
        rust_connect::dbus::conversations::Address {
            address: "+15550000001".into(),
        },
    ))
    .unwrap();
    let path = OwnedValue::from(zbus::zvariant::Str::from(file.to_str().unwrap()));
    conversation
        .call::<_, _, ()>("sendWithoutConversation", &(vec![address], "", vec![path]))
        .await
        .unwrap();
    let packet = peer
        .recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
        .await
        .unwrap();
    assert_eq!(packet.packet_type, "kdeconnect.sms.request");
    assert_eq!(packet.body["addresses"][0]["address"], "+15550000001");
    assert_eq!(
        STANDARD
            .decode(
                packet.body["attachments"][0]["base64EncodedFile"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
        b"actual-audio-bytes"
    );
    // Original MMS data uses a separate, certificate-pinned payload channel.
    let mut received = conversation
        .receive_signal("attachmentReceived")
        .await
        .unwrap();
    let transfer = rust_connect::protocol::payload_transfer::PayloadTransfer::new(
        cert.clone(),
        "desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
    );
    let (mut payload, sent) = transfer
        .send_file(&file, "127.0.0.1".parse().unwrap())
        .await
        .unwrap();
    // Ignore a phone-supplied IP; fetch only from the authenticated main peer.
    payload.ip = Some("203.0.113.99".into());
    let identifier = "../../voice-note.m4a";
    conversation
        .call::<_, _, ()>("requestAttachmentFile", &(12i64, identifier))
        .await
        .unwrap();
    let request = peer
        .recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
        .await
        .unwrap();
    assert_eq!(request.body["unique_identifier"], identifier);
    app.plugins
        .sms
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.sms.attachment_file".into(),
                serde_json::json!({"filename":identifier}),
            )
            .with_payload_size(18)
            .with_payload_transfer_info(serde_json::to_value(payload).unwrap()),
        )
        .await
        .unwrap();
    let signal = tokio::time::timeout(Duration::from_secs(5), received.next())
        .await
        .unwrap()
        .unwrap();
    let (downloaded, original): (String, String) = signal.body().deserialize().unwrap();
    assert_eq!(original, identifier);
    assert_eq!(
        tokio::fs::read(&downloaded).await.unwrap(),
        b"actual-audio-bytes"
    );
    assert!(std::path::Path::new(&downloaded)
        .starts_with(app.settings.data_dir.join("sms-attachments")));
    assert!(!downloaded.contains(".."));
    sent.await.unwrap().unwrap();
    assert_ne!(
        app.plugins.sms.attachment_path("another-phone", identifier),
        app.plugins
            .sms
            .attachment_path("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", identifier)
    );
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/upstream-wire/sms/android_v2_batch.json"
    ))
    .unwrap();
    app.plugins
        .sms
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new("kdeconnect.sms.messages".into(), fixture),
        )
        .await
        .unwrap();
    conversation
        .call::<_, _, ()>(
            "replyToConversation",
            &(42i64, "reply", Vec::<OwnedValue>::new()),
        )
        .await
        .unwrap();
    let reply = peer
        .recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
        .await
        .unwrap();
    assert_eq!(reply.body["subID"], 3);
    assert_eq!(reply.body["addresses"][0]["address"], "+15550000001");
    if let Some(bridge) = std::env::var_os("MAINSTREAM_PHONE_BRIDGE") {
        check_python_bridge(&bus, &peer, temp.path(), bridge, &app).await;
    }
    let notification = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications",
        "org.kde.kdeconnect.device.notifications",
    )
    .await
    .unwrap();
    app.plugins.notification.handle_packet("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",Packet::new("kdeconnect.notification".into(),serde_json::json!({"id":"0|app|1","appName":"Chat","title":"Sender","text":"Message","actions":["Mark read"],"requestReplyId":"separate-reply-handle"}))).await.unwrap();
    until(|| async { seen.lock().unwrap().len() == 1 }).await;
    notification
        .call::<_, _, ()>("sendReply", &("0|app|1", "hello"))
        .await
        .unwrap();
    assert_eq!(
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
            .await
            .unwrap()
            .body,
        serde_json::json!({"requestReplyId":"separate-reply-handle","message":"hello"})
    );
    notification
        .call::<_, _, ()>("sendAction", &("0|app|1", "Mark read"))
        .await
        .unwrap();
    assert_eq!(
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
            .await
            .unwrap()
            .body,
        serde_json::json!({"key":"0|app|1","action":"Mark read"})
    );
    // Calls publish their notification object before the alert; cancellation must remove it.
    app.plugins.telephony.handle_packet("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",Packet::new("kdeconnect.telephony".into(),serde_json::json!({"event":"ringing","phoneNumber":"+15550000001","contactName":"Mom"}))).await.unwrap();
    until(|| async { seen.lock().unwrap().len() == 2 }).await;
    assert_eq!(seen.lock().unwrap()[1].2, vec!["Mute Call", "Mute Call"]);
    assert!(hints.lock().unwrap()[1].contains_key("x-kdeconnect-source-device"));
    desktop_bus
        .emit_signal(
            None::<&str>,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "ActionInvoked",
            &(2u32, "Mute Call"),
        )
        .await
        .unwrap();
    let muted = tokio::time::timeout(
        Duration::from_secs(3),
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(muted.packet_type, "kdeconnect.telephony.request_mute");
    app.plugins
        .telephony
        .handle_packet(
            "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Packet::new(
                "kdeconnect.telephony".into(),
                serde_json::json!({"event":"ringing","isCancel":"true"}),
            ),
        )
        .await
        .unwrap();
    until(|| async {
        notification
            .call::<_, _, Vec<String>>("activeNotifications", &())
            .await
            .unwrap()
            .len()
            == 1
    })
    .await;
    assert_eq!(seen.lock().unwrap().len(), 2);
    // Only explicit user dismissal propagates back; timeout leaves phone state intact.
    desktop_bus
        .emit_signal(
            None::<&str>,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "NotificationClosed",
            &(1u32, 1u32),
        )
        .await
        .unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
    )
    .await
    .is_err());
    // An inline reply typed into the desktop notification goes to the phone.
    desktop_bus
        .emit_signal(
            None::<&str>,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "NotificationReplied",
            &(1u32, "typed inline"),
        )
        .await
        .unwrap();
    let inline = tokio::time::timeout(
        Duration::from_secs(3),
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        inline.body,
        serde_json::json!({"requestReplyId":"separate-reply-handle","message":"typed inline"})
    );
    desktop_bus
        .emit_signal(
            None::<&str>,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "NotificationClosed",
            &(1u32, 2u32),
        )
        .await
        .unwrap();
    let cancel = tokio::time::timeout(
        Duration::from_secs(3),
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(cancel.body, serde_json::json!({"cancel":"0|app|1"}));
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_actions_survive_more_than_a_hundred_active_alerts() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let certificate_dir = tempfile::tempdir().unwrap();
    let certificates =
        rust_connect::protocol::CertificateManager::new(certificate_dir.path().into());
    let (pem, _) = certificates.generate_certificate(PHONE, "Phone").unwrap();
    pair(
        &app,
        openssl::x509::X509::from_pem(&pem)
            .unwrap()
            .to_der()
            .unwrap(),
    )
    .await;
    // Populate before bus startup as well, so service recovery is tested.
    for id in 0..150 {
        app.plugins.notification.handle_packet("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",Packet::new("kdeconnect.notification".into(),serde_json::json!({"id":format!("0|app|{id}"),"appName":"Chat","text":format!("Alert {id}"),"requestReplyId":format!("reply-{id}"),"silent":true}))).await.unwrap();
    }
    assert_eq!(app.plugins.notification.active_notifications().len(), 150);
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications",
        "org.kde.kdeconnect.device.notifications",
    )
    .await
    .unwrap();
    until(|| async {
        proxy
            .call::<_, _, Vec<String>>("activeNotifications", &())
            .await
            .unwrap()
            .len()
            == 150
    })
    .await;
    let id = rust_connect::utils::compat::component("0|app|0");
    let item = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        format!("/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications/{id}"),
        "org.kde.kdeconnect.device.notifications.notification",
    )
    .await
    .unwrap();
    assert_eq!(
        item.get_property::<String>("replyId").await.unwrap(),
        "reply-0"
    );
    service.stop().await;
}

#[test]
fn compatibility_installer_handles_spaces_and_preserves_unrelated_launchers() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("a user home");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let binary = bin.join("rust-connect");
    std::fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let install = || {
        Command::new("bash")
            .arg(root.join("packaging/install-shell-compat.sh"))
            .env("HOME", &home)
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    };
    assert!(install().status.success());
    let activation = std::fs::read_to_string(
        home.join(".local/share/dbus-1/services/org.kde.kdeconnect.service"),
    )
    .unwrap();
    assert!(activation.contains(&format!("Exec=\"{}\"", binary.display())));
    assert!(install().status.success());
    let launcher = Command::new(bin.join("kdeconnect-app"))
        .arg("--test-argument")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(launcher.stdout).unwrap(),
        "app\n--test-argument\n"
    );
    std::fs::write(bin.join("kdeconnectd"), "unrelated executable").unwrap();
    assert!(!install().status.success());
    assert_eq!(
        std::fs::read_to_string(bin.join("kdeconnectd")).unwrap(),
        "unrelated executable"
    );
}

async fn bridge_event(
    output: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    expected: &str,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_ne!(event["event"], "error", "{event}");
            // Availability and device snapshots can arrive independently of
            // command responses, including during initial bridge startup.
            if event["event"] == "backend" {
                assert_eq!(event["available"], true, "{event}");
            }
            if event["event"] == expected {
                return event;
            }
        }
    })
    .await
    .unwrap()
}

/// Optional cross-repository check: run the existing Gio bridge unchanged.
async fn check_python_bridge(
    bus: &Bus,
    peer: &rust_connect::protocol::ConnectionManager,
    directory: &std::path::Path,
    script: std::ffi::OsString,
    app: &AppState,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut child = tokio::process::Command::new("python3")
        .arg(script)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("HOME", directory)
        .env("XDG_CACHE_HOME", directory.join("cache"))
        .env("XDG_DATA_HOME", directory.join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"{\"cmd\":\"devices\"}\n").await.unwrap();
    let event = bridge_event(&mut output, "devices").await;
    assert_eq!(
        event["devices"][0]["id"],
        "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    input
        .write_all(b"{\"cmd\":\"conversations\",\"device\":\"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n")
        .await
        .unwrap();
    let event = bridge_event(&mut output, "conversations").await;
    assert_eq!(event["items"][0]["uid"], 75);
    assert_eq!(event["items"][0]["attachments"][0]["mime"], "audio/amr");
    assert_eq!(
        peer.recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
            .await
            .unwrap()
            .packet_type,
        "kdeconnect.sms.request_conversations"
    );
    input
        .write_all(
            b"{\"cmd\":\"thread\",\"device\":\"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"thread\":42}\n",
        )
        .await
        .unwrap();
    let request = peer
        .recv_packet(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
        .await
        .unwrap();
    assert_eq!(request.body["threadID"], 42);
    loop {
        let event: serde_json::Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(5), output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_ne!(event["event"], "error", "{event}");
        if event["event"] == "thread" && event["messages"].as_array().unwrap().len() == 2 {
            assert_eq!(event["messages"][0]["uid"], 74);
            break;
        }
    }
    input.write_all(b"{\"cmd\":\"attachment\",\"device\":\"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"part\":77,\"uid\":\"bridge-error\"}\n").await.unwrap();
    let request = peer.recv_packet(&DESKTOP.into()).await.unwrap();
    assert_eq!(request.body["unique_identifier"], "bridge-error");
    app.plugins
        .sms
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.sms.attachment_file".into(),
                serde_json::json!({"filename":"bridge-error"}),
            ),
        )
        .await
        .unwrap();
    loop {
        let event: serde_json::Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(5), output.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        if event["event"] == "error" {
            assert_eq!(event["context"], "attachment");
            assert!(event["message"]
                .as_str()
                .unwrap()
                .contains("attachment size"));
            break;
        }
    }
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

struct LoopbackPhone {
    manager: Arc<rust_connect::protocol::ConnectionManager>,
    certificates: Arc<rust_connect::protocol::CertificateManager>,
    _directory: tempfile::TempDir,
}
async fn loopback_phone(app: &Arc<AppState>) -> LoopbackPhone {
    use rust_connect::protocol::{CertificateManager, ConnectionManager};
    app.cert_manager
        .ensure_own_certificate("desktopbbbbbbbbbbbbbbbbbbbbbbbbb", "Desktop")
        .unwrap();
    app.connection_manager
        .set_device_identity("desktopbbbbbbbbbbbbbbbbbbbbbbbbb", "Desktop");
    let peer_dir = tempfile::tempdir().unwrap();
    let cert = Arc::new(CertificateManager::new(peer_dir.path().into()));
    cert.init().unwrap();
    cert.ensure_own_certificate("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Phone")
        .unwrap();
    let peer = Arc::new(ConnectionManager::new(cert.clone()).unwrap());
    peer.set_device_identity("phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Phone");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = peer.clone();
    let accept = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        server
            .accept_test("desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into(), socket)
            .await
            .unwrap();
    });
    app.connection_manager
        .connect(&"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(), address)
        .await
        .unwrap();
    accept.await.unwrap();
    let certificate = app
        .connection_manager
        .get_peer_certificate(&"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
        .await
        .unwrap();
    pair(app, certificate).await;
    cert.store_peer_certificate(
        "desktopbbbbbbbbbbbbbbbbbbbbbbbbb",
        &peer
            .get_peer_certificate(&"desktopbbbbbbbbbbbbbbbbbbbbbbbbb".into())
            .await
            .unwrap(),
    )
    .unwrap();

    LoopbackPhone {
        manager: peer,
        certificates: cert,
        _directory: peer_dir,
    }
}

const PHONE: &str = "phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DESKTOP: &str = "desktopbbbbbbbbbbbbbbbbbbbbbbbbb";

async fn respond_with_attachment(
    app: &AppState,
    phone: &LoopbackPhone,
    file: &std::path::Path,
    identifier: &str,
    declared_size: u64,
) {
    let transfer = rust_connect::protocol::payload_transfer::PayloadTransfer::new(
        phone.certificates.clone(),
        DESKTOP.into(),
    );
    let (info, sending) = transfer
        .send_file(file, "127.0.0.1".parse().unwrap())
        .await
        .unwrap();
    app.plugins
        .sms
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.sms.attachment_file".into(),
                serde_json::json!({"filename":identifier}),
            )
            .with_payload_size(declared_size)
            .with_payload_transfer_info(serde_json::to_value(info).unwrap()),
        )
        .await
        .unwrap();
    sending.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refreshing_an_attachment_replaces_a_bad_cached_copy() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let bus = Bus::new();
        let temp = tempfile::tempdir().unwrap();
        let app = state(temp.path()).await;
        let phone = loopback_phone(&app).await;
        let _service = DBusService::on_connection(
            app.clone(),
            bus.connect(Some("org.kde.kdeconnect")).await,
            temp.path().join("contacts"),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let client = bus.connect(None).await;
        let proxy = zbus::Proxy::new(
            &client,
            "org.kde.kdeconnect",
            "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "org.kde.kdeconnect.device.conversations",
        )
        .await
        .unwrap();
        let cached = app.plugins.sms.attachment_path(PHONE, "clip").unwrap();
        tokio::fs::create_dir_all(cached.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&cached, b"corrupt").await.unwrap();
        let mut received = proxy.receive_signal("attachmentReceived").await.unwrap();
        // The normal request hands back the cached copy without asking the phone.
        proxy
            .call::<_, _, ()>("requestAttachmentFile", &(3i64, "clip"))
            .await
            .unwrap();
        let signal = received.next().await.unwrap();
        let (path, _): (String, String) = signal.body().deserialize().unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"corrupt");
        assert!(tokio::time::timeout(
            Duration::from_millis(80),
            phone.manager.recv_packet(&DESKTOP.into())
        )
        .await
        .is_err());
        // A refresh drops the cached copy and asks the phone again.
        proxy
            .call::<_, _, ()>("refreshAttachmentFile", &(3i64, "clip"))
            .await
            .unwrap();
        let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
        assert_eq!(packet.packet_type, "kdeconnect.sms.request_attachment");
        assert_eq!(packet.body["unique_identifier"], "clip");
        assert!(!cached.exists());
        let file = temp.path().join("fresh.bin");
        tokio::fs::write(&file, b"good bytes").await.unwrap();
        respond_with_attachment(&app, &phone, &file, "clip", 10).await;
        let signal = received.next().await.unwrap();
        let (path, identifier): (String, String) = signal.body().deserialize().unwrap();
        assert_eq!(identifier, "clip");
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"good bytes");
    })
    .await
    .expect("refresh must replace the cached copy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_bursts_queue_before_requesting_and_failures_can_be_retried() {
    tokio::time::timeout(Duration::from_secs(20), exercise_attachment_queue())
        .await
        .expect("attachment queue must drain");
}
async fn exercise_attachment_queue() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let phone = loopback_phone(&app).await;
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    let mut failures = proxy.receive_signal("attachmentFailed").await.unwrap();
    for part in 0..6i64 {
        proxy
            .call::<_, _, ()>("requestAttachmentFile", &(part, format!("file-{part}")))
            .await
            .unwrap();
    }
    // A duplicate request shares one job rather than consuming another slot.
    proxy
        .call::<_, _, ()>("requestAttachmentFile", &(0i64, "file-0"))
        .await
        .unwrap();
    let mut requested = std::collections::VecDeque::new();
    for _ in 0..4 {
        let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
        assert_eq!(packet.packet_type, "kdeconnect.sms.request_attachment");
        requested.push_back(
            packet.body["unique_identifier"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(80),
            phone.manager.recv_packet(&DESKTOP.into())
        )
        .await
        .is_err(),
        "queued files must not open phone listeners prematurely"
    );
    assert_eq!(
        app.plugins.sms.attachment_state(PHONE, "file-5").0,
        "queued"
    );
    let file = temp.path().join("attachment.bin");
    tokio::fs::write(&file, b"test bytes").await.unwrap();
    let mut completed = std::collections::HashSet::new();
    for index in 0..6 {
        let identifier = requested.pop_front().unwrap();
        respond_with_attachment(&app, &phone, &file, &identifier, 10).await;
        until(|| async { app.plugins.sms.attachment_state(PHONE, &identifier).0 == "ready" }).await;
        assert!(completed.insert(identifier));
        if index < 2 {
            let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
            requested.push_back(
                packet.body["unique_identifier"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
    }
    assert_eq!(completed.len(), 6);
    // Truncated originals publish a real error, clean partials, then permit retry.
    proxy
        .call::<_, _, ()>("requestAttachmentFile", &(20i64, "retry"))
        .await
        .unwrap();
    phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
    respond_with_attachment(&app, &phone, &file, "retry", 20).await;
    let signal = tokio::time::timeout(Duration::from_secs(3), failures.next())
        .await
        .unwrap()
        .unwrap();
    let (identifier, error): (String, String) = signal.body().deserialize().unwrap();
    assert_eq!(identifier, "retry");
    assert!(error.to_lowercase().contains("truncated"));
    assert_eq!(app.plugins.sms.attachment_state(PHONE, "retry").0, "failed");
    assert!(!app
        .plugins
        .sms
        .attachment_path(PHONE, "retry")
        .unwrap()
        .exists());
    proxy
        .call::<_, _, ()>("requestAttachmentFile", &(20i64, "retry"))
        .await
        .unwrap();
    phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
    respond_with_attachment(&app, &phone, &file, "retry", 10).await;
    until(|| async { app.plugins.sms.attachment_state(PHONE, "retry").0 == "ready" }).await;
    // Pending memory is bounded, and disconnect cancels both active and queued jobs.
    for part in 0..64i64 {
        app.plugins
            .sms
            .request_attachment(PHONE, part, &format!("cancel-{part}"))
            .unwrap();
    }
    assert!(app
        .plugins
        .sms
        .request_attachment(PHONE, 65, "over-capacity")
        .is_err());
    app.plugins.sms.on_disconnected(PHONE).await;
    until(|| async {
        (0..64).all(|i| {
            app.plugins
                .sms
                .attachment_state(PHONE, &format!("cancel-{i}"))
                .0
                == "failed"
        })
    })
    .await;
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflowed_message_ids_report_errors_and_do_not_stop_other_threads() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    let mut errors = proxy.receive_signal("conversationError").await.unwrap();
    let mut updated = proxy.receive_signal("conversationUpdated").await.unwrap();
    let mut created = proxy.receive_signal("conversationCreated").await.unwrap();
    app.plugins
        .sms
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.sms.messages".into(),
                serde_json::json!({"messages":[
                    {"thread_id":42,"_id":i32::MAX as i64+1,"date":100,"body":"too large"},
                    {"thread_id":43,"_id":9,"date":101,"body":"valid"}
                ]}),
            ),
        )
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), errors.next())
        .await
        .unwrap()
        .unwrap();
    assert!(error
        .body()
        .deserialize::<(i64, String)>()
        .unwrap()
        .1
        .contains("32-bit"));
    let valid=tokio::time::timeout(Duration::from_secs(3),async {
        tokio::select! { message=updated.next()=>message.unwrap(), message=created.next()=>message.unwrap() }
    }).await.unwrap();
    let (value,): (OwnedValue,) = valid.body().deserialize().unwrap();
    assert_eq!(Message::try_from(value).unwrap().uid, 9);
    assert!(proxy
        .call::<_, _, Vec<OwnedValue>>("activeConversations", &())
        .await
        .is_err());
    assert!(proxy
        .call::<_, _, ()>("requestConversation", &(42i64, 0i32, 50i32))
        .await
        .is_err());
    assert_eq!(
        app.plugins.sms.get_thread(PHONE, 42)[0].uid,
        Some(i32::MAX as i64 + 1)
    );
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_backlog_recovers_threads_cancellations_battery_and_attachment_errors() {
    tokio::time::timeout(Duration::from_secs(15), exercise_backlog_recovery())
        .await
        .expect("all missed state must recover");
}
async fn exercise_backlog_recovery() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let phone = loopback_phone(&app).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let desktop = Desktop {
        gate: Some(gate.clone()),
        ..Default::default()
    };
    let entered = desktop.entered.clone();
    let seen = desktop.seen.clone();
    let _desktop_bus = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", desktop)
        .unwrap()
        .build()
        .await
        .unwrap();
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    proxy
        .call::<_, _, ()>("requestConversation", &(42i64, 0i32, 50i32))
        .await
        .unwrap();
    phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
    let mut updated = proxy.receive_signal("conversationUpdated").await.unwrap();
    let mut created = proxy.receive_signal("conversationCreated").await.unwrap();
    let mut loaded = proxy.receive_signal("conversationLoaded").await.unwrap();
    let mut failed = proxy.receive_signal("attachmentFailed").await.unwrap();
    let telephony = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/telephony",
        "org.kde.kdeconnect.device.telephony",
    )
    .await
    .unwrap();
    let mut calls = telephony.receive_signal("callReceived").await.unwrap();
    let battery = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/battery",
        "org.kde.kdeconnect.device.battery",
    )
    .await
    .unwrap();
    let mut refreshed = battery.receive_signal("refreshed").await.unwrap();
    app.plugins
        .notification
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.notification".into(),
                serde_json::json!({"id":"blocked","appName":"Test","text":"hold event forwarder"}),
            ),
        )
        .await
        .unwrap();
    until(|| async { entered.load(Ordering::Relaxed) > 0 }).await;
    app.plugins
        .sms
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.sms.messages".into(),
                serde_json::json!({"messages":[
                    {"thread_id":42,"_id":1,"date":100,"body":"one"},
                    {"thread_id":42,"_id":2,"date":101,"body":"two"},
                    {"thread_id":42,"_id":3,"date":102,"body":"three"}
                ]}),
            ),
        )
        .await
        .unwrap();
    for cancel in [false, true] {
        app.plugins.telephony.handle_packet(PHONE,Packet::new("kdeconnect.telephony".into(),serde_json::json!({"event":"ringing","contactName":"Mom","phoneNumber":"+15550000001","isCancel":cancel}))).await.unwrap();
    }
    app.plugins
        .battery
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.battery".into(),
                serde_json::json!({"currentCharge":55,"isCharging":true,"thresholdEvent":0}),
            ),
        )
        .await
        .unwrap();
    app.plugins
        .sms
        .request_attachment(PHONE, 1, "lost-error")
        .unwrap();
    phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
    app.plugins
        .sms
        .handle_packet(
            PHONE,
            Packet::new(
                "kdeconnect.sms.attachment_file".into(),
                serde_json::json!({"filename":"lost-error"}),
            ),
        )
        .await
        .unwrap();
    until(|| async { app.plugins.sms.attachment_state(PHONE, "lost-error").0 == "failed" }).await;
    for _ in 0..400 {
        app.plugin_events
            .broadcast(rust_connect::plugins::PluginEvent::ClipboardUpdate {
                device_id: PHONE.into(),
                content: String::new(),
            });
    }
    gate.add_permits(1);
    let signal = tokio::time::timeout(Duration::from_secs(4), loaded.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(signal.body().deserialize::<(i64, u64)>().unwrap(), (42, 3));
    let mut identities = std::collections::HashSet::new();
    while identities.len() < 3 {
        let message = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! { message=updated.next()=>message.unwrap(), message=created.next()=>message.unwrap() }
        }).await.unwrap();
        let (value,): (OwnedValue,) = message.body().deserialize().unwrap();
        identities.insert(Message::try_from(value).unwrap().uid);
    }
    let call = tokio::time::timeout(Duration::from_secs(3), calls.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        call.body()
            .deserialize::<(String, String, String)>()
            .unwrap()
            .2,
        "Mom"
    );
    let error = tokio::time::timeout(Duration::from_secs(3), failed.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        error.body().deserialize::<(String, String)>().unwrap().0,
        "lost-error"
    );
    let charge = tokio::time::timeout(Duration::from_secs(3), refreshed.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        charge.body().deserialize::<(bool, i32)>().unwrap(),
        (true, 55)
    );
    let notifications = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications",
        "org.kde.kdeconnect.device.notifications",
    )
    .await
    .unwrap();
    let active: Vec<String> = notifications
        .call("activeNotifications", &())
        .await
        .unwrap();
    assert_eq!(active, vec!["blocked"]);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "an ended call must not ring again during recovery"
    );
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_icons_follow_exact_updates_and_late_cache_availability() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data with # and %");
    let app = state(&data).await;
    let desktop = Desktop::default();
    let seen = desktop.seen.clone();
    let hints = desktop.hints.clone();
    let icons = desktop.icons.clone();
    let _desktop_bus = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", desktop)
        .unwrap()
        .build()
        .await
        .unwrap();
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let cache = data.join("notification-icons").join(PHONE);
    std::fs::create_dir_all(&cache).unwrap();
    let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    for hash in [hash_a, hash_b] {
        std::fs::write(cache.join(hash), b"fixture icon bytes").unwrap();
    }
    let send = |id: &str, name: &str, hash: Option<&str>| {
        let packet = Packet::new(
            "kdeconnect.notification".into(),
            serde_json::json!({
                "id":id,"appName":name,"title":"Steam sale in a Gmail subject","text":"Fixture only","payloadHash":hash
            }),
        );
        let plugin = app.clone();
        async move {
            plugin
                .plugins
                .notification
                .handle_packet(PHONE, packet)
                .await
                .unwrap();
        }
    };
    // Body artwork stays separate from the header app icon for each notification.
    send("mail", "Gmail", Some(hash_a)).await;
    until(|| async { seen.lock().unwrap().len() == 1 }).await;
    send("game", "Steam", Some(hash_b)).await;
    until(|| async { seen.lock().unwrap().len() == 2 }).await;
    for (index, hash) in [hash_a, hash_b].into_iter().enumerate() {
        let expected = rust_connect::utils::compat::file_uri(&cache.join(hash));
        assert_eq!(icons.lock().unwrap()[index], "");
        let guard = hints.lock().unwrap();
        assert_eq!(
            <&str>::try_from(&guard[index]["image-path"]).unwrap(),
            expected
        );
        assert_eq!(
            <&str>::try_from(&guard[index]["x-mainstream-image-role"]).unwrap(),
            "notification-image"
        );
        assert_eq!(
            <&str>::try_from(&guard[index]["x-mainstream-icon-hash"]).unwrap(),
            hash
        );
        assert!(expected.contains("%23") && expected.contains("%25") && expected.contains("%20"));
    }
    // A replacement with a different icon must publish despite identical text.
    send("mail", "Gmail", Some(hash_b)).await;
    until(|| async { seen.lock().unwrap().len() == 3 }).await;
    {
        let guard = icons.lock().unwrap();
        assert_eq!(guard[2], guard[1]);
    }
    {
        let guard = hints.lock().unwrap();
        assert_eq!(
            <&str>::try_from(&guard[2]["image-path"]).unwrap(),
            rust_connect::utils::compat::file_uri(&cache.join(hash_b))
        );
        let first_key = <&str>::try_from(&guard[0]["x-mainstream-notification-key"]).unwrap();
        assert_ne!(
            first_key,
            <&str>::try_from(&guard[1]["x-mainstream-notification-key"]).unwrap()
        );
        assert_eq!(
            first_key,
            <&str>::try_from(&guard[2]["x-mainstream-notification-key"]).unwrap()
        );
    }
    // Explicitly missing artwork clears the previous icon, rather than inheriting it.
    send("mail", "Gmail", None).await;
    until(|| async { seen.lock().unwrap().len() == 4 }).await;
    assert_eq!(icons.lock().unwrap()[3], "");
    assert!(!hints.lock().unwrap()[3].contains_key("image-path"));
    let late = "cccccccccccccccccccccccccccccccc";
    send("mail", "Gmail", Some(late)).await;
    until(|| async { seen.lock().unwrap().len() == 5 }).await;
    assert_eq!(icons.lock().unwrap()[4], "");
    {
        let guard = hints.lock().unwrap();
        assert!(!guard[4].contains_key("image-path"));
        assert_eq!(
            <&str>::try_from(&guard[4]["x-mainstream-icon-hash"]).unwrap(),
            late
        );
    }
    std::fs::write(cache.join(late), b"late fixture icon").unwrap();
    send("mail", "Gmail", Some(late)).await;
    until(|| async { seen.lock().unwrap().len() == 6 }).await;
    assert_eq!(icons.lock().unwrap()[5], "");
    assert_eq!(
        <&str>::try_from(&hints.lock().unwrap()[5]["image-path"]).unwrap(),
        rust_connect::utils::compat::file_uri(&cache.join(late))
    );
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notifications_dismissed_while_the_phone_was_away_close_after_it_reconnects() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let _phone = loopback_phone(&app).await;
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa/notifications",
        "org.kde.kdeconnect.device.notifications",
    )
    .await
    .unwrap();
    let post = |id: &str| {
        Packet::new(
            "kdeconnect.notification".into(),
            serde_json::json!({"id":id,"appName":"Mail","text":id}),
        )
    };
    let active = || async {
        proxy
            .call::<_, _, Vec<String>>("activeNotifications", &())
            .await
            .unwrap()
            .len()
    };
    for id in ["kept", "dismissed"] {
        app.plugins
            .notification
            .handle_packet(PHONE, post(id))
            .await
            .unwrap();
    }
    until(|| async { active().await == 2 }).await;
    // Back online, the phone sends again only what it still shows.
    app.plugins.notification.on_disconnected(PHONE).await;
    app.broadcaster
        .broadcast(rust_connect::device::DeviceEvent::StateChanged {
            device_id: PHONE.into(),
            old_state: rust_connect::device::DeviceState::Disconnected,
            new_state: rust_connect::device::DeviceState::Connected,
        });
    app.plugins
        .notification
        .handle_packet(PHONE, post("kept"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while active().await != 1 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    service.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnect_burst_does_not_report_conversations_as_removed() {
    let bus = Bus::new();
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let service = DBusService::on_connection(
        app.clone(),
        bus.connect(Some("org.kde.kdeconnect")).await,
        temp.path().join("contacts"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = bus.connect(None).await;
    let proxy = zbus::Proxy::new(
        &client,
        "org.kde.kdeconnect",
        "/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "org.kde.kdeconnect.device.conversations",
    )
    .await
    .unwrap();
    let mut removed = proxy.receive_signal("conversationRemoved").await.unwrap();
    let mut loaded = proxy.receive_signal("conversationLoaded").await.unwrap();
    let batch = |thread: i64| {
        Packet::new(
            "kdeconnect.sms.messages".into(),
            serde_json::json!({"messages":[{"thread_id":thread,"_id":thread,"date":100 + thread,"body":"hello"}]}),
        )
    };
    // Android sends a conversation list one conversation per packet.
    for thread in 1..=3 {
        app.plugins
            .sms
            .handle_packet(PHONE, batch(thread))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), loaded.next())
            .await
            .unwrap()
            .unwrap();
    }
    // A reconnect clears the phone's threads, and the first packet back holds one.
    app.plugins.sms.on_disconnected(PHONE).await;
    app.plugins
        .sms
        .handle_packet(PHONE, batch(2))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), loaded.next())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), removed.next())
            .await
            .is_err(),
        "threads that have not arrived again yet must not be reported removed"
    );
    service.stop().await;
}

#[tokio::test]
async fn cached_attachments_keep_readable_names_and_old_copies_are_still_found() {
    let temp = tempfile::tempdir().unwrap();
    let app = state(temp.path()).await;
    let sms = &app.plugins.sms;
    sms.handle_packet(
        PHONE,
        Packet::new(
            "kdeconnect.sms.messages".into(),
            serde_json::json!({"messages":[{"thread_id":7,"_id":1,"date":1,"body":"","attachments":[
                {"part_id":5,"mime_type":"image/jpeg","unique_identifier":"PART_1700000000000","encoded_thumbnail":""}
            ]}]}),
        ),
    )
    .await
    .unwrap();
    let path = sms.attachment_path(PHONE, "PART_1700000000000").unwrap();
    assert_eq!(path.file_name().unwrap(), "PART_1700000000000.jpg");
    // Identifiers that read the same still get their own copies.
    let lookalike = sms
        .attachment_path(PHONE, "PART_1700000000000.jpg")
        .unwrap();
    assert_eq!(lookalike.file_name(), path.file_name());
    assert_ne!(lookalike, path);
    let slot = path.parent().unwrap();
    let directory = slot.parent().unwrap();
    std::fs::create_dir_all(slot).unwrap();
    // Copies cached under the old digest names are still served.
    let legacy = directory.join(rust_connect::plugins::sms::attachment_filename("PART_old"));
    std::fs::write(&legacy, b"old").unwrap();
    assert_eq!(sms.attachment_path(PHONE, "PART_old").unwrap(), legacy);
    // A copy cached with its extension is still found once a reconnect has
    // cleared the message that gave its type.
    std::fs::write(&path, b"new").unwrap();
    sms.on_disconnected(PHONE).await;
    assert_eq!(
        sms.attachment_path(PHONE, "PART_1700000000000").unwrap(),
        path
    );
}

async fn next_pair_state(states: &mut zbus::proxy::SignalStream<'_>) -> i32 {
    let signal = tokio::time::timeout(Duration::from_secs(3), states.next())
        .await
        .unwrap()
        .unwrap();
    signal.body().deserialize::<(i32,)>().unwrap().0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_asking_to_pair_can_be_accepted_or_declined_over_dbus() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let bus = Bus::new();
        let temp = tempfile::tempdir().unwrap();
        let app = state(temp.path()).await;
        let phone = loopback_phone(&app).await;
        let _service = DBusService::on_connection(
            app.clone(),
            bus.connect(Some("org.kde.kdeconnect")).await,
            temp.path().join("contacts"),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let client = bus.connect(None).await;
        // Uncached, so each read sees the daemon's current answer.
        let device: zbus::Proxy = zbus::proxy::Builder::new(&client)
            .destination("org.kde.kdeconnect")
            .unwrap()
            .path("/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap()
            .interface("org.kde.kdeconnect.device")
            .unwrap()
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        // Cached, as most clients are, so it relies on PropertiesChanged.
        let cached: zbus::Proxy = zbus::proxy::Builder::new(&client)
            .destination("org.kde.kdeconnect")
            .unwrap()
            .path("/modules/kdeconnect/devices/phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap()
            .interface("org.kde.kdeconnect.device")
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut requested = cached
            .receive_property_changed::<bool>("isPairRequestedByPeer")
            .await;
        let mut states = device.receive_signal("pairStateChanged").await.unwrap();
        let ask = || async {
            let certificate = app
                .connection_manager
                .get_peer_certificate(&PHONE.into())
                .await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            app.pairing_handler
                .receive_pair_request_with_cert(&PHONE.into(), Some(now), certificate)
                .await
                .unwrap();
            app.broadcaster
                .broadcast(rust_connect::device::DeviceEvent::PairRequested {
                    device_id: PHONE.into(),
                    device_name: "Phone".into(),
                });
        };
        app.pairing_handler.unpair(&PHONE.into()).await.unwrap();
        while next_pair_state(&mut states).await != 0 {}

        // A request from the phone is visible with the code both screens show.
        ask().await;
        assert_eq!(next_pair_state(&mut states).await, 2);
        assert_eq!(device.get_property::<i32>("pairState").await.unwrap(), 2);
        assert!(device
            .get_property::<bool>("isPairRequestedByPeer")
            .await
            .unwrap());
        let key = device
            .get_property::<String>("verificationKey")
            .await
            .unwrap();
        assert_eq!(key.len(), 8, "verification key {key:?}");
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(change) = requested.next().await {
                if change.get().await.unwrap() {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            cached
                .cached_property::<String>("verificationKey")
                .unwrap()
                .unwrap(),
            key
        );
        device.call::<_, _, ()>("acceptPairing", &()).await.unwrap();
        let answer = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
        assert_eq!(answer.packet_type, "kdeconnect.pair");
        assert_eq!(answer.body["pair"], true);
        assert!(device.get_property::<bool>("isPaired").await.unwrap());
        assert_eq!(next_pair_state(&mut states).await, 3);

        // Declining a later request tells the phone no and clears it.
        app.pairing_handler.unpair(&PHONE.into()).await.unwrap();
        while next_pair_state(&mut states).await != 0 {}
        ask().await;
        assert_eq!(next_pair_state(&mut states).await, 2);
        device.call::<_, _, ()>("cancelPairing", &()).await.unwrap();
        loop {
            let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
            if packet.packet_type == "kdeconnect.pair" {
                assert_eq!(packet.body["pair"], false);
                break;
            }
        }
        assert_eq!(next_pair_state(&mut states).await, 0);
        assert!(!device
            .get_property::<bool>("isPairRequestedByPeer")
            .await
            .unwrap());
        assert!(device.call::<_, _, ()>("acceptPairing", &()).await.is_err());

        // Pairing started here asks the phone and shows as requested.
        device.call::<_, _, ()>("requestPair", &()).await.unwrap();
        loop {
            let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
            if packet.packet_type == "kdeconnect.pair" {
                assert_eq!(packet.body["pair"], true);
                break;
            }
        }
        assert_eq!(next_pair_state(&mut states).await, 1);
        device.call::<_, _, ()>("cancelPairing", &()).await.unwrap();
        loop {
            let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
            if packet.packet_type == "kdeconnect.pair" {
                assert_eq!(packet.body["pair"], false);
                break;
            }
        }
        assert_eq!(next_pair_state(&mut states).await, 0);

        // The Mainstream shell's bridge reports the request and answers it.
        if let Some(script) = std::env::var_os("MAINSTREAM_PHONE_BRIDGE") {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            ask().await;
            assert_eq!(next_pair_state(&mut states).await, 2);
            let mut child = tokio::process::Command::new("python3")
                .arg(script)
                .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
                .env("HOME", temp.path())
                .env("XDG_CACHE_HOME", temp.path().join("cache"))
                .env("XDG_DATA_HOME", temp.path().join("data"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
            input.write_all(b"{\"cmd\":\"devices\"}\n").await.unwrap();
            let event = bridge_event(&mut output, "pairRequests").await;
            assert_eq!(event["requests"][0]["id"], PHONE);
            assert_eq!(event["requests"][0]["key"].as_str().unwrap().len(), 8);
            input
                .write_all(b"{\"cmd\":\"acceptPairing\",\"device\":\"phoneaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n")
                .await
                .unwrap();
            loop {
                let packet = phone.manager.recv_packet(&DESKTOP.into()).await.unwrap();
                if packet.packet_type == "kdeconnect.pair" {
                    assert_eq!(packet.body["pair"], true);
                    break;
                }
            }
            let event = bridge_event(&mut output, "pairRequests").await;
            assert!(event["requests"].as_array().unwrap().is_empty());
        }
    })
    .await
    .unwrap();
}
