//! Bounded sound conversion. Ogg framing, CRC and identification are
//! checked here; clients still perform the actual Vorbis setup/audio decode.
use crate::{MAX_TOTAL_BYTES, SoundMapping, confined, read_bounded, read_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs,
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definitions {
    format_version: String,
    sound_definitions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeEvent<'a> {
    category: &'a str,
    #[serde(borrow)]
    sounds: Vec<NativeClip<'a>>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(untagged)]
enum NativeClip<'a> {
    Path(&'a str),
    Options(#[serde(borrow)] ClipOptions<'a>),
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipOptions<'a> {
    name: &'a str,
    #[serde(default)]
    stream: bool,
    volume: Option<f32>,
    pitch: Option<f32>,
    weight: Option<i32>,
}

impl<'a> NativeClip<'a> {
    const fn options(self) -> ClipOptions<'a> {
        match self {
            Self::Path(name) => ClipOptions {
                name,
                stream: false,
                volume: None,
                pitch: None,
                weight: None,
            },
            Self::Options(clip) => clip,
        }
    }
}

#[derive(Serialize)]
struct JavaEvent {
    sounds: Vec<JavaClip>,
}

#[derive(Serialize)]
struct JavaClip {
    name: String,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pitch: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weight: Option<i32>,
}

pub fn convert(
    root: &Path,
    mappings: &[SoundMapping],
    output: &Path,
    total: &mut usize,
) -> Result<()> {
    ensure!(mappings.len() <= 64, "at most 64 sound events");
    if mappings.is_empty() {
        return Ok(());
    }
    let definitions: Definitions = read_json(&confined(root, "sounds/sound_definitions.json")?)?;
    ensure!(
        matches!(definitions.format_version.as_str(), "1.14.0" | "1.20.20"),
        "unsupported sound definition format"
    );
    let mut native = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut namespaces: BTreeMap<&str, BTreeMap<&str, JavaEvent>> = BTreeMap::new();
    for entry in mappings {
        crate::validate_resource_id(&entry.java).map_err(anyhow::Error::msg)?;
        ensure!(
            !entry.bedrock.is_empty()
                && entry.bedrock.len() <= 128
                && entry
                    .bedrock
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)),
            "invalid native sound event"
        );
        ensure!(
            native.insert(&entry.bedrock) && targets.insert(&entry.java),
            "duplicate sound event"
        );
        let (namespace, name) = entry
            .java
            .split_once(':')
            .context("invalid sound identifier")?;
        let event = definitions
            .sound_definitions
            .get(&entry.bedrock)
            .with_context(|| format!("missing mapped sound event: {}", entry.bedrock))?;
        let event = NativeEvent::deserialize(event)
            .with_context(|| format!("unsupported sound definition: {}", entry.bedrock))?;
        let primary = entry
            .file
            .strip_suffix(".ogg")
            .context("sounds must be OGG Vorbis")?;
        ensure!(
            event.category == "player"
                && event
                    .sounds
                    .first()
                    .is_some_and(|clip| clip.options().name == primary),
            "mapped event must be player category and start with its mapped clip: {}",
            entry.bedrock
        );
        let output_dir = output.join(format!("assets/{namespace}/sounds"));
        let sounds = convert_clips(
            root,
            event.sounds,
            namespace,
            name,
            &output_dir,
            total,
            &mut files,
        )?;
        namespaces
            .entry(namespace)
            .or_default()
            .insert(name, JavaEvent { sounds });
    }
    for (namespace, events) in namespaces {
        fs::write(
            output.join(format!("assets/{namespace}/sounds.json")),
            serde_json::to_vec_pretty(&events)?,
        )?;
    }
    Ok(())
}

fn convert_clips(
    root: &Path,
    clips: Vec<NativeClip<'_>>,
    namespace: &str,
    event_name: &str,
    output: &Path,
    total: &mut usize,
    files: &mut BTreeSet<String>,
) -> Result<Vec<JavaClip>> {
    let mut converted = BTreeMap::new();
    let mut sounds = Vec::with_capacity(clips.len());
    let mut total_weight = 0i32;
    let positive = |value: f32| value.is_finite() && value > 0.0;
    for (index, clip) in clips.into_iter().enumerate() {
        let options = clip.options();
        let weight = options.weight.unwrap_or(1);
        ensure!(weight > 0, "sound weights must be positive");
        total_weight = total_weight
            .checked_add(weight)
            .context("sound weights exceed Java range")?;
        ensure!(
            options.volume.is_none_or(positive) && options.pitch.is_none_or(positive),
            "sound volume and pitch must be positive finite numbers"
        );
        let clip = options.name;
        let id = match converted.entry(clip) {
            Entry::Occupied(existing) => existing.into_mut(),
            Entry::Vacant(vacant) => {
                ensure!(
                    clip.starts_with("sounds/")
                        && clip
                            .split('/')
                            .all(|part| !part.is_empty() && part != "." && part != ".."),
                    "sound must be under sounds/"
                );
                let source_name = format!("{clip}.ogg");
                let data = read_bounded(&confined(root, &source_name)?, 16 * 1_048_576)?;
                validate_vorbis(&data)
                    .with_context(|| format!("invalid Vorbis clip {source_name}"))?;
                *total += data.len();
                ensure!(
                    *total <= MAX_TOTAL_BYTES,
                    "converted content exceeds 256 MiB"
                );

                let name = if index == 0 {
                    event_name.to_owned()
                } else {
                    format!("{event_name}/{}", index + 1)
                };
                let id = format!("{namespace}:{name}");
                crate::validate_resource_id(&id).map_err(anyhow::Error::msg)?;
                ensure!(files.insert(id.clone()), "duplicate Java sound file {id}");
                let target = output.join(format!("{name}.ogg"));
                fs::create_dir_all(target.parent().context("sound parent")?)?;
                fs::write(target, data)?;
                vacant.insert(id)
            }
        };
        sounds.push(JavaClip {
            name: id.clone(),
            stream: options.stream,
            volume: options.volume,
            pitch: options.pitch,
            weight: options.weight,
        });
    }
    Ok(sounds)
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80000000 != 0 {
                (crc << 1) ^ 0x04c11db7
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}
const CRC_TABLE: [u32; 256] = crc_table();

fn validate_vorbis(data: &[u8]) -> Result<()> {
    let mut offset = 0usize;
    let mut serial = None;
    let mut sequence = 0u32;
    let mut sample_rate = 0u64;
    let mut last_granule = 0;
    let mut ended = false;
    let mut continuation = false;
    while offset < data.len() {
        ensure!(!ended, "chained/trailing Ogg data is unsupported");
        let header = data
            .get(offset..offset + 27)
            .context("truncated Ogg header")?;
        ensure!(
            &header[..4] == b"OggS" && header[4] == 0 && header[5] & !7 == 0,
            "invalid Ogg header"
        );
        let flags = header[5];
        ensure!((flags & 1 != 0) == continuation, "invalid Ogg continuation");
        let granule = u64::from_le_bytes(header[6..14].try_into()?);
        let page_serial = u32::from_le_bytes(header[14..18].try_into()?);
        let seq = u32::from_le_bytes(header[18..22].try_into()?);
        ensure!(seq == sequence, "Ogg sequence discontinuity");
        sequence = sequence.checked_add(1).context("Ogg sequence overflow")?;
        let lacing = data
            .get(offset + 27..offset + 27 + usize::from(header[26]))
            .context("truncated Ogg lacing")?;
        ensure!(!lacing.is_empty(), "empty Ogg page unsupported");
        let payload_size: usize = lacing.iter().map(|b| usize::from(*b)).sum();
        let page_size = 27 + lacing.len() + payload_size;
        let page = data
            .get(offset..offset + page_size)
            .context("truncated Ogg payload")?;
        let mut crc = 0u32;
        for (i, &byte) in page.iter().enumerate() {
            let byte = if (22..26).contains(&i) { 0 } else { byte };
            crc = (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ byte) as usize];
        }
        ensure!(
            crc == u32::from_le_bytes(header[22..26].try_into()?),
            "Ogg CRC mismatch"
        );
        if let Some(serial) = serial {
            ensure!(
                serial == page_serial && flags & 2 == 0,
                "multiple Ogg streams unsupported"
            );
        } else {
            serial = Some(page_serial);
            ensure!(
                flags == 2 && lacing == [30],
                "invalid Vorbis identification page"
            );
            let id = &page[28..];
            ensure!(
                &id[..7] == b"\x01vorbis" && id[7..11] == [0; 4] && (1..=2).contains(&id[11]),
                "Vorbis requires one or two channels"
            );
            sample_rate = u64::from(u32::from_le_bytes(id[12..16].try_into()?));
            ensure!(
                (8000..=48000).contains(&sample_rate),
                "Vorbis rate must be 8000..48000 Hz"
            );
            let (small, large) = (id[28] & 15, id[28] >> 4);
            ensure!(
                (6..=13).contains(&small) && (small..=13).contains(&large) && id[29] == 1,
                "invalid Vorbis block/framing flags"
            );
        }
        if granule != u64::MAX {
            ensure!(
                granule >= last_granule && granule <= sample_rate * 30,
                "sound exceeds 30 seconds or has invalid sample position"
            );
            last_granule = granule;
        }
        continuation = lacing.last() == Some(&255);
        ended = flags & 4 != 0;
        if ended {
            ensure!(
                !continuation && granule != u64::MAX && granule > 0,
                "invalid Ogg end"
            );
        }
        offset += page_size;
    }
    ensure!(ended, "missing Ogg end page");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_tone_is_valid_and_corruption_and_truncation_are_rejected() {
        let bytes = include_bytes!("../tests/fixtures/tone.ogg");
        validate_vorbis(bytes).unwrap();
        for length in [0, 26, bytes.len() - 1] {
            assert!(validate_vorbis(&bytes[..length]).is_err());
        }
        let mut bad = bytes.to_vec();
        *bad.last_mut().unwrap() ^= 1;
        assert!(validate_vorbis(&bad).is_err());
    }
    #[test]
    fn sound_conversion_preserves_original_bytes_and_rejects_semantic_loss() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pack")
            .canonicalize()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut mapping = SoundMapping {
            bedrock: "sculith.demo_tone".into(),
            java: "sculith:demo_tone".into(),
            file: "sounds/sculith_demo.ogg".into(),
        };
        convert(&root, &[mapping.clone()], dir.path(), &mut 0).unwrap();
        assert_eq!(
            fs::read(root.join(&mapping.file)).unwrap(),
            fs::read(dir.path().join("assets/sculith/sounds/demo_tone.ogg")).unwrap()
        );
        mapping.bedrock = "nonexistent".into();
        assert!(convert(&root, &[mapping], dir.path(), &mut 0).is_err());
    }

    #[test]
    fn sound_conversion_preserves_variant_order_and_repeated_choices() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().canonicalize().unwrap();
        let output = tempfile::tempdir().unwrap();
        let sounds = root.join("sounds");
        fs::create_dir(&sounds).unwrap();
        let bytes = include_bytes!("../tests/fixtures/tone.ogg");
        fs::write(sounds.join("first.ogg"), bytes).unwrap();
        fs::write(sounds.join("second.ogg"), bytes).unwrap();
        fs::write(
            sounds.join("sound_definitions.json"),
            r#"{"format_version":"1.20.20","sound_definitions":{"example.bell":{
                "category":"player","sounds":[
                    {"name":"sounds/first","volume":0.625,"pitch":0.75,"weight":3,"stream":true},
                    "sounds/second",{"name":"sounds/first","weight":2}
                ]
            }}}"#,
        )
        .unwrap();
        let mappings = [SoundMapping {
            bedrock: "example.bell".into(),
            java: "example:bell".into(),
            file: "sounds/first.ogg".into(),
        }];
        let mut total = 0;
        convert(&root, &mappings, output.path(), &mut total).unwrap();
        let java: serde_json::Value =
            read_json(&output.path().join("assets/example/sounds.json")).unwrap();
        assert_eq!(
            java["bell"]["sounds"],
            serde_json::json!([
                {"name":"example:bell","stream":true,"volume":0.625,"pitch":0.75,"weight":3},
                {"name":"example:bell/2","stream":false},
                {"name":"example:bell","stream":false,"weight":2},
            ])
        );
        assert_eq!(total, 2 * bytes.len());
        for name in ["bell", "bell/2"] {
            let path = output
                .path()
                .join(format!("assets/example/sounds/{name}.ogg"));
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert!(
            !output
                .path()
                .join("assets/example/sounds/bell/3.ogg")
                .exists()
        );

        fs::write(sounds.join("second.ogg"), b"invalid clip").unwrap();
        let invalid = tempfile::tempdir().unwrap();
        let error = convert(&root, &mappings, invalid.path(), &mut 0).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid Vorbis clip sounds/second.ogg")
        );
    }

    #[test]
    fn rejects_clip_options_java_cannot_use() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().canonicalize().unwrap();
        let output = tempfile::tempdir().unwrap();
        let sounds = root.join("sounds");
        fs::create_dir(&sounds).unwrap();
        fs::write(
            sounds.join("first.ogg"),
            include_bytes!("../tests/fixtures/tone.ogg"),
        )
        .unwrap();
        let mapping = [SoundMapping {
            bedrock: "example.bell".into(),
            java: "example:bell".into(),
            file: "sounds/first.ogg".into(),
        }];
        for (clip, expected) in [
            (
                r#"{"name":"sounds/first","weight":0}"#,
                "sound weights must be positive",
            ),
            (
                r#"{"name":"sounds/first","volume":0}"#,
                "sound volume and pitch must be positive",
            ),
            (
                r#"{"name":"sounds/first","weight":2147483647}"#,
                "sound weights exceed Java range",
            ),
            (
                r#"{"name":"sounds/first","pitch":[0.8,1.0]}"#,
                "unsupported sound definition",
            ),
        ] {
            let json = format!(
                r#"{{"format_version":"1.14.0","sound_definitions":{{"example.bell":{{
                "category":"player","sounds":[{clip},"sounds/first"]
            }}}}}}"#
            );
            fs::write(sounds.join("sound_definitions.json"), json).unwrap();
            let error = convert(&root, &mapping, output.path(), &mut 0).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }
}
