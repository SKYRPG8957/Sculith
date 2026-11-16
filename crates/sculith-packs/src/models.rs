//! Explicit static cuboids with one texture and untransformed bone hierarchy.
use crate::{MAX_TOTAL_BYTES, ModelMapping, confined, read_bounded, read_json};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Write},
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeometryFile {
    format_version: String,
    #[serde(rename = "minecraft:geometry")]
    geometries: Vec<Geometry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Geometry {
    description: Description,
    bones: Vec<Bone>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Description {
    identifier: String,
    texture_width: u32,
    texture_height: u32,
    visible_bounds_width: Option<f32>,
    visible_bounds_height: Option<f32>,
    visible_bounds_offset: Option<[f32; 3]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bone {
    name: String,
    parent: Option<String>,
    #[serde(default)]
    cubes: Vec<Cube>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cube {
    origin: [f32; 3],
    size: [f32; 3],
    uv: BTreeMap<String, Face>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Face {
    uv: [f32; 2],
    uv_size: [f32; 2],
}

pub(super) fn convert(
    root: &Path,
    mappings: &[ModelMapping],
    output: &Path,
    total: &mut usize,
) -> Result<()> {
    ensure!(mappings.len() <= 32, "at most 32 static models");
    let mut native = BTreeSet::new();
    let mut java = BTreeSet::new();
    for entry in mappings {
        for id in [&entry.bedrock, &entry.java] {
            sculith_core::validate_resource_id(id).map_err(anyhow::Error::msg)?;
        }
        ensure!(
            !entry.bedrock.starts_with("minecraft:") && !entry.java.starts_with("minecraft:"),
            "model identifiers must use a custom namespace"
        );
        ensure!(
            native.insert(&entry.bedrock) && java.insert(&entry.java),
            "duplicate model identifier"
        );
        for (name, prefix, suffix) in [
            (&entry.entity, "entity/", ".json"),
            (&entry.geometry, "models/entity/", ".json"),
            (&entry.texture, "textures/", ".png"),
            (&entry.controller, "render_controllers/", ".json"),
        ] {
            ensure!(
                name.starts_with(prefix) && name.ends_with(suffix),
                "invalid model asset path"
            );
        }
        let geometry: GeometryFile = read_json(&confined(root, &entry.geometry)?)?;
        ensure!(
            geometry.format_version == "1.12.0" && geometry.geometries.len() == 1,
            "static models require one format-1.12 geometry"
        );
        let geometry = geometry
            .geometries
            .into_iter()
            .next()
            .expect("one geometry");
        let description = &geometry.description;
        ensure!(
            description.identifier.starts_with("geometry.")
                && (10..=128).contains(&description.identifier.len())
                && description
                    .identifier
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)),
            "invalid geometry identifier"
        );
        for bound in [
            description.visible_bounds_width,
            description.visible_bounds_height,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                bound.is_finite() && (0.0..=16.0).contains(&bound),
                "invalid visibility bound"
            );
        }
        if let Some(offset) = description.visible_bounds_offset {
            ensure!(
                offset.iter().all(|v| v.is_finite() && v.abs() <= 16.0),
                "invalid visibility offset"
            );
        }
        let controller: serde_json::Value = read_json(&confined(root, &entry.controller)?)?;
        let expected_controller = serde_json::json!({"format_version":"1.8.0","render_controllers":{"controller.render.sculith_static":{"geometry":"Geometry.default","materials":[{"*":"Material.default"}],"textures":["Texture.default"]}}});
        ensure!(
            controller == expected_controller,
            "only the static default geometry/material/texture controller is supported"
        );
        let entity: serde_json::Value = read_json(&confined(root, &entry.entity)?)?;
        let texture = entry
            .texture
            .strip_suffix(".png")
            .context("PNG texture required")?;
        let expected_entity = serde_json::json!({"format_version":"1.10.0","minecraft:client_entity":{"description":{"identifier":entry.bedrock,"runtime_identifier":"minecraft:armor_stand","materials":{"default":"entity_alphatest"},"textures":{"default":texture},"geometry":{"default":description.identifier},"render_controllers":["controller.render.sculith_static"]}}});
        ensure!(
            entity == expected_entity,
            "client entity must be the explicit static armor-stand runtime renderer without scripts/animations"
        );
        let bytes = read_bounded(&confined(root, &entry.texture)?, 16 * 1_048_576)?;
        let (width, height) = opaque_texture(&bytes)?;
        ensure!(
            description.texture_width == width && description.texture_height == height,
            "geometry and PNG texture dimensions differ"
        );
        *total += bytes.len();
        ensure!(
            *total <= MAX_TOTAL_BYTES,
            "converted content exceeds 256 MiB"
        );
        let elements = elements(&geometry)?;
        let (namespace, name) = entry
            .java
            .split_once(':')
            .expect("validated model identifier");
        let texture_id = format!("{namespace}:item/sculith_models/{name}");
        let model_id = format!("{namespace}:sculith/{name}");
        for (relative, data) in [
            (
                format!("assets/{namespace}/textures/item/sculith_models/{name}.png"),
                bytes,
            ),
            (
                format!("assets/{namespace}/models/sculith/{name}.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"ambientocclusion":false,"textures":{"main":texture_id,"particle":texture_id},"elements":elements}),
                )?,
            ),
            (
                format!("assets/{namespace}/items/{name}.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"model":{"type":"minecraft:model","model":model_id}}),
                )?,
            ),
        ] {
            let path = output.join(relative);
            fs::create_dir_all(path.parent().context("model asset parent")?)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("duplicate generated model asset {}", path.display()))?
                .write_all(&data)?;
        }
    }
    Ok(())
}

/// Opaque 8-bit textures avoid silently changing native/Java material semantics.
fn opaque_texture(bytes: &[u8]) -> Result<(u32, u32)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info()?;
    let (width, height) = (reader.info().width, reader.info().height);
    ensure!(
        width <= 4096 && height <= 4096,
        "model texture exceeds 4096x4096"
    );
    let size = reader.output_buffer_size().context("PNG output overflow")?;
    ensure!(size <= 64 * 1_048_576, "decoded model PNG exceeds 64 MiB");
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer)?;
    ensure!(
        info.bit_depth == png::BitDepth::Eight,
        "model texture requires 8-bit channels"
    );
    let channels = match info.color_type {
        png::ColorType::Rgba => Some(4),
        png::ColorType::GrayscaleAlpha => Some(2),
        png::ColorType::Rgb | png::ColorType::Grayscale => None,
        _ => anyhow::bail!("model PNG palette expansion failed"),
    };
    if let Some(channels) = channels {
        ensure!(
            buffer[..info.buffer_size()]
                .chunks_exact(channels)
                .all(|pixel| pixel[channels - 1] == 255),
            "translucent model textures are unsupported by the opaque preview"
        );
    }
    ensure!(
        reader.info().animation_control.is_none(),
        "animated model textures are unsupported"
    );
    reader.finish()?;
    Ok((width, height))
}

fn elements(geometry: &Geometry) -> Result<Vec<serde_json::Value>> {
    ensure!(
        !geometry.bones.is_empty() && geometry.bones.len() <= 64,
        "models need 1..64 static bones"
    );
    let mut names = BTreeMap::new();
    for bone in &geometry.bones {
        ensure!(
            !bone.name.is_empty()
                && bone.name.len() <= 64
                && bone
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid bone name"
        );
        ensure!(
            names.insert(&bone.name, bone).is_none(),
            "duplicate bone name"
        );
    }
    for bone in &geometry.bones {
        let mut visiting = BTreeSet::new();
        let mut current = bone;
        while let Some(parent) = &current.parent {
            ensure!(visiting.insert(&current.name), "cyclic bone hierarchy");
            current = names.get(parent).context("unknown parent bone")?;
        }
    }
    let mut result = Vec::new();
    let d = &geometry.description;
    for cube in geometry.bones.iter().flat_map(|b| &b.cubes) {
        ensure!(result.len() < 128, "at most 128 cuboids per model");
        let from = [cube.origin[0] + 8.0, cube.origin[1], cube.origin[2] + 8.0];
        let to = std::array::from_fn::<_, 3, _>(|i| from[i] + cube.size[i]);
        ensure!(
            cube.size.iter().all(|v| v.is_finite() && *v > 0.0)
                && from
                    .iter()
                    .chain(&to)
                    .all(|v| v.is_finite() && (-16.0..=32.0).contains(v)),
            "cuboid exceeds vanilla Java coordinate bounds"
        );
        ensure!(
            !cube.uv.is_empty() && cube.uv.len() <= 6,
            "cuboids need explicit per-face UVs"
        );
        let mut faces = BTreeMap::new();
        for (name, face) in &cube.uv {
            ensure!(
                ["north", "south", "west", "east", "up", "down"].contains(&name.as_str()),
                "unknown cuboid face"
            );
            let end = [face.uv[0] + face.uv_size[0], face.uv[1] + face.uv_size[1]];
            ensure!(
                face.uv_size.iter().all(|v| v.is_finite() && *v > 0.0)
                    && face
                        .uv
                        .iter()
                        .chain(&end)
                        .all(|v| v.is_finite() && *v >= 0.0)
                    && end[0] <= d.texture_width as f32
                    && end[1] <= d.texture_height as f32,
                "UV exceeds texture bounds"
            );
            let uv = [
                face.uv[0] * 16.0 / d.texture_width as f32,
                face.uv[1] * 16.0 / d.texture_height as f32,
                end[0] * 16.0 / d.texture_width as f32,
                end[1] * 16.0 / d.texture_height as f32,
            ];
            faces.insert(name, serde_json::json!({"uv":uv,"texture":"#main"}));
        }
        result.push(serde_json::json!({"from":from,"to":to,"faces":faces}));
    }
    ensure!(!result.is_empty(), "model contains no cuboids");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/pack/bedrock")
            .canonicalize()
            .unwrap()
    }
    #[test]
    fn model_material_requires_opaque_pixels_including_expanded_alpha() {
        for alpha in [255u8, 128] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                writer.write_image_data(&[10, 20, 30, alpha]).unwrap();
            }
            assert_eq!(opaque_texture(&bytes).is_ok(), alpha == 255);
        }
    }

    #[test]
    fn a_texture_mapping_cannot_be_silently_overwritten_by_a_generated_model_texture() {
        let root = source();
        let mut mapping: serde_json::Value =
            read_json(&root.parent().unwrap().join("textures.json")).unwrap();
        mapping["textures"].as_array_mut().unwrap().push(serde_json::json!({"bedrock":"textures/blocks/sculith_demo.png","java":"sculith:item/sculith_models/demo_statue"}));
        let dir = tempfile::tempdir().unwrap();
        let map = dir.path().join("mapping.json");
        let output = dir.path().join("converted");
        fs::write(&map, serde_json::to_vec(&mapping).unwrap()).unwrap();
        assert!(crate::convert(&root, &map, &output, 97, 1).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn converts_original_three_cuboid_bone_hierarchy_and_exact_texture() {
        let root = source();
        let map: crate::TextureMap =
            read_json(&root.parent().unwrap().join("textures.json")).unwrap();
        let out = tempfile::tempdir().unwrap();
        convert(&root, &map.models, out.path(), &mut 0).unwrap();
        let value: serde_json::Value = read_json(
            &out.path()
                .join("assets/sculith/models/sculith/demo_statue.json"),
        )
        .unwrap();
        assert_eq!(value["elements"].as_array().unwrap().len(), 3);
        assert_eq!(
            value["elements"][0]["from"],
            serde_json::json!([0.0, 0.0, 4.0])
        );
        assert_eq!(
            fs::read(root.join("textures/entity/sculith_statue.png")).unwrap(),
            fs::read(
                out.path()
                    .join("assets/sculith/textures/item/sculith_models/demo_statue.png")
            )
            .unwrap()
        );
    }
    #[test]
    fn rejects_rotation_cycles_and_invalid_uv() {
        let raw = fs::read(source().join("models/entity/sculith_statue.geo.json")).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        value["minecraft:geometry"][0]["bones"][1]["rotation"] = serde_json::json!([0, 90, 0]);
        assert!(serde_json::from_value::<GeometryFile>(value).is_err());
        let mut parsed: GeometryFile = serde_json::from_slice(&raw).unwrap();
        parsed.geometries[0].bones[0].parent = Some("head".into());
        assert!(elements(&parsed.geometries[0]).is_err());
        let mut parsed: GeometryFile = serde_json::from_slice(&raw).unwrap();
        parsed.geometries[0].bones[1].cubes[0]
            .uv
            .get_mut("north")
            .unwrap()
            .uv_size = [17.0, 16.0];
        assert!(elements(&parsed.geometries[0]).is_err());
    }
}
