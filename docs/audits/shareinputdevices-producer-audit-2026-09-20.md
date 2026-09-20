# Audit — shareinputdevices producer vs XDG InputCapture portal spec + KDE Connect protocol — 2026-09-20 (vk #1121)

Prosecutor-shaped adversarial review of the InputCapture producer
(`src/plugins/shareinputdevices/`, 5 files, 5,967 lines) at `4bcd182`,
against the XDG InputCapture portal spec (v2 XML), kdeconnect-kde
(upstream producer), kdeconnect-android (consumer), and libei/reis
semantics. Brief staged in vault
(`projects/rust-connect-audit-2026-09-20-fable-brief.md`) with the full
already-litigated list (4 panel rounds, 31 findings, M3 cycle) excluded
from scope.

**Execution model transparency:** fired at claude.ai Fable 5.1 High on the
Max account; Anthropic's safeguard classifier **downgraded the session to
Opus 4.8 High** (documented Fable 5 behavior for security-shaped prompts —
see vault `references/claude-fable-mythos-5.md`). The prosecution below is
Opus 4.8's work. It fetched the portal spec + impl-portal spec and read all
five attached files; upstream cpp was provenance-gated in its environment.

**Integrator verification (main session, laptop, same day):** every
finding was re-checked against the repo at `4bcd182`, the upstream clone
(`~/repos/kdeconnect-kde` @ 39ddfd1a7 + master on invent.kde.org), the
v2 spec XML (flatpak/xdg-desktop-portal `data/org.freedesktop.portal.InputCapture.xml`),
mutter's parser (GNOME/mutter `src/backends/meta-input-capture-session.c`),
xdg-desktop-portal-kde's parser (`src/inputcapture.cpp`), and Qt's
demarshaller (qtbase `qdbusdemarshaller.cpp`). Dispositions are recorded
per finding; where the prosecution's claim needed refinement, the
refinement is part of the record.

## FINDING 1 — [BLOCKER, VERIFIED] Barrier position goes out as `ai`; the portal requires `(iiii)` — the producer is dead on GNOME

`portal.rs:390-394` (`barrier_position` → `Value::Array([x1,y1,x2,y2])`),
consumed at `portal.rs:694` and in `rearm_barriers`. The spec
(`org.freedesktop.portal.InputCapture.xml` SetPointerBarriers, position
key) and the impl spec both declare `position (iiii)` — a struct, not an
array.

The code comment (`portal.rs:384-389`) defends `ai` as byte-parity with
upstream `QList<int>` (inputcapturesession.cpp:230). **The parity claim is
true** (verified in our clone AND on upstream master — unchanged), and it
does not save us:

- **GNOME/mutter (supported target):**
  `handle_add_barrier` does `g_variant_get(position, "(iiii)", ...)`
  (meta-input-capture-session.c:890). GLib is strict: an `ai` value fails
  the decode (CRITICAL, out-params unset), `check_barrier` runs on
  garbage, AddBarrier returns ACCESS_DENIED → the barrier lands in
  `failed_barriers` → our `start()` errors → **plugin inert on GNOME.**
  Upstream kdeconnect-kde is broken there too; its `barriersSet` handler
  doesn't treat `failed_barriers` as fatal, so it just never activates.
  "Upstream does it" is not evidence it works — on mutter it doesn't.
- **KDE impl portal (why upstream's `ai` "works" on KDE):**
  xdg-desktop-portal-kde parses with `beginStructure()`
  (inputcapture.cpp:386-390), and Qt's demarshaller makes no type
  distinction — `beginStructure()`, `beginArray()`, `beginMap()` all call
  the same `beginCommon()` with no wire-type check
  (qtbase qdbusdemarshaller.cpp:340-353). Qt reads four ints out of either
  aggregate shape. The leniency is Qt's, not the spec's.

**Fix:** emit a real struct — `Value::from((x1, y1, x2, y2))` (→ `(iiii)`),
same shape `insert_doubles` already uses for `(dd)` at `portal.rs:276-278`.
Correct on mutter, still correct on KDE. The suite's `ai`-pinned wire
tests are the self-consistency trap the brief predicted: they prove
round-trip shape, not portal acceptance. Fix the comment at
`portal.rs:384-389` in the same change — its "the struct route is wrong
here" is backwards.

## FINDING 2 — [HIGH, VERIFIED] ZonesChanged rearm never re-calls Enable; the session stays suspended after any monitor change

The spec (SetPointerBarriers doc): "Setting pointer barriers immediately
suspends the current session and the application must call Enable() after
this method." `rearm_barriers` (`portal.rs:1480-1549`) does GetZones →
plan → SetPointerBarriers → check `failed_barriers` → update state → Ok.
**No Enable.** Contrast `start()`'s explicit step 7 Enable
(`portal.rs:748-752`), and upstream, whose `barriersSet` — the response
handler for EVERY SetPointerBarriers, initial and rearm alike — ends with
`enable()` (inputcapturesession.cpp:251).

Failure: on the first ZonesChanged (monitor hotplug, resolution/scale
change, lid event), the barrier is re-armed but the session stays
suspended; capture silently never fires again for the session's life. On
real multi-monitor GNOME/KDE this is routine. The fake portal never emits
a realistic ZonesChanged and no test asserts Enable-follows-rearm.

**Fix:** call Enable after a successful rearm (mirror upstream: Enable on
every SetPointerBarriers success), plus a test that drives ZonesChanged
through the fake portal and asserts the second Enable.

## FINDING 3 — [HIGH, VERIFIED with refinement] Silent peer death during active capture traps the cursor until TCP gives up

Both relay arms swallow send failures: `mod.rs:1727-1736` and
`mod.rs:1772-1781` log `shareinputdevices_activated_send_failed` /
`wire_send_failed` and continue — no Release, no deactivation. The only
cursor-return paths are the peer's release packet (`handle_packet`
`mod.rs:1929-1956`) and gate deactivation, which needs a DeviceEvent.

Refinement found in verification (bounds the trap, does not close it): a
failed write DOES tear the link down — `send_failure_teardown`
(`connection/mod.rs:628`, the 2026-09-02 A3 fix) → StateChanged
(Disconnected) → gate → `do_deactivate` → Disable → cursor freed. The
trap is therefore bounded by **when the write actually errors**, not by
any app-level liveness check: with a silently-dead peer (sleep, Wi-Fi
drop, no FIN/RST) that is the kernel's TCP retransmit timeout — tens of
seconds to minutes — during which the pointer is pinned at the barrier
and keystrokes flow into a dead socket.

**Fix shape:** during active capture, treat a relay send error as a
cursor-safety event — initiate Release/Disable immediately instead of
warn-and-continue (the teardown cascade already proves the peer is gone);
longer-term, an app-level liveness signal while capture is active so the
trap window is seconds, not TCP-scale.

## FINDING 4 — [MEDIUM, VERIFIED] Portal Disabled with the peer still connected deactivates permanently; the comment claiming re-evaluation is false

`set_on_disabled` (`mod.rs:1490-1533`) takes the slot, flips
`backend_available=false`, spawns close — and emits **no DeviceEvent**.
The gate's subscription loop only wakes on the DeviceEvent broadcaster
(StateChanged/Paired/Unpaired, `mod.rs:851-901`); the eager re-eval runs
once at spawn. The comment at `mod.rs:1506-1511` asserts "the Disabled
signal arrives on the bus subscription and re-evaluates against the
consumer snapshot" — but Disabled arrives on the portal D-Bus signal
stream, a different channel. The claimed re-eval does not exist.

Failure: compositor-side Disabled unrelated to the peer (permission-store
revocation, another client taking capture, session suspend/lock, portal
restart/name-owner change) while the phone stays connected → barrier dead
until the next device event, which may never come. This is the deferred
"post-deactivate re-arm with zero subsequent events" residue made
user-visible; the acceptance rationale ("all operational recovery paths
re-evaluate") does not cover it.

**Fix shape:** after a Disabled-driven deactivation, drive one
`do_evaluate_after_event` (the named Arc-clone refactor of
`do_evaluate_after_event` from the M4 disposition unblocks the Send
bound), and correct the comment.

## Task 5 — #1102 flake verdict (analysis, consistent with the day's independent diagnosis)

- **(a)** Phase 3 is **fail-loud, not fully race-free**: rust's mDNS
  unicast (Path Y, kde dials rust, proves nothing) can still beat the
  desired UDP-nudge → rust-dials path (Path X). A Path-Y win now dies
  loudly at the `wait_for 15 "Starting client ssl"` instead of passing
  vacuously. The 4/4 is sequence-usually under current timings, small N.
- **(b)** In-harness the nudge lands against a rust with no live link
  (clean SIGTERM/SIGKILL delivers RST; readiness wait is up to 30 s), so
  the ~60 s reaper window does not open in practice. **Recommended
  harness hardening:** assert rust logged a link-drop for KDE_ID before
  issuing the nudge — makes Path-X ordering explicit, not timing-lucky.
- **(c)** Phase-1 zombie-device mitigation ranking: **coalesce rust's
  startup + network-change announces (winner — removes the double-dial
  provocation in our own code, hides nothing)**; report the
  deviceAdded-without-reachableChanged ctor-ordering race to KDE (also,
  but not alone); detect-and-restart kdeconnectd (**rejected** — masks a
  real interaction, can re-trigger the race, dishonest).
- **(d)** Run-count math: 0 failures in N runs gives a 95% upper bound of
  ~3/N (rule of three): ~60 clean runs to claim <5%, ~20-30 for an
  interim <10-15% gate, plus a targeted stress that forces same-cert
  redials. Back-to-back cadence is fine (netns-isolated, zero-leak
  verified; the double-dial is intrinsic to rust's announce cadence, not
  a cadence artifact) — confirm with a spaced-out comparison run set.

## Ledger — plausibly RIGHT, evidence missing (functional-coverage.md material, not defects)

- Barrier geometry pinned only to synthetic 1920×1080 zones — never real
  mutter/KWin GetZones, never fractional scaling.
- `cursor_position (dd)` decode-tested but never accepted by a real portal.
- Scroll y-negation asymmetry (`mod.rs:345-386`) — deliberate upstream
  mirror, unvalidated against a real phone (M4b).
- `CONSUMER_INCOMING_CAPS` AND-match (`mod.rs:151-154, 186-237`) assumes
  phones advertise both caps — unverified until M4b; if one cap,
  activation silently never fires.
- specialKey table — transcribed-oracle-tested, never phone-consumed.
- +8 keycode offset, keymap NUL-strip (`ei.rs:966-1053`), and
  `update_key`/`update_mask` mixing (`ei.rs:687-732`) — synthetic keymaps
  only, knowingly cpp-mirrored, unvalidated live.
- Release omits `activation_id`; CreateSession's *granted* capabilities
  never checked against the request (`portal.rs:608-627` checks only
  SupportedCapabilities at probe) — both benign-looking, both unexercised
  against a real compositor.
- Permission-store denial vs portal-absent: `probe_portal_available`
  (`portal.rs:132-200`) does not distinguish them.

## Sound (checked in verification, no finding)

Deactivated → log-only (session stays armed, correct); Release-without-
activation_id omission (spec: optional key, benign for one barrier);
SIGKILL-while-armed (portal sees the D-Bus drop, compositor releases);
graceful shutdown (Drop → best-effort Close); Disabled during active
capture (spec emits Deactivated first, cursor already returned).

## Next actions

1. Fix lane: Findings 1 + 2 (same file, both wire-shape/lifecycle —
  `(iiii)` struct + Enable-after-rearm + comment corrections + red tests
  driving a spec-strict fake portal that decodes `(iiii)` and asserts
  Enable-after-rearm).
2. Fix lane: Finding 3 (send-error → immediate Release path) and
   Finding 4 (post-Disabled re-eval + comment correction) — can ride one
   branch.
3. Harness: assert rust link-drop for KDE_ID before the phase-3 nudge
   (Task 5b); coalesce startup/network-change announces (Task 5c — also
   the #1102 Phase-1 mitigation decision, pending George).
4. Upstream: kdeconnectd destroy/recreate zombie-device race report
   (evidence pack ready in vk #1102); optionally the upstream `ai`
   barrier position as a GNOME interop note.
5. M4b instrumentation list per Task 6 of the brief (dbus-monitor of
   SetPointerBarriers, Enable-after-rearm assert, real GetZones logging,
   EIS disconnect reason codes).
