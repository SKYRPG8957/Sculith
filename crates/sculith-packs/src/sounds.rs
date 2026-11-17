//! Bounded single-clip sound conversion. Ogg framing/CRC/identification are
//! checked here; clients still perform the actual Vorbis setup/audio decode.
use crate::{MAX_TOTAL_BYTES, SoundMapping, confined, read_bounded, read_json};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definitions {
    format_version: String,
    sound_definitions: BTreeMap<String, serde_json::Value>,
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
        definitions.format_version == "1.14.0",
        "unsupported sound definition format"
    );
    let mut native = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let mut namespaces: BTreeMap<&str, BTreeMap<&str, serde_json::Value>> = BTreeMap::new();
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
        let clip = entry
            .file
            .strip_suffix(".ogg")
            .context("sounds must be OGG Vorbis")?;
        ensure!(
            clip.starts_with("sounds/")
                && clip
                    .split('/')
                    .all(|p| !p.is_empty() && p != "." && p != ".."),
            "sound must be under sounds/"
        );
        // Refuse selected definitions whose additional weighting, pitch, range,
        // category or streaming semantics would be silently lost in conversion.
        let expected = serde_json::json!({"category":"player","sounds":[clip]});
        ensure!(
            definitions.sound_definitions.get(&entry.bedrock) == Some(&expected),
            "mapped native event must be one unmodified player-category clip: {}",
            entry.bedrock
        );
        let data = read_bounded(&confined(root, &entry.file)?, 16 * 1_048_576)?;
        validate_vorbis(&data).with_context(|| format!("invalid Vorbis clip {}", entry.file))?;
        *total += data.len();
        ensure!(
            *total <= MAX_TOTAL_BYTES,
            "converted content exceeds 256 MiB"
        );
        let (namespace, name) = entry
            .java
            .split_once(':')
            .context("invalid sound identifier")?;
        let file = output.join(format!("assets/{namespace}/sounds/{name}.ogg"));
        fs::create_dir_all(file.parent().context("sound parent")?)?;
        fs::write(file, data)?;
        namespaces.entry(namespace).or_default().insert(
            name,
            serde_json::json!({"sounds":[{"name":entry.java,"stream":false}]}),
        );
    }
    for (namespace, events) in namespaces {
        fs::write(
            output.join(format!("assets/{namespace}/sounds.json")),
            serde_json::to_vec_pretty(&events)?,
        )?;
    }
    Ok(())
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
}
