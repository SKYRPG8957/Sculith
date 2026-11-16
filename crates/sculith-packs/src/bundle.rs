//! Immutable edition archives built from one bounded source snapshot.
//! ZIP members use the stored method, fixed dates and sorted UTF-8 names.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha1::{Digest as _, Sha1};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{BedrockManifest, ConversionReport};

pub const MAX_PACK_BYTES: usize = 32 * 1_048_576;
const MAX_FILES: usize = 512;
const MAX_DEPTH: usize = 16;
const MAX_FILE_BYTES: u64 = 16 * 1_048_576;

#[derive(Clone)]
pub struct Archive {
    pub bytes: Arc<[u8]>,
    pub sha1: String,
    pub sha256: [u8; 32],
}

pub struct Bundle {
    pub bedrock_id: Uuid,
    pub bedrock_version: String,
    pub java_id: Uuid,
    pub bedrock: Archive,
    pub java: Archive,
    pub report: ConversionReport,
}

impl Bundle {
    /// Freeze both the source and the mapping before conversion. No session ever
    /// reads the mutable operator directory or rebuilds an archive on demand.
    pub fn prepare(input: &Path, mapping: &Path) -> Result<Self> {
        let root = input
            .canonicalize()
            .context("missing Bedrock pack source")?;
        ensure!(root.is_dir(), "Bedrock pack source must be a directory");
        let files = snapshot(&root)?;
        let manifest_bytes = files
            .get("manifest.json")
            .context("missing manifest.json")?;
        ensure!(manifest_bytes.len() <= 1_048_576, "manifest exceeds 1 MiB");
        let manifest: BedrockManifest = serde_json::from_slice(manifest_bytes)?;
        let original = Uuid::parse_str(&manifest.header.uuid)?;
        ensure!(
            manifest.header.name.len() <= 256,
            "pack name exceeds 256 bytes"
        );
        let version = manifest
            .header
            .version
            .map(|part| part.to_string())
            .join(".");
        let source_hash = snapshot_hash(&files);
        let bedrock_id = content_uuid(original, &source_hash);

        let stage = tempfile::tempdir()?;
        let frozen = stage.path().join("bedrock");
        fs::create_dir(&frozen)?;
        let mut normalized: Value = serde_json::from_slice(manifest_bytes)?;
        ensure!(
            normalized
                .get("dependencies")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)),
            "pack dependencies are not supported by the single-pack delivery preview"
        );
        ensure!(
            normalized
                .get("subpacks")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)),
            "subpack selection is not supported by this preview"
        );
        let mut native = files;
        normalized["header"]["uuid"] = Value::String(bedrock_id.to_string());
        let modules = normalized["modules"]
            .as_array_mut()
            .context("missing pack modules")?;
        for (index, module) in modules.iter_mut().enumerate() {
            // Generated module IDs belong to this immutable generated pack;
            // authored manifests on disk are never rewritten.
            module["uuid"] =
                Value::String(content_uuid(bedrock_id, &(index as u64).to_be_bytes()).to_string());
        }
        native.insert(
            "manifest.json".into(),
            serde_json::to_vec_pretty(&normalized)?,
        );
        for (name, bytes) in &native {
            let path = frozen.join(name);
            fs::create_dir_all(path.parent().context("missing member parent")?)?;
            fs::write(path, bytes)?;
        }
        let frozen_mapping = stage.path().join("mapping.json");
        fs::write(&frozen_mapping, crate::read_bounded(mapping, 1_048_576)?)?;
        let java_root = stage.path().join("java");
        let report = crate::convert(&frozen, &frozen_mapping, &java_root, 97, 1)?;
        let bedrock = archive(&native)?;
        let java = archive(&snapshot(&java_root)?)?;
        let java_id = content_uuid(original, &java.sha256);
        Ok(Self {
            bedrock_id,
            bedrock_version: version,
            java_id,
            bedrock,
            java,
            report,
        })
    }

    /// Export inspectable artifacts into a new directory without changing source.
    pub fn write(&self, output: &Path) -> Result<()> {
        ensure!(!output.try_exists()?, "bundle output already exists");
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stage = tempfile::tempdir_in(parent)?;
        fs::write(stage.path().join("bedrock.mcpack"), &self.bedrock.bytes)?;
        fs::write(stage.path().join("java.zip"), &self.java.bytes)?;
        fs::write(
            stage.path().join("bundle.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "bedrock_id": self.bedrock_id.to_string(),
                "bedrock_version": self.bedrock_version,
                "java_id": self.java_id.to_string(),
                "java_sha1": self.java.sha1,
                "bedrock_sha256": hex(&self.bedrock.sha256),
                "java_sha256": hex(&self.java.sha256),
                "conversion": self.report,
            }))?,
        )?;
        fs::rename(stage.path(), output).context("cannot commit pack bundle")?;
        Ok(())
    }
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    fn visit(
        root: &Path,
        dir: &Path,
        depth: usize,
        files: &mut BTreeMap<String, Vec<u8>>,
        total: &mut usize,
        entries: &mut usize,
    ) -> Result<()> {
        ensure!(
            depth <= MAX_DEPTH,
            "pack directory nesting exceeds {MAX_DEPTH}"
        );
        for entry in fs::read_dir(dir)? {
            *entries += 1;
            ensure!(*entries <= 1024, "pack exceeds 1024 directory entries");
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "pack source cannot contain symlinks");
            let relative_path = path.strip_prefix(root)?;
            for part in relative_path.components() {
                let part = part
                    .as_os_str()
                    .to_str()
                    .context("pack path is not UTF-8")?;
                ensure!(
                    part.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                    "pack path components require portable ASCII names"
                );
            }
            let relative = relative_path
                .to_str()
                .context("pack path is not UTF-8")?
                .replace('\\', "/");
            ensure!(
                relative.len() <= 240
                    && relative
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b)),
                "pack paths require portable ASCII names, at most 240 bytes"
            );
            if kind.is_dir() {
                visit(root, &path, depth + 1, files, total, entries)?;
            } else {
                ensure!(kind.is_file(), "pack members must be regular files");
                ensure!(files.len() < MAX_FILES, "pack exceeds {MAX_FILES} files");
                let data = crate::read_bounded(&path, MAX_FILE_BYTES)?;
                *total += data.len();
                ensure!(*total <= MAX_PACK_BYTES, "pack exceeds 32 MiB");
                ensure!(
                    files.insert(relative, data).is_none(),
                    "duplicate pack path"
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, 0, &mut files, &mut 0, &mut 0)?;
    Ok(files)
}

fn snapshot_hash(files: &BTreeMap<String, Vec<u8>>) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"sculith-pack-snapshot-v1\0");
    for (name, data) in files {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((data.len() as u64).to_be_bytes());
        digest.update(data);
    }
    digest.finalize().into()
}

fn content_uuid(namespace: Uuid, content: &[u8]) -> Uuid {
    // RFC 9562 UUIDv5: SHA-1(namespace bytes || name), with version/variant bits.
    let mut digest = Sha1::new();
    digest.update(namespace.as_bytes());
    digest.update(content);
    let hash = digest.finalize();
    let mut id: [u8; 16] = hash[..16].try_into().expect("SHA-1 has 20 bytes");
    id[6] = (id[6] & 15) | 0x50;
    id[8] = (id[8] & 63) | 0x80;
    Uuid::from_bytes(id)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[(byte >> 4) as usize] as char,
                DIGITS[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

fn archive(files: &BTreeMap<String, Vec<u8>>) -> Result<Archive> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let size = data.len() as u32;
        let crc = crc32fast::hash(data);
        // APPNOTE 6.3.10: stored ZIP, UTF-8 flag, DOS 1980-01-01, no extras.
        out.extend_from_slice(&0x04034b50u32.to_le_bytes());
        for value in [20u16, 0x800, 0, 0, 0x21] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, size, size] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in [name.len() as u16, 0] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        for value in [0x0314u16, 20, 0x800, 0, 0, 0x21] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, size, size] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        for value in [name.len() as u16, 0, 0, 0, 0] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0o100644u32 << 16, offset] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        central.extend_from_slice(name.as_bytes());
        ensure!(
            out.len() + central.len() + 22 <= MAX_PACK_BYTES,
            "archive exceeds 32 MiB"
        );
    }
    let offset = out.len() as u32;
    let size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    for value in [0u16, 0, files.len() as u16, files.len() as u16] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in [size, offset] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(Archive {
        sha1: format!("{:x}", Sha1::digest(&out)),
        sha256: Sha256::digest(&out).into(),
        bytes: out.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/pack/bedrock")
    }

    #[test]
    fn snapshot_is_deterministic_and_content_changes_invalidate_cache_ids() {
        let mapping = source().parent().unwrap().join("textures.json");
        let before = fs::read(source().join("manifest.json")).unwrap();
        let first = Bundle::prepare(&source(), &mapping).unwrap();
        let second = Bundle::prepare(&source(), &mapping).unwrap();
        assert_eq!(first.bedrock.bytes, second.bedrock.bytes);
        assert_eq!(first.java.bytes, second.java.bytes);
        assert_eq!(first.java_id, second.java_id);
        assert_eq!(first.bedrock_id, second.bedrock_id);
        assert_eq!(fs::read(source().join("manifest.json")).unwrap(), before);
        let copy = tempfile::tempdir().unwrap();
        for (name, bytes) in snapshot(&source()).unwrap() {
            let target = copy.path().join(name);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, bytes).unwrap();
        }
        fs::write(
            copy.path().join("credits.txt"),
            "Authored example; revised credits.",
        )
        .unwrap();
        let changed = Bundle::prepare(copy.path(), &mapping).unwrap();
        assert_ne!(first.bedrock_id, changed.bedrock_id);
        assert_ne!(first.java_id, changed.java_id);
        // Existing immutable archive bytes never change when source changes.
        assert_eq!(first.bedrock.bytes, second.bedrock.bytes);
        let output = copy.path().join("out");
        first.write(&output).unwrap();
        assert!(first.write(&output).is_err());
        assert_eq!(
            fs::read(output.join("java.zip")).unwrap(),
            first.java.bytes.as_ref()
        );
    }

    #[test]
    fn uuid_v5_matches_the_rfc_dns_vector() {
        assert_eq!(
            content_uuid(Uuid::NAMESPACE_DNS, b"www.example.com").to_string(),
            "2ed6657d-e927-568b-95e1-2665a8aea6a2"
        );
    }

    #[test]
    fn bounded_source_rejects_special_paths_and_deep_or_large_trees() {
        let dir = tempfile::tempdir().unwrap();
        let mut path = dir.path().to_owned();
        for _ in 0..=MAX_DEPTH {
            path.push("nested");
        }
        fs::create_dir_all(&path).unwrap();
        assert!(snapshot(dir.path()).is_err());
        let oversized = tempfile::tempdir().unwrap();
        fs::File::create(oversized.path().join("large.png"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(snapshot(oversized.path()).is_err());
        #[cfg(unix)]
        {
            let linked = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(
                source().join("manifest.json"),
                linked.path().join("manifest.json"),
            )
            .unwrap();
            assert!(snapshot(linked.path()).is_err());
            let invalid = tempfile::tempdir().unwrap();
            fs::write(invalid.path().join("bad\\name"), b"bad").unwrap();
            assert!(snapshot(invalid.path()).is_err());
        }
    }
}
