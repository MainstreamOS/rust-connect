# rust-connect replacement requirements for Mainstream’s Dynamic Island and Phone tab

Independent requirements/specification — 18 October 2026

This is the pre-implementation audit. Section 9 records the source examined at that time, not the current implementation. See [the implementation and validation guide](mainstream-shell-compatibility.md) for current behavior and remaining live-device checks.

## 1. Scope and evidence

Target shell: `dots-hyprland`, `mainstream`, commit `c88cbb2c5516a731a6069ca57a2c1248ebb14769`.

This specification defines the desktop backend needed to preserve the phone-related features presently exposed by that shell. It does not assume that rust-connect’s implementation, its “drop-in replacement” description, or its existing tests establish compatibility. Implementation observations appear separately in section 9.

The baseline includes all Dynamic Island panels, the Phone tab and its attachment/media components, `Phone.qml`, `ContactService.qml`, `IslandState.qml`, phone history, notification policy/actions/image caching, both Python phone bridges, and the files/media helpers those components call. Local panels were checked for indirect phone dependencies as well as explicit KDE Connect references.

Primary protocol sources were independently inspected at these upstream revisions:

- KDE Connect Android: `f6a699de894d1bad83bfbcc61e5e757a54788051`.
- KDE Connect desktop: `b180447d2afe0b9f6bc70e812ce214763201edc0`.

The proposed replacement is the **desktop** KDE Connect daemon and KDE GUI dependencies. A compatible companion app can remain on the phone. Replacing the Android/iOS companion app is a separate project.

Local source entry points:

- [Phone service](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/services/Phone.qml)
- [Phone bridge](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/scripts/phone/kdeconnect-bridge.py)
- [Island state](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/services/IslandState.qml)
- [Island phone bridge](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/scripts/island/phone.py)
- [Notification service](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/services/Notifications.qml)
- [Notification action bridge](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/scripts/phone/kdeconnect-action.py)
- [Shared contact registry](https://github.com/MainstreamOS/dots-hyprland/blob/c88cbb2c5516a731a6069ca57a2c1248ebb14769/dots/.config/quickshell/ii/scripts/phone/contacts.py)

## 2. Recommended integration boundary

Use a lightweight, KDE-compatible **session D-Bus service** for the first complete replacement. Retaining the name `org.kde.kdeconnect` and compatible interface names does not require linking KDE Frameworks or Qt. It minimizes changes to the working shell UI.

One desktop backend must own that bus name. The replacement must provide network discovery, authentication/pairing, capability negotiation, reconnects and authenticated payload transfer, then expose the API in section 5. It must start in the desktop user’s session and remain reachable after shell reloads.

A later native rust-connect API is possible, but would require migrating every consumer listed above. A REST API by itself does not satisfy the current shell integration.

Separate transport from presentation: rust-connect supplies device state, messages, attachments, contacts, notifications, call events and file access. QML continues to provide layouts, animation, privacy settings, history presentation and local media playback.

## 3. Complete user-visible feature inventory

“Backend” means rust-connect must provide equivalent data/transport. “Shell” means the existing UI/helper can remain once its underlying transport works.

| ID | Feature to preserve | Required backend behavior | What remains in the shell |
|---|---|---|---|
| DEV-01 | Phone detection and connection indicator | Discover, pair, retain identity, enumerate paired/reachable devices, publish name/type and connection changes | Active-device selection logic, empty/offline UI, panel visibility |
| DEV-02 | Phone battery in Phone tab and island | Percentage, charging state, explicit unavailable state and updates | Battery cards and duplicate-device filtering |
| CON-01 | Names and circular contact pictures | Synchronize complete vCards, multiple numbers, names and embedded photos; report refresh/deletion | Shared identity matching, avatar rendering and privacy controls |
| MSG-01 | Conversation list and latest-message previews | Enumerate threads with stable IDs, participants, latest timestamps, direction/read state and attachment metadata | Sorting/presentation and preview formatting |
| MSG-02 | Open and scroll conversation history | Fetch bounded ranges, replay cached messages, deliver load completion and new-message updates | Thread cache, scrolling and loading UI |
| MSG-03 | New SMS and replies | Send to one or multiple recipients; retain destination and SIM context where available; report request failure | Composer, optimistic copies and reconciliation |
| MSG-04 | Group conversations | Preserve complete recipient lists without duplicate numbers or confusing the sender with the whole group | Group titles and recipient display |
| MMS-01 | Send images, video, GIFs, audio and arbitrary supported attachments | Read supplied files and build valid outgoing MMS attachment containers; support attachment-only messages | File picker, drag/drop, conversion/compression and progress/error presentation |
| MMS-02 | Receive pictures, videos and audio messages | Preserve MIME, part ID, thumbnail and unique attachment ID; download the original on request | Image layout, fullscreen viewers, video/audio playback, seek controls and save buttons |
| MMS-03 | Voice notes recorded on the computer | Send the recorded audio file as an MMS attachment | Microphone recording and encoding |
| MSG-05 | Reactions and quoted replies | Reliably transport the resulting text and original message body | Current reaction parsing, hold-to-react UI, quote formatting and jump-to-message logic |
| MSG-06 | Rich links and inline online media | Transport ordinary message text and URLs unchanged | Link metadata, Tenor search, stream extraction, downloading and playback |
| ISL-01 | Island recent calls/messages | Stable event identity, contact data and live-versus-historical distinction | Persistent logs, deduplication, formatting and clearing |
| ISL-02 | Island quick reply and opening the full thread | Same SMS send/history services as Phone tab; retain device/address/thread identity | Reply input and navigation to the correct Phone-tab conversation |
| NOT-01 | Mirrored phone notifications | App/title/body, stable identity, image payloads, posting/updating/cancelling, reconnect deduplication | Notification history, grouping, image layout and privacy |
| NOT-02 | Notification buttons | Preserve action labels and send the selected action to its originating device/notification | Existing filtering of unreliable foreground actions and button display |
| NOT-03 | Inline notification replies | Preserve the phone’s reply handle, independently of notification ID, and send the entered text | Inline reply composer and saved metadata |
| NOT-04 | Notification dismissal | Define and preserve local dismissal; when synchronizing dismissal to the phone, distinguish explicit user dismissal from expiration | Clear/dismiss UI |
| CALL-01 | Incoming/missed-call alerts, number and photo | Forward telephony metadata, ringing/talking/cancellation lifecycle and mute-ringer action | Call panel, identity enrichment and recent-call log |
| CALL-02 | Accept, decline and end call | Preserve the separate Bluetooth HFP control path described in section 4 | Existing PipeWire/oFono adapter and call buttons |
| FILE-01 | Send files directly to phone | Authenticated file-share/payload transport, multiple files and failures | Drag/drop, file picker and confirmation UI |
| FILE-02 | Browse phone storage | Request SFTP access and expose mounted, readable local paths with named storage roots and mount errors | List/grid browsing, quick places, sorting, image/video viewers |
| FILE-03 | Open/save remote files and show thumbnails | Keep mounted files accessible and provide stable local attachment paths | Local copy/open operations, thumbnail generation, media metadata and save-location cards |
| UI-01 | Automatic island alerts and duplicate suppression | Identify phone origin and event type reliably, without duplicate representations on reconnect | Calls/texts take priority, DND, privacy, fullscreen hiding, hover expansion and sidebar controls |

MSG-05 does not require a new native RCS/iMessage reaction protocol: the current shell sends reaction/quote text through the ordinary message path. It does not establish native RCS or iMessage support.

## 4. Important boundaries: calls, iPhone and local panels

### Call notification is different from call control

The shell reads KDE-compatible telephony metadata for caller identity. Its **Answer/Decline/End** operations use `org.pipewire.Telephony.Call1` or `org.ofono.VoiceCall` over Bluetooth HFP. The network telephony plugin’s mute action silences ringing; it is not Answer.

Preserve those existing Bluetooth adapters and the device-profile connection fix. A successful Wi-Fi pairing alone must never cause the UI to advertise working Answer/Decline buttons. Expose explicit capabilities for ringing alerts, mute, answer and hangup. Upstream desktop KDE Connect also implements mute as a distinct telephony request. [Upstream call implementation](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/plugins/telephony/telephonyplugin.cpp).

The island currently uses a KDE-reachable-phone condition when adding its call/message panels. If Bluetooth-only devices are meant to work independently, replace that condition with backend-neutral capabilities rather than merely renaming it.

### iPhone

The island’s separate BlueZ ANCS adapter reads iPhone notifications and supported positive/negative actions. Keep it, together with Bluetooth call control. This code is not a full iPhone conversation-history/SMS API. Full iPhone Phone-tab parity must not be claimed from ANCS notification support alone; any additional companion-app capabilities need their own acceptance tests.

### Features requiring no new phone backend plugin

The island’s clock, weather, calendar/tasks, timers, computer RAM/CPU/GPU/battery, network toggles, night light, volume, brightness/gamma, tray and desktop utility buttons are local services. Its LocalSend send/receive panel uses LocalSend, independently of KDE Connect’s direct-to-phone file sharing.

Local PC media players use standard MPRIS and continue to work without KDE Connect. If phone-origin remote MPRIS players are also a desired use case, specify and test a remote-player MPRIS exporter separately; local Spotify/mpv/browser controls do not require it.

These screens do not themselves require KDE Connect clipboard synchronization, remote mouse/keyboard, presenter, run-command or find-my-phone plugins. Keeping such features elsewhere in the OS is a separate scope decision.

## 5. D-Bus contract consumed by the shell

Service: `org.kde.kdeconnect`, session bus. Use real D-Bus errors for unavailable devices, rejected requests and failed sends; returning success after discarding an error is unacceptable.

### Daemon and device

- `/modules/kdeconnect`, `org.kde.kdeconnect.daemon`: `devices(onlyReachable, onlyPaired)`; changes through `deviceAdded`, `deviceRemoved`, `deviceVisibilityChanged`, `deviceListChanged`.
- `/modules/kdeconnect/devices/{device}`, `org.kde.kdeconnect.device`: properties `name`, `type`, `isReachable`; `loadedPlugins()`; coherent connection/capability change notifications.
- Pairing must also be operable from the replacement’s own CLI/UI or shell integration, even though the current Phone-tab empty state launches an external KDE pairing GUI.
- Report actual available/negotiated plugins, not an unconditional list of every implemented feature.

### Conversations

On the device root, interface `org.kde.kdeconnect.device.conversations`:

| Method | Inputs / expected behavior |
|---|---|
| `activeConversations()` | Variant array containing the latest known message in each thread |
| `requestAllConversationThreads()` | Start initial synchronization and enable subsequent live message updates |
| `requestConversation(thread, start, end)` | Supply the requested message-index range from cache and/or phone; this is not a direct timestamp range |
| `sendWithoutConversation(addresses, text, attachments)` | Address variants containing address structs; file-path variants for outgoing attachments |
| `replyToConversation(thread, text, attachments)` | Recover recipients and SIM context for an existing thread |
| `requestAttachmentFile(part, uniqueId)` | Return/download the exact attachment and announce its local path |

Required event surface: `conversationCreated`, `conversationUpdated`, `conversationLoaded`, `conversationRemoved`, `attachmentReceived(localPath, uniqueId)`. Emit creation/update/completion events when their conditions occur; expose removal when known rather than inventing a remote-delete capability. Preserve variant wrapping and actual KDE wire signatures where drop-in compatibility is claimed. A history request must complete for empty, fully cached and repeated batches too; do not leave the UI waiting for a signal that is only declared.

Message values must preserve event, body, participants, date, type/direction, read state, thread ID, message ID, SIM ID and attachment tuples. The fallback `/sms` interface must provide `requestAllConversations()` for the island bridge. [Upstream conversation API and lifecycle](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/plugins/sms/conversationsdbusinterface.h), [implementation](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/plugins/sms/conversationsdbusinterface.cpp).

### Battery, contacts and calls

- `{device}/battery`, `org.kde.kdeconnect.device.battery`: properties `charge`, `isCharging`, `hasBattery`; event `refreshed`.
- `{device}/contacts`, `org.kde.kdeconnect.device.contacts`: `synchronizeRemoteWithLocal()`; event `localCacheSynchronized` only after usable contact data is available.
- `{device}/telephony`, `org.kde.kdeconnect.device.telephony`: `callReceived(event, number, contactName)`, plus the notification/lifecycle behavior described above. Do not turn call cancellation into another ringing event.

### Files

- `{device}/share`, `org.kde.kdeconnect.device.share`: `shareUrls(urls)` for direct file sending.
- `{device}/sftp`, `org.kde.kdeconnect.device.sftp`: `mountAndWait()`, `isMounted()`, `mountPoint()`, `getDirectories()`, `getMountError()`; `mounted` and `unmounted` signals.
- Returned directory roots must resolve to actual local paths under the mounted filesystem. Test with a phone advertising `/storage/emulated/0`, not only `/`.

### Notifications

- `{device}/notifications`, `org.kde.kdeconnect.device.notifications`: `activeNotifications()`, `sendAction(internalId, action)`, `sendReply(idOrHandle, text)`.
- Child objects, `org.kde.kdeconnect.device.notifications.notification`: properties `internalId`, `replyId`, `appName`, `title`, `text`, `ticker`, `dismissable`; methods `sendReply(text)` and `dismiss()`.
- Use legal, stable D-Bus object-path keys and a separate mapping to Android’s raw notification IDs. IDs containing `|` or `.` cannot be pasted directly into object paths.
- Preserve phone-origin/application hints in freedesktop notifications: `x-kdeconnect-source-device`, `x-kde-appname`, `x-kde-display-appname`, and usable desktop-entry metadata. For telephony classification preserve the event hint or provide an equivalent explicit route.
- Keep real action labels, the distinct reply handle and image data. Publish usable objects/state before notifying the shell that a notification exists.

The shell uses freedesktop notifications for presentation and the KDE-compatible notification objects for reliable reply/action lookup. Implementing just one half loses functionality. [Phone notification actions and reply handling](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/plugins/notifications/NotificationsPlugin.kt).

## 6. Network packet requirements and state semantics

The required plugin families are SMS/MMS, notifications, contacts, telephony, battery, share and SFTP, on top of compatible discovery/pairing/encrypted transport. Packet field names are case-sensitive and are not interchangeable with D-Bus field names.

### Messaging compatibility details

- Incoming current message identity uses `thread_id`, `_id`, `sub_id`. Attachment metadata uses `part_id`, `mime_type`, `encoded_thumbnail`, `unique_identifier`. Preserve them before converting to shell-facing values. [Actual Android serialization](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/helpers/SMSHelper.kt).
- Outgoing messages use the version-2 address-list format. Outgoing attachments require file name, encoded file data and MIME; forwarding a path alone is insufficient. SIM selection must follow the receiver’s implemented `subID` behavior rather than relying on a conflicting comment. [Actual Android request handling](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/plugins/sms/SMSPlugin.kt).
- Conversation fetches use `threadID`, `rangeStartTimestamp`, `numberToRequest`; attachment fetches use `part_id` and `unique_identifier`; attachment responses identify the file through `filename`. The D-Bus message-index range needs an adapter to this network API. [Desktop packet construction and attachment reception](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/plugins/sms/smsplugin.cpp).

Other required wire directions:

| Family | Desktop sends | Desktop receives |
|---|---|---|
| SMS/MMS | `kdeconnect.sms.request`, `.request_conversations`, `.request_conversation`, `.request_attachment` | `kdeconnect.sms.messages`, `.attachment_file` |
| Notifications | `kdeconnect.notification.request`, `.action`, `.reply` | `kdeconnect.notification` plus any image payload |
| Contacts | `kdeconnect.contacts.request_all_uids_timestamps`, `.request_vcards_by_uid` | `kdeconnect.contacts.response_uids_timestamps`, `.response_vcards` |
| Calls | `kdeconnect.telephony.request_mute` | `kdeconnect.telephony` lifecycle/identity events |
| Battery | `kdeconnect.battery.request` where needed | `kdeconnect.battery` |
| Files | `kdeconnect.share.request` with payload as appropriate; `kdeconnect.sftp.request` | File-transfer payloads and `kdeconnect.sftp` connection metadata |

Do not confuse receiving phone notification mirroring with sending desktop notifications to the phone; the latter is not required by these two surfaces.

Required state rules:

1. Scope identities by device; two phones must not share message/attachment/notification state accidentally.
2. Upsert repeated messages by stable ID and sort by timestamp; historical fetches must not move an older message into a conversation’s newest-preview slot.
3. Distinguish initial history/replay from a new live event so reconnects do not trigger old SMS alerts or duplicate recent-log entries.
4. Preserve outgoing direction, participant order, SIM identity and attachment type through conversion.
5. Cancel/replace notifications and active call entries correctly; do not create a new event on cancellation.
6. Bound histories, payloads, image caches and background work; keep D-Bus and UI responsive during downloads, contact refreshes and large histories.
7. Surface failure rather than pretending a message was sent. Transport acceptance is not proof of carrier delivery; do not invent delivery receipts the backend cannot obtain.

### Companion-app setup is part of the contract

The desktop replacement still needs a compatible app on the phone, correct pairing, and the relevant phone-side permissions/plugins: SMS read/send and phone-state access, contacts, notification access, and storage/file-browsing grants where supported. A missing permission must be distinguishable from an empty inbox, a zero-percent battery or a disconnected device. Background/reconnect behavior must be tested with the phone screen asleep. Desktop code cannot manufacture capabilities or permissions that the companion app does not provide.

Sensitive contact/message/image caches should retain private user access, and peer-provided file names must not escape their device-scoped cache. Authentication and path validation apply to attachment and notification payloads as well as ordinary shared files.

## 7. Storage and remaining dependencies

Contacts currently come from plain files under `$XDG_DATA_HOME/kpeoplevcard/kdeconnect-{device}/`. Preserve that layout initially or update both consumers together. **The folder name does not require the kpeople library.** Preserve raw vCard photos, folded/encoded fields, multiple numbers and updates/deletions. `ContactService` and the registry can retain their current rendering/matching behavior.

Attachment helpers currently search KDE-style cache directories and also use explicit downloaded paths. Either maintain the legacy cache layout or replace those searches together, including the island audio fetch path. Cache lookups must not mix attachments with the same ID from different phones. Keep file URLs/paths valid through shell reloads, and store notification images durably before their temporary image-provider URLs expire.

Phone file browsing presently consumes a **local filesystem mount** through `FolderListModel`, direct file copies and QtMultimedia. An SFTP credentials endpoint alone does not preserve this. Initially retain sshfs/FUSE and its required user-session permissions, or implement a local filesystem facade. A pure remote listing API would require rewriting the file browser and viewers.

These dependencies may remain independently of KDE Connect:

| Dependency | Why it remains unless separately replaced |
|---|---|
| Quickshell, Qt Quick and QtMultimedia | Existing shell UI and inline image/video/audio playback |
| Python/PyGObject/Gio and python-dbus | Current bridges and notification-action helper |
| BlueZ and PipeWire; optionally the existing oFono route | Bluetooth connectivity and call control |
| sshfs/FUSE and SSH support | Current mounted phone-files interface |
| ffmpeg/ffprobe | Voice recording, durations, video thumbnails and MMS conversion |
| ImageMagick/exiv2 where used | Image compatibility and thumbnail helpers |
| yt-dlp and network access to supported services | Rich inline online media and protected-media caching |
| LocalSend | Island LocalSend panel, independent of direct-to-phone share |

The installed KDE Connect package currently depends on KDE components including kio, kirigami, kpeople, knotifications, modemmanager-qt and pulseaudio-qt. A Rust D-Bus service need not inherit them. Exact disk/RAM savings require checking what the rest of the OS still needs; removing KDE Connect does not eliminate the shell’s existing Qt dependency.

## 8. Shell/distribution changes required for the swap

1. Replace `Phone.qml`’s executable check for `kdeconnectd` with service availability/activation and feature detection for the replacement. React to the backend appearing/disappearing, rather than checking a binary once.
2. Replace the `kdeconnect-app` pairing/settings launcher and phone setup instructions with the replacement’s pairing UI/CLI integration.
3. Make capability and connection gating backend-neutral, including Bluetooth-only call paths where intended. Keep the user’s Phone-tab enable/disable preference.
4. Preserve the D-Bus service, object paths, contact storage and attachment-cache contract, or migrate every associated consumer in the same change.
5. Preserve origin hints, contact privacy, notification suppression, DND and fullscreen behavior. Notification replies must remain possible after notification persistence/reload when the underlying phone handle is still valid.
6. Package/start the Rust daemon in the graphical user session with access to D-Bus, its identity/cache files and any required FUSE helpers. Check installation policies; this repository already lists `kdeconnect` in the Arch KDE package blocklist.
7. Prevent the old daemon from reclaiming the shared bus name through autostart or D-Bus activation. Choose an explicit migration policy for existing pairing certificates versus re-pairing.
8. Test the packaging/session sandbox with the actual cache directories and SFTP mount paths, not only by starting the binary manually.

## 9. Independently observed risks in the current rust-connect source

These are observations from the audited source, not accepted claims about completed features. Snapshot: `e6aedaf`, following earlier `a2bda16`; concurrent work elsewhere may subsequently change it.

| Priority | Observation | User-visible consequence |
|---|---|---|
| P0 | `SmsMessage` expects `threadID`/`uID`/`subscriptionID`, while current upstream serializes `thread_id`/`_id`/`sub_id`. Attachment conversion similarly expects camelCase fields. | Real incoming messages can lose thread/message identity; attachment MIME, identity and thumbnails can disappear. |
| P0 | D-Bus send methods build `phoneNumbers`/`sendSms` or thread-only payloads; both discard `_attachments`. | Text sends are not compatible with the current receiver; photo/GIF/video/voice-note sending cannot work through those methods. |
| P0 | History requests use a `range` array; attachment requests use camelCase keys; downloaded attachments are named from `uniqueIdentifier` instead of the response’s `filename`. | History ranges and attachment fetch/play/save paths can fail despite the methods existing. |
| P0 | The event loop emits per-message updates, but `conversationLoaded`/created/removed are only declared; the store appends batches and selects `.last()` rather than upserting/sorting. | Loading completion, duplicate history and latest-message previews are not reliable. |
| P0 | Notification raw Android IDs are inserted directly into D-Bus object paths, with registration errors discarded. A notification ID in the repository’s own upstream fixture contains illegal path characters. | Visible notifications can lack the objects needed for reliable replies and actions. |
| P1 | Telephony compatibility forwards ringing metadata only; it does not reproduce the KDE call-notification/mute/cancellation path, and ringing cancellation is not excluded there. | Wi-Fi call alerts and mute affordances are incomplete even if the independent HFP route works. |
| P1 | `loadedPlugins()` returns a fixed list, and several D-Bus sends discard transport errors. | Unsupported controls appear available and failed requests may look successful. |
| P1 | SFTP mounts the advertised remote root, while the shell prepends the mount point to advertised absolute directories. | Root-path duplication is possible; validate and normalize this before claiming working phone browsing. |
| P1 | The desktop notification code deliberately does not map shell close events to remote dismissals. | Explicit dismissal synchronization needs a defined implementation, with expiration excluded. |
| P1 | The unmodified shell still checks for `kdeconnectd` and launches `kdeconnect-app`. | The Phone tab can disappear or setup can fail after removing KDE Connect, regardless of backend protocol support. |

Source anchors: `src/plugins/sms.rs`, `src/dbus/conversations.rs`, `src/dbus/mod.rs`, `src/dbus/device.rs`, `src/dbus/sftp.rs`, `src/plugins/sftp/mod.rs`, `src/plugins/notification.rs` in the rust-connect checkout; shell locations are listed above.

The source builds with `cargo check --locked --offline`, and its four current `dbus_test` tests pass. Those tests verify selected local types/conversions, not full shell-to-phone compatibility. The notification fixture’s raw ID was independently checked and rejected as a D-Bus object path. No live backend cutover was performed.

## 10. Acceptance criteria before removing desktop KDE Connect

Each test must run with KDE Connect’s desktop daemon absent, including its activation service. Use upstream-derived fixtures and at least one real Android device. A Rust-generated fixture that repeats the implementation’s field assumptions is insufficient.

| Test | Required result |
|---|---|
| Fresh session, pair, restart and phone sleep/wake | Stable pairing identity; correct reachability; automatic recovery; no rapid connect/disconnect loop |
| Two phones, overlapping local IDs | Correct per-device conversations, images, notifications and actions |
| Initial synchronization with many threads | Correct recipients, latest previews, cache replay and no flood of old-message island alerts |
| Open cached, uncached, empty and long threads | Correct ordered messages and an explicit completion/failure; responsive UI |
| Receive a message while its thread is open or closed | Immediate thread/list/island update with one logical message |
| Fetch the same history repeatedly | No duplicate messages, no stale preview replacing a newer one |
| SMS to new and existing contacts; group message | Correct recipients and SIM context; offline/send failures remain visible |
| Attachment-only and text-plus-attachment sends | Photo, GIF, video, recorded voice note and multiple attachments arrive correctly |
| Receive image/video/audio MMS | Correct classification, thumbnails, original-file fetch, playback, seek and save |
| Shell reload and backend reconnect | Histories and contact pictures survive; no duplicate alerts; saved handles behave correctly |
| Contact created/changed after a call | Names/photos update consistently in both current events and retained logs |
| Notification from a non-SMS app | Correct app/contact/body/image and phone-origin routing |
| Inline reply and a safe background action | Correct notification and originating phone receive the requested operation |
| Update/cancel/reconnect notification | One logical notification; correct replacement/removal; no re-dump popup storm |
| Explicit dismissal versus timeout | Defined local/remote behavior; timeout must not delete a phone notification accidentally |
| Ringing, missed, answered, ended and cancelled call | Correct number/photo/lifecycle/history; mute works; HFP buttons reflect actual capability |
| Phone connected only by Wi-Fi versus Bluetooth HFP | Mute is not misrepresented as Answer; supported call-control route is explicit |
| SFTP roots and stale mounts | Storage/camera/download/screenshot paths resolve, mount errors recover, copy/save/view work |
| File sharing versus LocalSend | Direct-to-phone share works; separate LocalSend behavior remains intact |
| DND, privacy and fullscreen | No duplicate phone popups; hidden fields stay hidden; island never appears over fullscreen |
| Optional iPhone support | ANCS/HFP features tested separately; unsupported history/send capabilities are not advertised |
| Packaged service with KDE components removed | Phone tab remains visible when enabled; pairing works; caches, D-Bus and mounts work without KDE GUI/Frameworks |

Completion means the real shell flows pass, not just that the binary builds or that interface names appear in introspection.

## 11. Suggested implementation order

1. Correct the SMS/MMS packet model, sender and history/attachment translation using upstream fixtures.
2. Implement the conversation lifecycle and deterministic, device-scoped cache behavior.
3. Complete notification object identity, reply/action routing and explicit cancellation/dismissal semantics.
4. Verify contact/photo cache behavior, battery/capability reporting, call lifecycle and SFTP path mapping.
5. Update shell availability checks, pairing entry points and session packaging.
6. Run the acceptance matrix with the old desktop backend stopped, then perform the dependency-removal trial.

This sequence preserves the existing UI and targets the actual backend contract before spending time rewriting working presentation code.
