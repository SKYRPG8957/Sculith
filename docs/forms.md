# Shared simple forms

Rust plugin API v3 can open one edition-independent simple choice form and receive
its validated selection/cancellation. Bedrock uses a native menu form; unmodified
Java 26.3 uses a vanilla multi-action dialog. This is a functional UI fallback,
not automatic conversion of arbitrary Bedrock JSON UI, images, HUD or scripting.

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/forms-preview.toml
```

This source example also enables the existing gameplay welcome and shared texture
pack. Append `.exe` to plugin paths on Windows. Public Preview 12 includes the
forms executable and enables it only in new settings. Existing operator settings
are preserved.

## Plugin contract

Subscribe to `Hook::PlayerJoined` and `Hook::FormSubmitted`. After initialization:

```rust
use sculith_plugin_api::{Action, SimpleForm};

let action = Action::ShowForm {
    player_id: player.id,
    request_id: 7,
    form: SimpleForm {
        title: "Welcome 👋".into(),
        body: "Choose an option.\nThe same menu works in both editions.".into(),
        buttons: vec!["Help".into(), "Close".into()],
    },
};
```

The example is a callback fragment; `player` comes from `Event::PlayerJoined`.
`request_id` is your own request key, not a native packet identifier. The result is
`Event::FormSubmitted { player_id, request_id, selection }`: `Some(index)` for a
button, `None` for cancellation. The Java exit button/Escape maps to cancellation;
Bedrock closed/busy/null responses also map to cancellation. A button that you
label "Close" remains a normal indexed choice. Results go only to the plugin
that opened the form, even when another plugin subscribes to the same hook.

Title and each button: nonblank, at most 128 UTF-8 bytes, no control characters.
Body: at most 2,048 UTF-8 bytes; newline is supported. There must be 1–8 buttons.
Labels are plain text, never client commands, action identifiers or arbitrary JSON.
The host validates the entire action batch before applying it.

The world owner checks a live initialized lease, one pending form per player and
a one-per-second form budget. It allocates nonwrapping internal IDs and checks
session, pending ID, choice bounds, monotonic time and the 60-second reply deadline.
Wrong IDs/choices retain the valid pending request. A valid selection/cancellation
consumes it once; replay cannot notify the plugin twice. Expired replies clear it
without calling the plugin. Disconnect cleanup removes pending state.

Opening a form never changes a block revision and never reenters game hooks as a
new request. Accepted replies are semantic events, separate from the protocol
observer stream. The existing bounded mailbox/game-event/worker policies apply.
Closed sessions, pending forms and rate limits are ordinary action rejections,
logged without disabling an otherwise valid plugin.

## Client implementation and verification

The unmodified Java game displayed Korean/emoji title, body and buttons:
[Exact source, actual mouse/Escape outcomes and hashes](verification/2026-11-16-shared-forms.md).

![Actual stock Java shared menu](verification/shared-form-java.png)

An actual mouse click reached the owning Rust plugin and displayed its private
help response while the server-delivered checkerboard remained active:

![Actual stock Java button result](verification/shared-form-java-result.png)

The independent executable probe checks the same text in both editions:

```sh
python3 scripts/forms_smoke.py --client target/gophertunnel-probe
```

An independent Python NBT reader checks the Java direct dialog holder, nested
modified UTF-8 and static custom actions. The separate Go/Pion Bedrock client
decodes native form JSON and returns numeric choices/cancellation. It attempts to
answer another session's form and sends a malformed choice first. The Java form
stays pending, receives no foreign private result and can still be answered.
The owning Rust worker receives exactly four callbacks across choices and
cancellations, while a second subscribed observer worker receives none. Replays
do not add callbacks. Graceful stopping is checked.

Domain/owner tests cover pending/dead leases, wrong IDs, invalid indices, replay,
clock regression, expiry, rate limits, ID exhaustion, no world revision change and
separation of presentation from semantic result events. SDK tests cover highest
common version selection and rejection of UI hooks on a v2-only host. Real retained
v1 and v2 Rust binaries continue to pass the corresponding executable checks.

Primary wire facts come from the pinned official Java 26.3 codecs: play ShowDialog
143 uses an inline holder followed by network NBT; CustomClickAction 68 includes a
length-prefixed optional NBT payload. This adapter emits static custom actions with
no payload and accepts their one-byte TAG_End payload. Java text is encoded as
modified UTF-8, including nested supplementary characters. Native packets are
ModalFormRequest/Response 100/101 in the pinned V2225 dependency and independent Go
codec. No decompiled implementation or proprietary game assets are distributed.

Only simple forms are supported. There is no text input, arbitrary JSON UI,
replacement/cancel API, automatic on-screen timeout close or expiry callback yet.
An unanswered form occupies the one pending slot until a response or disconnect;
an expired answer is not applied. Callback success does not grant permissions for
future game actions. Official Windows Bedrock rendering remains unverified;
native results here are independent protocol-client results. Plugins remain
trusted executables, not a sandbox.
