# Shared resources

The fork can compile one authored Bedrock resource pack for both editions at
startup. No pack or server-specific content is enabled by default.

In `pumpkin.toml`:

```toml
[resource_pack.shared]
source = "resource_packs/authored"
mapping = "resource_packs/mapping.json"
required = true
```

Paths are relative to the server's working directory. Disable edition-specific
pack settings when using `shared`. Source validation finishes before worlds are
opened. Restart the server after editing resources; existing sessions never read
partly changed source files.

The mapping names the resources to expose to vanilla Java. For example:

```json
{
  "textures": [
    { "bedrock": "textures/blocks/stone.png", "java": "minecraft:block/stone" }
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

The supported conversion is explicit PNG texture overrides, single-clip Ogg Vorbis
sounds, and static cuboid entity geometry with a direct texture and render
controller. Java 26.3 uses pack format 97.1. These are converted assets: registering
an asset alone does not spawn a model, play a sound, or create a plugin interface.
Arbitrary Bedrock UI JSON, behavior packs, scripts, animation and Molang are not
converted. Unsupported mapped features fail validation. Unmapped native assets
remain in the Bedrock archive and are not automatically translated for Java.

Single-pack delivery currently limits the source to 32 MiB, 512 files, 16 levels
of directories and 16 MiB per file. Symlinks, dependencies and subpack selection
are rejected. Files under `crates/sculith-packs/tests/fixtures` are test inputs;
they are never loaded by the default server.

Native plugin ABI is 8 after adding shared form sessions to players. Rebuild native
plugins against this branch. The Wasm v0.1 interface is unchanged by pack delivery.

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

Shared forms support at most 64 buttons or elements, 32 KiB per definition or
reply, and 1024 UTF-16 units per text input. Button images are rejected because
vanilla Java dialogs cannot display them. The projection is verified on Java
26.3; versions before 1.21.6 do not support dialogs.
Java displays custom-form labels above its input controls, uses cycle buttons
for dropdowns and step sliders, and shows text placeholders as label tooltips.
The layouts differ; the response types and element indices are shared. This API
does not translate arbitrary Bedrock UI JSON or add content to the default server.

The v0.1 method is additive and synchronous to the guest; the host releases its
borrow while packet observers run. The experimental v0.2 method is async.
