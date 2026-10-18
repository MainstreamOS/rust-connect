# Mainstream Phone tab and Dynamic Island compatibility

The desktop daemon now provides a session D-Bus service under
`org.kde.kdeconnect`, using the existing Rust transport and plugins. It does not
link KDE Frameworks, Qt, KIO, KPeople or KNotifications. This implements the
backend boundary in [the independent requirements](mainstream-shell-requirements.md),
against dots-hyprland `mainstream` at `c88cbb2c5516a731a6069ca57a2c1248ebb14769`.

## Implemented boundary

| Shell flow | Rust interface / behavior |
| --- | --- |
| Phone discovery and pairing | `/modules/kdeconnect`: `devices`, `selfId`, device list/visibility events; per-device name/type/reachability/pairing properties, `requestPair`, `unpair`, `loadedPlugins`, `hasPlugin`. Pairing uses the certificate-anchored pairing store; capabilities reflect the peer identity. |
| Battery | `/battery`: charge, charging/availability and `refreshed`; unknown charge is `-1`. |
| Conversation list and history | Device-root conversations interface, variant-wrapped message values with KDE's `(isa(s)xiixixa(xsss))` signature; index-range cache replay followed by Android's timestamp/count request. Upserts use IDs and timestamps, with explicit completion signals and bounded caches. The `/sms` fallback supports `requestAllConversations`. |
| Text and MMS sending | `sendWithoutConversation`, `replyToConversation`; Android v2 recipients, actual base64 attachment bytes/MIME/file names and implemented `subID` SIM field. Transport errors propagate to D-Bus callers; acceptance does not claim carrier delivery. |
| MMS image/video/audio retrieval | `requestAttachmentFile` sends Android's snake-case keys; `attachmentReceived(path, identifier)` follows authenticated TLS retrieval. Filenames cannot escape the device-scoped cache. Up to 64 requests are queued/deduplicated with four active slots reserved before the phone opens its transfer listener. A five-minute overall deadline and disconnect cancellation bound jobs. `attachmentFailed(identifier, error)` and `attachmentState(identifier)` expose failures/retries without claiming success. Each payload is at most 100 MiB; per-phone cached originals are trimmed to 512 MiB / 1,024 files. |
| Contacts and pictures | `/contacts` synchronization plus raw, photo-preserving vCards under `$XDG_DATA_HOME/kpeoplevcard/kdeconnect-{public-device-id}/`. A completed snapshot updates/deletes files, then emits `localCacheSynchronized`. Changed cards must arrive before completion. Revision checks skip unchanged snapshots before copying photos or hashing data. Files are 0600, directories 0700. |
| Phone notifications | Legal, stable D-Bus object keys map separately to Android raw IDs. Notification item properties, reply handles, actions and explicit dismissals back the existing shell helpers. Objects are published before desktop alerts. Phone-origin and image hints remain available to island suppression/privacy logic. |
| Reconnects and large notification lists | Content deduplication preserves desktop replacement IDs; up to 2,048 active records retain reply/action metadata independently of the REST history's 100-entry cap. Event backlog recovery rebuilds device/battery/storage state, latest conversation previews, watched thread contents/completion, attachment results and call state as well as active notification objects. A bounded call journal includes cancellations; replay does not ring for ended calls. Desktop-server replacement invalidates old server IDs. |
| Calls | Ringing/missed-call events, number/contact metadata, private thumbnail cache and compatible desktop call alerts. Mute routes to the phone; talking/cancellation/disconnect removes the active call. Answer/decline/hang-up continue through the shell's existing Bluetooth HFP route. |
| File sending / browsing | `/share.shareUrls` sends authenticated payloads; failures after acceptance emit `transferFailed`. `/sftp` provides mount/status/error/roots. Local roots are relative to the actual mount root, including multiple phone storage roots. Existing sshfs/FUSE browsing remains in use. |

Incoming message parsing accepts current Android `thread_id`, `_id`, `sub_id`
and snake-case attachment metadata, while preserving the existing REST JSON
names. Repeated history batches cannot append duplicate messages or replace a
newer conversation preview with an older message. History has a 500-message
per-thread / 200-thread per-device memory bound. Empty cache replays complete;
a disconnected phone returns an error instead of pretending to refresh.

Outgoing MMS is bounded to 16 files and 8 MiB total decoded bytes. The shell's
existing compression/conversion helpers should run before sending larger media.
The message UID is a 32-bit D-Bus integer to match KDE's existing wire ABI;
thread IDs, SIM IDs and timestamps are 64-bit. IDs outside the signed 32-bit
message ABI are retained unchanged in the core store and rejected explicitly at
the compatibility boundary, with a `conversationError(thread, error)` signal;
they are never truncated or turned into zero. Other threads continue updating.
Displaying those exceptional IDs would require an extended wire API and client
support rather than silently violating KDE's signature.

Notification expiry and programmatic closure do not cancel the phone alert.
Explicit user dismissal does. Network send failures remain errors. Pairing
revocation clears compatibility notification objects and the contact mirror;
an ordinary offline interval retains the shell's existing historical display.
The service releases its bus name and removes exported objects on shutdown.
If KDE Connect already owns the name, Rust logs that compatibility is unavailable
and keeps its native REST/CLI service running; it does not take over that name.

The companion changes in dots-hyprland's Phone bridge subscribe to the optional
`attachmentFailed` and `conversationError` signals. They use the existing error
UI to stop attachment/thread loading and display the error; KDE Connect remains
compatible because it simply does not emit those extension signals. Install the
updated shell sources along with the new daemon for immediate UI error reporting.

## Upstream evidence

These shapes were researched independently rather than copied from earlier
rust-connect D-Bus work:

- Android [`SMSPlugin.kt`](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/plugins/sms/SMSPlugin.kt),
  [`SMSHelper.kt`](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/helpers/SMSHelper.kt),
  [`NotificationsPlugin.kt`](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/plugins/notifications/NotificationsPlugin.kt)
  and [`TelephonyPlugin.kt`](https://github.com/KDE/kdeconnect-android/blob/f6a699de894d1bad83bfbcc61e5e757a54788051/src/main/java/org/kde/kdeconnect/plugins/telephony/TelephonyPlugin.kt).
- Desktop [`conversationmessage.h`](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/models/conversationmessage.h),
  [`conversationsdbusinterface.cpp`](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/plugins/sms/conversationsdbusinterface.cpp)
  and [`smsplugin.cpp`](https://github.com/KDE/kdeconnect-kde/blob/b180447d2afe0b9f6bc70e812ce214763201edc0/plugins/sms/smsplugin.cpp).

The Android v2 fixture in `tests/fixtures/upstream-wire/sms/android_v2_batch.json`
uses those receiver/serializer keys with synthetic content; the provenance index
records the independently observed revision separately from older capability pins. It deliberately includes unsorted messages,
integer read flags, outgoing direction, a SIM identifier and audio attachment
metadata. Older fixtures remain for legacy REST behavior.

## Install and cut over deliberately

From this branch, install the daemon normally, then install the optional
user-local compatibility entry points:

```sh
./packaging/install-user-service.sh
./packaging/install-shell-compat.sh
```

The second script installs small `kdeconnectd` and `kdeconnect-app` launchers
beside `~/.local/bin/rust-connect`, plus a user D-Bus activation file. It refuses
to overwrite unrelated local launchers/activation files. Ensure `~/.local/bin`
is on the graphical session's PATH. The settings launcher runs `rust-connect
app`, which opens the existing local web UI without putting an API key in the
URL or command line. Use that UI/CLI to pair with the phone's companion app.
The two existing shell executable checks then work without a KDE GUI package.

Before a live cutover, stop the old desktop KDE Connect daemon and disable its
autostart/activation according to the distribution. Restart the Rust user
service so it can acquire the name. Keep the existing phone application;
re-pair with Rust's separate desktop identity. This change does not import
KDE's pairing certificate, remove packages, change the shell, or install these
launchers automatically.

The supplied systemd user unit includes the contact mirror in its writable
paths. If using non-default XDG data/download paths, adjust `ExecStartPre` and
`ReadWritePaths` in a user drop-in for those paths. sshfs/FUSE and its mount
permissions remain necessary for the existing phone file browser. The shell
still needs its Python/Gio/python-dbus, QtMultimedia and media helpers; call
control still needs BlueZ/PipeWire HFP (or its existing oFono route). LocalSend
and local island panels are independent of this backend.

## Validation and remaining device acceptance

Run the private-bus integration tests without a real phone:

```sh
cargo test --locked --test shell_compat
cargo test --locked --lib
cargo test --locked --test notification_reply_tests --test rust_capability_inventory --test packaging_lint
```

To exercise dots-hyprland's Python/Gio Phone bridge as well:

```sh
MAINSTREAM_PHONE_BRIDGE=/path/to/dots-hyprland/dots/.config/quickshell/ii/scripts/phone/kdeconnect-bridge.py \
  cargo test --locked --test shell_compat dbus_sends_mms
```

This optional cross-repository check requires Python's Gio bindings. It gives the
bridge a private session bus and temporary HOME/cache/data directories and checks
its device, conversation, thread and attachment-error output using the existing
shell's transport. The small optional-signal subscriptions described above keep
the existing Phone UI informed without changing its layout.
When running the wider suite on a desktop, unset DISPLAY/WAYLAND_DISPLAY and set
DBUS_SESSION_BUS_ADDRESS to an unused socket path to keep unrelated integration
fixtures out of that session; the private-bus harness supplies its own address.

The harness starts isolated D-Bus daemons, fake notification servers, temporary
certificate/cache directories and a loopback TLS peer. It tests actual D-Bus
variants/signals, large history completion, upserts, MMS bytes, reply/action
packets, call mute/cancellation, notification identities, contact photo updates
and deletions, private file permissions, multiple storage roots and launcher
installation into a temporary HOME. Stress checks include a six-file burst,
queue capacity/disconnect cancellation, truncated-file retry, 32-bit ID boundary
errors, idle contact revisions and deliberately overflowed event channels. It never uses the desktop's session bus or
sends a message to a real phone.

These automated checks do **not** establish full replacement readiness. Before
removing desktop KDE Connect, run the requirements' acceptance matrix with the
real shell and an Android phone: permission grants, sleep/wake/reconnect,
group/SIM messages, image/video/audio receive/play/save/send, phone storage
mounts, call photos and HFP controls, notification privacy/DND/fullscreen policy,
shell/backend restarts and packaged-service operation without KDE libraries.
iPhone ANCS/HFP support remains the shell's separate, limited route; it does not
acquire Android's SMS history/MMS capabilities from this service.
