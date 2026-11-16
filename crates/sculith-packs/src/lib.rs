//! Explicit Bedrock PNG -> vanilla Java texture overrides.
//! Explicit single-clip Vorbis sounds are shared; models and behaviors remain separate.

pub mod bundle;
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
    #[serde(default)]
    pub textures: Vec<TextureMapping>,
    #[serde(default)]
    pub sounds: Vec<SoundMapping>,
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
    /// Existing native event in sounds/sound_definitions.json.
    pub bedrock: String,
    /// Canonical API name and Java event, e.g. sculith:demo_tone.
    pub java: String,
    /// The exact single Vorbis clip used by the native event.
    pub file: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConversionReport {
    pub bedrock_pack_uuid: String,
    pub bedrock_pack_version: [u32; 3],
    pub java_pack_format: u32,
    pub java_pack_format_minor: u32,
    pub converted_textures: usize,
    pub converted_sounds: Vec<SoundMapping>,
    pub scope: String,
    pub limitations: Vec<String>,
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data = read_bounded(path, MAX_JSON_BYTES)?;
    serde_json::from_slice(&data).with_context(|| format!("invalid JSON in {}", path.display()))
}

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
        !mapping.textures.is_empty() || !mapping.sounds.is_empty(),
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
            entry.bedrock.starts_with("textures/") && entry.bedrock.ends_with(".png"),
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

    sounds::convert(&root, &mapping.sounds, stage.path(), &mut total_bytes)?;

    let report = ConversionReport {
        bedrock_pack_uuid: manifest.header.uuid,
        bedrock_pack_version: manifest.header.version,
        java_pack_format: pack_format,
        java_pack_format_minor: pack_format_minor,
        converted_textures: mapping.textures.len(),
        converted_sounds: mapping.sounds,
        scope: "explicit PNG overrides and single-clip Vorbis sound events".into(),
        limitations: vec![
            "Only explicitly mapped PNG and OGG files are converted; unmapped assets are not included.".into(),
            "PNG targets must already exist in the target client; custom sound events are registered by the pack.".into(),
            "Models, animations, UI, fonts, materials and behavior/script packs are not converted; weighted/streaming sounds are unsupported.".into(),
            "Texture conversion does not register custom blocks/items; archive delivery is configured separately.".into(),
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
        texture.starts_with("block/") || texture.starts_with("item/"),
        "only Java block/ and item/ textures are supported"
    );
    ensure!(
        !texture.ends_with(".png"),
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
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[80, 160, 80]).unwrap();
        }
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
