#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Explicit Bedrock textures, sounds and static models for vanilla Java.
//! Selected content is compiled explicitly; behaviors and animation remain separate.

pub mod bundle;
mod models;
mod sounds;

use std::{
    collections::HashSet,
    fs,
    io::{Cursor, Read},
    path::{Component, Path},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const MAX_JSON_BYTES: u64 = 1_048_576;
const MAX_TEXTURE_BYTES: u64 = 16 * 1_048_576;
const MAX_TOTAL_BYTES: usize = 256 * 1_048_576;
const MAX_TEXTURES: usize = 256;

#[derive(Debug, Deserialize)]
pub struct BedrockManifest {
    pub format_version: u32,
    pub header: Header,
    pub modules: Vec<Module>,
}

#[derive(Debug, Deserialize)]
pub struct Header {
    pub name: String,
    pub uuid: String,
    pub version: [u32; 3],
}

#[derive(Debug, Deserialize)]
pub struct Module {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextureMap {
    /// Carry the authored Bedrock action-form layout for Sculith Companion.
    #[serde(default)]
    pub companion_forms: bool,
    /// Bitmap fonts compiled from an authored form. Each name selects sculith/fonts/<name>.json.
    #[serde(default)]
    pub form_fonts: Vec<String>,
    #[serde(default)]
    pub textures: Vec<TextureMapping>,
    #[serde(default)]
    pub sounds: Vec<SoundMapping>,
    #[serde(default)]
    pub models: Vec<ModelMapping>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextureMapping {
    /// Relative PNG path in the Bedrock pack, e.g. textures/blocks/stone.png.
    pub bedrock: String,
    /// Java resource identifier without .png, e.g. minecraft:block/stone.
    pub java: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundMapping {
    /// Existing native event in `sounds/sound_definitions.json`.
    pub bedrock: String,
    /// Canonical API name and Java event, e.g. `sculith:demo_tone`.
    pub java: String,
    /// The exact single Vorbis clip used by the native event.
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMapping {
    pub bedrock: String,
    pub java: String,
    pub entity: String,
    pub geometry: String,
    pub texture: String,
    pub controller: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConversionReport {
    pub bedrock_pack_uuid: String,
    pub bedrock_pack_version: [u32; 3],
    pub java_pack_format: u32,
    pub java_pack_format_minor: u32,
    #[serde(default)]
    pub companion_forms: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form_fonts: Vec<String>,
    pub converted_textures: usize,
    pub converted_sounds: Vec<SoundMapping>,
    pub converted_models: Vec<ModelMapping>,
    pub scope: String,
    pub limitations: Vec<String>,
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data = read_bounded(path, MAX_JSON_BYTES)?;
    serde_json::from_slice(&data).with_context(|| format!("invalid JSON in {}", path.display()))
}

#[expect(clippy::too_many_lines, reason = "one bounded conversion transaction")]
pub fn convert(
    input: &Path,
    mapping: &Path,
    output: &Path,
    pack_format: u32,
    pack_format_minor: u32,
) -> Result<ConversionReport> {
    ensure!(pack_format > 0, "Java pack format must be positive");
    let root = input
        .canonicalize()
        .context("Bedrock pack directory is missing")?;
    ensure!(root.is_dir(), "Bedrock pack input must be a directory");
    ensure!(
        !output.try_exists()?,
        "output already exists; choose a new directory"
    );
    let name = output
        .file_name()
        .context("output must name a new directory")?;
    ensure!(
        matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        ),
        "invalid output name"
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent
        .canonicalize()
        .context("output parent directory must exist")?;
    ensure!(
        !parent.starts_with(&root),
        "output must be outside the source pack"
    );
    let destination = parent.join(name);
    let manifest_path = confined(&root, "manifest.json")?;
    let manifest: BedrockManifest = read_json(&manifest_path)?;
    ensure!(
        manifest.format_version == 2,
        "only Bedrock manifest format_version 2 is supported"
    );
    uuid::Uuid::parse_str(&manifest.header.uuid).context("invalid Bedrock header UUID")?;
    ensure!(
        !manifest.modules.is_empty(),
        "Bedrock resource pack needs a resources module"
    );
    ensure!(
        manifest.modules.iter().all(|m| m.kind == "resources"),
        "only resource packs can be converted; behavior/script modules are unsupported"
    );
    let mapping: TextureMap = read_json(mapping)?;
    ensure!(
        mapping.companion_forms
            || !mapping.form_fonts.is_empty()
            || !mapping.textures.is_empty()
            || !mapping.sounds.is_empty()
            || !mapping.models.is_empty(),
        "content mapping is empty"
    );
    ensure!(
        mapping.textures.len() <= MAX_TEXTURES,
        "at most {MAX_TEXTURES} textures per conversion"
    );

    // All writes are staged. Errors leave both the original pack and output untouched.
    let stage = tempfile::Builder::new()
        .prefix(".sculith-pack-")
        .tempdir_in(&parent)?;
    let mut targets = HashSet::new();
    let mut total_bytes = 0;
    for entry in &mapping.textures {
        let target = java_texture_path(&entry.java)?;
        ensure!(
            targets.insert(target.clone()),
            "duplicate Java target: {}",
            entry.java
        );
        ensure!(
            entry.bedrock.starts_with("textures/")
                && Path::new(&entry.bedrock)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("png")),
            "Bedrock textures must be PNG files under textures/"
        );
        let source = confined(&root, &entry.bedrock)?;
        let bytes = read_bounded(&source, MAX_TEXTURE_BYTES)?;
        total_bytes += bytes.len();
        ensure!(
            total_bytes <= MAX_TOTAL_BYTES,
            "texture pack exceeds 256 MiB"
        );
        validate_png(&bytes).with_context(|| format!("invalid PNG: {}", entry.bedrock))?;
        let file = stage.path().join(target);
        fs::create_dir_all(file.parent().context("missing texture parent")?)?;
        fs::write(file, bytes)?;
    }

    if mapping.companion_forms {
        let source = confined(&root, "ui/server_form.json")?;
        let bytes = read_bounded(&source, 128 * 1024)?;
        let form: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid Bedrock server_form.json")?;
        ensure!(
            form.get("namespace").and_then(serde_json::Value::as_str) == Some("server_form"),
            "expected the Bedrock server_form namespace"
        );
        let directory = stage.path().join("assets/sculith/bedrock_ui");
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("server_form.json"), bytes)?;
    }

    ensure!(mapping.form_fonts.len() <= 32, "too many form fonts");
    for name in &mapping.form_fonts {
        install_form_font(&root, stage.path(), name, &mut targets)?;
    }
    sounds::convert(&root, &mapping.sounds, stage.path(), &mut total_bytes)?;

    models::convert(&root, &mapping.models, stage.path(), &mut total_bytes)?;
    let report = ConversionReport {
        bedrock_pack_uuid: manifest.header.uuid,
        bedrock_pack_version: manifest.header.version,
        java_pack_format: pack_format,
        java_pack_format_minor: pack_format_minor,
        companion_forms: mapping.companion_forms,
        converted_textures: mapping.textures.len(),
        converted_sounds: mapping.sounds,
        converted_models: mapping.models,
        scope: if !mapping.form_fonts.is_empty() {
            "mapped textures, bitmap form fonts, sounds and static models for stock Java"
        } else if mapping.companion_forms {
            "mapped textures, sounds, static models and an authored Bedrock form for Sculith Companion"
        } else {
            "explicit PNG overrides, single-clip Vorbis sounds and static cuboid models"
        }.into(),
        form_fonts: mapping.form_fonts,
        limitations: vec![
            "Only explicitly mapped PNG, OGG and static geometry definitions are converted; unmapped assets are not included.".into(),
            "PNG overrides target existing textures; custom sound events and presentation-only item models are registered by the pack.".into(),
            "Form fonts support bitmap and spacing providers. Arbitrary UI, animated geometry, custom materials, behavior packs and weighted or streaming sounds are unsupported.".into(),
            "Asset conversion does not register new gameplay blocks/items; archive delivery is configured separately.".into(),
        ],
    };
    let metadata = serde_json::json!({"pack": {
        "description": format!("{} — Sculith content conversion", manifest.header.name),
        "pack_format": pack_format,
        "min_format": [pack_format, pack_format_minor],
        "max_format": [pack_format, pack_format_minor]
    }});
    fs::write(
        stage.path().join("pack.mcmeta"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    fs::write(
        stage.path().join("sculith-conversion.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    fs::rename(stage.path(), destination)
        .context("could not move completed Java pack into output")?;
    Ok(report)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "source must be a regular file: {}",
        path.display()
    );
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "file exceeds {limit} bytes: {}",
        path.display()
    );
    Ok(data)
}

fn confined(root: &Path, relative: &str) -> Result<std::path::PathBuf> {
    ensure!(
        !relative.contains('\\') && !relative.is_empty(),
        "invalid source path"
    );
    ensure!(
        Path::new(relative)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "source paths must be relative and cannot contain '..'"
    );
    let path = root
        .join(relative)
        .canonicalize()
        .with_context(|| format!("source missing: {relative}"))?;
    ensure!(
        path.starts_with(root),
        "source symlink escapes the Bedrock pack"
    );
    Ok(path)
}

fn java_texture_path(id: &str) -> Result<String> {
    let (namespace, texture) = id
        .split_once(':')
        .context("Java texture must be namespace:block/name or namespace:item/name")?;
    ensure!(
        !namespace.is_empty()
            && namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)),
        "invalid Java namespace"
    );
    ensure!(
        texture.starts_with("block/")
            || texture.starts_with("item/")
            || (namespace == "sculith" && texture.starts_with("ui/")),
        "expected a block/, item/ or sculith:ui/ texture"
    );
    ensure!(
        !Path::new(texture)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png")),
        "Java resource IDs must omit .png"
    );
    ensure!(
        texture
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_/.-".contains(&b)),
        "invalid Java texture ID"
    );
    if texture
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("invalid Java texture path");
    }
    Ok(format!("assets/{namespace}/textures/{texture}.png"))
}

fn install_form_font(
    root: &Path,
    stage: &Path,
    name: &str,
    targets: &mut HashSet<String>,
) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_'),
        "invalid form font name"
    );
    let target = format!("assets/sculith/font/{name}.json");
    ensure!(
        targets.insert(target.clone()),
        "duplicate form font: {name}"
    );
    let source = confined(root, &format!("sculith/fonts/{name}.json"))?;
    let bytes = read_bounded(&source, 128 * 1024)?;
    let font: serde_json::Value = serde_json::from_slice(&bytes)?;
    let providers = font["providers"]
        .as_array()
        .context("form font needs providers")?;
    ensure!(
        !providers.is_empty() && providers.len() <= 128,
        "invalid font provider count"
    );
    for provider in providers {
        match provider["type"].as_str() {
            Some("bitmap") => {
                let file = provider["file"]
                    .as_str()
                    .and_then(|id| id.strip_suffix(".png"))
                    .context("bitmap font provider needs a PNG")?;
                ensure!(
                    file.starts_with("sculith:ui/") && targets.contains(&java_texture_path(file)?),
                    "form font references an unmapped UI texture: {file}"
                );
            }
            Some("space") => {}
            _ => bail!("unsupported form font provider"),
        }
    }
    fs::create_dir_all(stage.join("assets/sculith/font"))?;
    fs::write(stage.join(target), bytes)?;
    Ok(())
}

fn validate_png(bytes: &[u8]) -> Result<()> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info()?;
    ensure!(
        reader.info().width <= 4096 && reader.info().height <= 4096,
        "PNG dimensions exceed 4096x4096"
    );
    let size = reader
        .output_buffer_size()
        .context("PNG output size overflow")?;
    ensure!(size <= 64 * 1_048_576, "decoded PNG exceeds 64 MiB");
    reader.next_frame(&mut vec![0; size])?;
    ensure!(
        reader.info().animation_control.is_none(),
        "APNG animation is unsupported"
    );
    reader.finish()?;
    Ok(())
}

fn validate_resource_id(id: &str) -> Result<(), String> {
    let valid = || {
        let (namespace, path) = id.split_once(':')?;
        if id.len() > 128
            || namespace.is_empty()
            || namespace == "."
            || namespace == ".."
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
            || !path
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_/.-".contains(&b))
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return None;
        }
        Some(())
    };
    valid().ok_or_else(|| {
        "resource ID requires namespace:path, at most 128 bytes without traversal".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bedrock");
        fs::create_dir_all(root.join("textures/blocks")).unwrap();
        fs::write(root.join("manifest.json"), br#"{
            "format_version":2,"header":{"name":"test","uuid":"0c10d423-a80a-49af-bc03-854b613ab2d7","version":[1,0,0]},
            "modules":[{"type":"resources"}]
        }"#).unwrap();
        let bytes = {
            let mut bytes = Vec::new();
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[80, 160, 80]).unwrap();
            writer.finish().unwrap();
            bytes
        };
        fs::write(root.join("textures/blocks/stone.png"), bytes).unwrap();
        fs::write(dir.path().join("map.json"), br#"{"textures":[{"bedrock":"textures/blocks/stone.png","java":"minecraft:block/stone"}]}"#).unwrap();
        dir
    }

    fn run(dir: &tempfile::TempDir) -> Result<ConversionReport> {
        convert(
            &dir.path().join("bedrock"),
            &dir.path().join("map.json"),
            &dir.path().join("java"),
            99,
            0,
        )
    }

    #[test]
    fn opted_in_companion_receives_the_native_layout_bytes() {
        let dir = fixture();
        let ui = dir.path().join("bedrock/ui");
        fs::create_dir(&ui).unwrap();
        let source =
            br#"{ "namespace": "server_form", "offer": {"type":"panel", "size":[73,24]} }"#;
        fs::write(ui.join("server_form.json"), source).unwrap();
        fs::write(dir.path().join("map.json"), br#"{"companion_forms":true}"#).unwrap();
        assert!(run(&dir).unwrap().companion_forms);
        assert_eq!(
            fs::read(
                dir.path()
                    .join("java/assets/sculith/bedrock_ui/server_form.json")
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn converts_png_and_writes_format_and_explicit_limitations() {
        let dir = fixture();
        let report = run(&dir).unwrap();
        assert_eq!(report.converted_textures, 1);
        assert_eq!(
            fs::read(dir.path().join("bedrock/textures/blocks/stone.png")).unwrap(),
            fs::read(
                dir.path()
                    .join("java/assets/minecraft/textures/block/stone.png")
            )
            .unwrap()
        );
        let meta: serde_json::Value = read_json(&dir.path().join("java/pack.mcmeta")).unwrap();
        assert_eq!(meta["pack"]["min_format"], serde_json::json!([99, 0]));
        assert_eq!(meta["pack"]["pack_format"], 99);
        assert!(report.limitations.len() >= 4);
        assert!(run(&dir).is_err(), "existing packs must not be overwritten");
    }

    #[test]
    fn serves_the_mapped_form_font_and_requires_its_atlas() {
        let dir = fixture();
        let root = dir.path().join("bedrock");
        fs::create_dir_all(root.join("sculith/fonts")).unwrap();
        let font = br#"{"providers":[{"type":"bitmap","file":"sculith:ui/row.png","ascent":1,"chars":["x"]}]}"#;
        fs::write(root.join("sculith/fonts/guild.json"), font).unwrap();
        let mapping = dir.path().join("map.json");
        let mut data = serde_json::json!({"form_fonts":["guild"],"textures":[
            {"bedrock":"textures/blocks/stone.png","java":"sculith:ui/other"}]});
        fs::write(&mapping, data.to_string()).unwrap();
        assert!(run(&dir).is_err());
        assert!(!dir.path().join("java").exists());
        data["textures"][0]["java"] = serde_json::json!("sculith:ui/row");
        fs::write(mapping, data.to_string()).unwrap();
        assert_eq!(run(&dir).unwrap().form_fonts, ["guild"]);
        assert_eq!(
            fs::read(dir.path().join("java/assets/sculith/font/guild.json")).unwrap(),
            font
        );
    }

    #[test]
    fn rejects_malformed_png_without_partial_output() {
        let dir = fixture();
        fs::write(
            dir.path().join("bedrock/textures/blocks/stone.png"),
            b"not a png",
        )
        .unwrap();
        assert!(run(&dir).is_err());
        assert!(!dir.path().join("java").exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn rejects_behavior_pack_and_duplicate_targets() {
        let dir = fixture();
        fs::write(
            dir.path().join("map.json"),
            br#"{"textures":[
            {"bedrock":"textures/blocks/stone.png","java":"minecraft:block/stone"},
            {"bedrock":"textures/blocks/stone.png","java":"minecraft:block/stone"}] }"#,
        )
        .unwrap();
        assert!(run(&dir).unwrap_err().to_string().contains("duplicate"));
        fs::write(dir.path().join("bedrock/manifest.json"), br#"{
            "format_version":2,"header":{"name":"test","uuid":"0c10d423-a80a-49af-bc03-854b613ab2d7","version":[1,0,0]},
            "modules":[{"type":"data"}]}"#).unwrap();
        assert!(
            run(&dir)
                .unwrap_err()
                .to_string()
                .contains("behavior/script")
        );
    }

    #[test]
    fn rejects_source_and_target_traversal() {
        let dir = fixture();
        let root = dir.path().join("bedrock").canonicalize().unwrap();
        for source in [
            "../map.json",
            "/etc/passwd",
            "textures/../../map.json",
            "textures\\stone.png",
        ] {
            assert!(confined(&root, source).is_err());
        }
        for id in [
            "minecraft:block/../../pack",
            "../minecraft:block/stone",
            "minecraft:item//x",
            "minecraft:block/x.png",
            "minecraft:block/",
            "minecraft:block/STONE",
        ] {
            assert!(java_texture_path(id).is_err(), "{id}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_source_symlink_outside_pack() {
        let dir = fixture();
        let source = dir.path().join("bedrock/textures/blocks/stone.png");
        fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink(dir.path().join("map.json"), source).unwrap();
        assert!(
            run(&dir)
                .unwrap_err()
                .to_string()
                .contains("symlink escapes")
        );
    }
}
