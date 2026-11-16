# Shared static model previews

The current source compiles an explicit static Bedrock geometry subset into a
stock Java item model and presents the same registered model through Rust API v5.
Bedrock receives the original client-entity definition, geometry, controller and
PNG. Java 26.3 uses an Item Display Entity and the generated item-model resource.
No client mod is required for this path.

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/models-preview.toml
```

Public [Windows Preview 14](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.14)
bundles all four examples and original assets. New settings enable them; existing
settings are preserved.

Java uses `127.0.0.1:25565`; the native preview uses `127.0.0.1:19132` on the same
machine. The source configuration accepts local offline identities. Append `.exe`
to the model plugin path on Windows. Required pack loading precedes initialization
and presentation. The example displays an original three-cuboid checkerboard
statue three blocks ahead of each initialized player.

## Author and map

The template is in `examples/pack/bedrock`: a format-1.10 client entity, format-1.12
geometry with untransformed bones, a static default controller and an original PNG.
Add a mapping alongside the existing texture/sound entries:

```json
{
  "models": [{
    "bedrock": "sculith:demo_statue",
    "java": "sculith:demo_statue",
    "entity": "entity/sculith_statue.entity.json",
    "geometry": "models/entity/sculith_statue.geo.json",
    "texture": "textures/entity/sculith_statue.png",
    "controller": "render_controllers/sculith_static.render_controllers.json"
  }]
}
```

The canonical API name is the `java` resource identifier. Both model IDs require
custom namespaces. The native renderer explicitly uses the armor-stand runtime,
one `entity_alphatest` material with a fully opaque 8-bit PNG, default texture/geometry bindings and the included
literal static controller. Selected definitions with scripts, animation, render
expressions or additional materials fail conversion. This subset does not parse
arbitrary Molang or behavior packs.

Geometry contains 1–64 uniquely named bones with validated parent references and
no cycles or transforms. Cubes have only `origin`, positive `size` and explicit
per-face `uv`/`uv_size`. Rotation, inflation, mirroring and box-UV shorthand are
rejected. Each model has 1–128 cubes; UV rectangles must fit the exact PNG size.
Generated Java coordinates must stay in `[-16,32]`. Native X/Z pixel origins gain
an eight-pixel centre offset in Java; display Y gains half a block to preserve
native feet Y after item-render centring. The static bone hierarchy is flattened.
The sample's symmetric geometry/UVs are the verified appearance scenario; general
asymmetric per-face orientation has not been established with the official native
renderer.

There are at most 32 mapped models. Existing file/path/source/archive limits still
apply. Model outputs use fresh files and cannot silently overwrite a texture
mapping. Omitted `models` retains older mapping-input compatibility. Native source
files and the authored manifest stay unchanged. Converted textures live under the
Java `item/` atlas directory; writing them under `entity/` reproduces a real-game
missing-texture failure in Java 26.3.

## Rust API and ownership

```rust
Action::ShowModel {
    player_id: player.id,
    model: "sculith:demo_statue".into(),
}
Action::HideModel { player_id: player.id }
```

Each player has one shared private preview slot across trusted plugins. The owner
checks a live initialized session, the immutable model catalog and a one-per-second
show budget. It chooses a point three blocks ahead in the player's horizontal
facing direction and checks conservative region/vertical bounds. Plugins and
clients do not supply arbitrary coordinates. Replacement reuses the current slot's
entity ID/UUID after explicit removal; hide is idempotent. A new show after hide
allocates a fresh entity ID. Player and presentation IDs share one allocator.

The view is cosmetic: no collision, interaction, physics, AI or persistent entity
state is implemented. The position stays fixed until another show; it does not
follow movement. Leaving retires the slot. Presentation changes neither region
revision nor plugin game hooks, and packets go only to the selected session.
This API does not implement other players' avatars or a general entity/world API.

Current SDK negotiation retains versions 1–4 and adds 5. Show/hide require v5;
existing game, form and sound minimums remain 2, 3 and 4 respectively. Plugins are
trusted executable processes with the server user's OS permissions.

## Verification

```sh
python3 scripts/model_smoke.py --client target/gophertunnel-probe
```

Independent Python verifies Java pack assets, the three converted cuboids,
Item Display spawn/metadata, replacement UUID and hide. Go/Pion checks native
actor registration, original geometry and AddActor/RemoveActor layout/lifecycle.
Interleaving two sessions checks that private actor traffic cannot cross recipients.
The Rust example's `--exercise` flag replaces the view after two seconds and hides
it after four, only for automated lifecycle checks; the normal example leaves the
statue visible. Ownership tests reject unknown/pending/retired recipients,
unregistered models, clock/rate/bounds errors, cycles, invalid UV and asset collisions.

The actual unmodified Java game has rendered the textured statue after correction
of the atlas path. Final exact-source captures and native Windows packaging are
recorded in [the verification record](verification/2026-11-16-shared-model.md).
Official Windows Bedrock-game rendering remains unverified; native wire/asset
tests are separate evidence.
