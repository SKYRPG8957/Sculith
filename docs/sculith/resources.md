# Shared resources

The fork can compile one authored Bedrock resource pack for both editions at
startup. No pack or server-specific content is enabled by default.

Configure the pack in `sculith.toml`.

```toml
[resource_pack.shared]
source = "resource_packs/authored"
mapping = "resource_packs/mapping.json"
required = true
```

Use a Bedrock `.mcpack` as the source with the same mapping file. A ZIP that
contains `bedrock/manifest.json` and `mapping.json` can carry the mapping.
Set `source` to that ZIP and omit `mapping` to use its embedded copy.

Paths are relative to the server's working directory. Disable edition-specific
pack settings when using `shared`. Source validation finishes before worlds are
opened. The server reads the source at startup. Native
plugins can also edit the active source in memory.

The mapping names the resources to expose to vanilla Java. For example:

```json
{
  "textures": [
    { "bedrock": "textures/blocks/stone.png", "java": "minecraft:block/stone" }
  ],
  "bitmap_fonts": [
    {
      "bedrock": "textures/ui/intro.png",
      "java": "sculith:intro/page",
      "ascent": 7,
      "chars": ["\uE100", "\uE101"]
    }
  ],
  "sounds": [
    { "bedrock": "example.bell", "java": "example:bell", "file": "sounds/bell.ogg" }
  ],
  "models": [
    {
      "bedrock": "example:statue",
      "java": "example:statue",
      "entity": "entity/statue.entity.json",
      "geometry": "models/entity/statue.geo.json",
      "texture": "textures/entity/statue.png",
      "controller": "render_controllers/statue.render_controllers.json"
    }
  ]
}
```

Java clients join with the normal server address and receive a download offer.
The first connection still shows Minecraft's standard resource-pack consent
screen. Java downloads use the Java TCP port, with no additional web server or
client mod. Bedrock receives chunks from the same immutable source snapshot.
Content IDs and hashes change when the corresponding archive changes.

The direct listener currently requires a reachable join host and port. Java SRV
records must resolve to that listener. Proxy mode is rejected for shared delivery;
separate external pack URLs remain available through the edition-specific config.

The supported conversion is explicit PNG texture overrides, Ogg Vorbis sound
events and static cuboid entity geometry with a direct texture and render
controller. Java 26.3 uses pack format 97.1. A mapped player-category sound
event can list clip paths and objects in Bedrock format 1.14.0 or 1.20.20.
Set its mapping's `file` to the first clip, including `.ogg`. Java receives
the same choices, with positive scalar `volume` and `pitch`, positive
integer `weight`, and `stream` when supplied. Repeated entries retain their
own settings and reuse the copied audio file. Numeric ranges and other clip
properties fail validation. [Bedrock sound schema](https://github.com/Mojang/bedrock-schemas/blob/main/schemas/rp/sounds/index.schema.json)
`bitmap_fonts` generates a Java font from a Bedrock PNG divided into the rows
and columns in `chars`. The tile height becomes the glyph height. A Java
dialog text component selects the font using the mapping's Java ID.
Shared form text passed through the Wasm API keeps its top-level font on
Java titles, messages, button captions and labeled inputs.
Explicit texture overrides can also target `minecraft:gui/sprites/dialog/`.
Arbitrary Bedrock UI JSON, behavior packs, scripts, animation and Molang are not
converted. Unsupported mapped features fail validation. Unmapped native assets
remain in the Bedrock archive and are not automatically translated for Java.

Single-pack delivery currently limits the source to 32 MiB, 512 files, 16 levels
of directories and 16 MiB per file. Symlinks, dependencies and subpack selection
are rejected. Files under `crates/sculith-packs/tests/fixtures` are test inputs;
they are never loaded by the default server.

Build native plugins against the API version of the server you deploy. Rust
Wasm plugins can use the shared forms, sounds and models through the stable
v0.1 interface.

## Editing a pack from Rust

Pass a batch of Bedrock file replacements or removals to the native API. The
server checks pack limits and mapped assets, then compiles both packs before
offering the new revision. A missing mapped asset rejects the entire batch.

```rust
use sculith_core::{plugin::Context, resource_pack::PackEdit};

async fn replace_statue(context: &Context, geometry_json: Vec<u8>) -> Result<(), String> {
    context.server.edit_shared_resources(vec![PackEdit::Put {
        path: "models/entity/statue.geo.json".into(),
        bytes: geometry_json,
    }]).await.map_err(|error| error.to_string())?;
    Ok(())
}
```

Edits use paths relative to the Bedrock pack root and apply to the in-memory
source for this server process. Keep authored files or the plugin that produces
them to reproduce changes after restart. Resource identifiers follow the mapping
loaded at startup.

Players reconnect to receive updated assets. A connection already negotiating
its pack finishes with the revision it was offered. The server keeps at most
four revisions in memory for delivery. If active downloads occupy that limit,
retry the edit after one finishes. Use the form, sound and model APIs below for
changes during play with the assets the client has loaded.

## Shared models

`world.spawn-model` creates one tracked server entity with a native Bedrock actor
and a Java item display. The API uses the mapping's Java identifier; the native
identifier can differ. Bedrock receives the actor registry before any spawn.

```rust
use pumpkin_plugin_api::display::EntityDisplayExt;

let entity = player.get_world().spawn_model("example:statue", (0.0, 64.0, 0.0), 1.0)?;
let model = entity.as_model_display().ok_or("Shared model unavailable")?;
model.set_scale(2.0)?;
model.set_position((2.0, 64.0, 0.0))?;
entity.remove();
```

These are static decorative entities without collision or damage. Uniform scale
must be finite, greater than zero and at most 16. Positions must be finite and
within the current world's limits. Dimension transfer and runtime rotation are
not implemented. Spawns honor the cancellable entity spawn event. Removed handles
reject further model changes; Java-only display setters cannot bypass these
shared constraints.

Models persist as standard Java item displays. Their item carries the canonical
identifier in `minecraft:item_model` and `minecraft:custom_data.sculith.model`.
If a mapping is unavailable or the saved transformation is unsupported, Sculith
retains the display data without showing a fallback actor. Restore the mapping
and restart to load its shared representation again.

Model updates and entity removal release the Wasm host borrow while nested
packet events run. The v0.1 calls stay synchronous for plugin callers. The
experimental v0.2 model mutators and `entity.remove` are async; rebuild v0.2
components after updating the interface.

## Plugin sounds

The existing `player.play-custom-sound` and `play-custom-sound-at` methods now
dispatch to both editions. Plugins use the Java identifier from the mapping
(for example `example:bell`); Bedrock receives the authored event name
(`example.bell`). World custom-sound broadcasts use the same mapping.

Named sound stops and stopping all sounds also work on Bedrock. Its wire format
has no category selector: a named stop stops every instance of that event, and a
category-only stop is not sent. Bedrock's authored sound definition determines
its mixer category. Java retains category selection.

Sound calls can produce nested packet events. Both host implementations release
their state borrow while running these calls through the plugin runtime, so a
packet observer can run while another handler from that plugin is active.
The stable Wasm v0.1 contract remains synchronous to plugin callers. The
experimental v0.2 player and world sound methods are now `async func`, consistent
with its other event-producing methods; v0.2 components must be rebuilt. That
host remains behind `plugin-v0_2` and is disabled in default builds.

The Rust fixture under `tests/interop/plugin` verifies dispatch, named stops and
join/packet callbacks on both editions. It is not default server content and
does not establish audible playback or arbitrary sound-definition compatibility.

## Shared forms

`player.open-form` opens the same simple, modal or custom form on either edition.
Rust plugins can use the builders in `pumpkin_plugin_api::forms` and subscribe
to `forms::FormResponseEvent`. This aliases the existing component event named
`bedrock-form-response-event`; its record is also used for Java shared forms.
`FormResponse::parse(event.response_data)` gives the selected index, modal boolean,
custom values or a closed result. The returned form ID identifies the request.

Only one shared form can be pending per player. Requests expire after five
minutes; unknown, malformed, expired and duplicate replies do not fire the
response event. Java actions include a per-request token. The `sculith:ui/`
action prefix and the upper half of Bedrock form IDs are reserved for this API.
The lower half remains available to the edition-specific Bedrock form API.
Closing a modal is distinct from choosing its second button.

On the current Bedrock protocol, a shared form opened from `PlayerJoinEvent`
waits for the client to acknowledge its first spawn. This uses the regular
pending form and its existing expiry. Later shared forms can open immediately.
Native plugins use ABI 43 for this change to the player layout. Build them
against this server revision.

Shared forms support at most 64 buttons or elements, 32 KiB per definition or
reply, and 1024 UTF-16 units per text input. A simple form button can use a
Bedrock texture path when that PNG is mapped under
`minecraft:gui/sprites/dialog/` in `textures`. For example, map the authored
`textures/ui/welcome.png` to `minecraft:gui/sprites/dialog/sculith/welcome`
and pass `path_image("textures/ui/welcome")` to the button builder. Bedrock
uses the authored path. Java 26.3 uses the converted GUI sprite alongside the
button label. Earlier Java versions that support shared forms show the label.
Button images use local pack paths. Shared dialogs require Java 1.21.6 or
later.
Java displays custom-form labels above its input controls, uses cycle buttons
for dropdowns and step sliders, and shows text placeholders as label tooltips.
The layouts differ; the response types and element indices are shared. This API
does not translate arbitrary Bedrock UI JSON or add content to the default server.

The v0.1 method is additive and synchronous to the guest; the host releases its
borrow while packet observers run. The experimental v0.2 method is async.
