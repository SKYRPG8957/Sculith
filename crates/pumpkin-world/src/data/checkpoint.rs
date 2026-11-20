//! Journal for a checkpoint spanning player files and terrain regions.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bytes::{Buf, BufMut, Bytes};
use pumpkin_nbt::{Nbt, compound::NbtCompound};
use pumpkin_util::math::vector2::Vector2;
use tokio::sync::{Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xxhash_rust::xxh64::xxh64;

use crate::{
    chunk::{
        ChunkData,
        io::{Dirtiable, FileIO, converter::RegionFormat, sync_parent, write_file_atomically},
    },
    level::{ChunkSaver, LevelFolder, SyncChunk},
};

use super::player_data::PlayerDataStorage;

const JOURNAL_NAME: &str = ".sculith-world-checkpoint";
const MAGIC: &[u8; 8] = b"SCWORLD\x02";
const LEGACY_MAGIC: &[u8; 8] = b"SCWORLD\x01";

pub type TerrainImages = BTreeMap<PathBuf, Vec<(Vector2<i32>, Bytes)>>;
pub type PreparedTerrain = BTreeMap<PathBuf, Vec<(Vector2<i32>, SyncChunk)>>;

/// Keeps ordinary writes behind a checkpoint, including retries after a failed write.
#[derive(Default)]
pub struct CheckpointFence {
    access: Arc<RwLock<()>>,
    terrain_reads: Arc<RwLock<()>>,
    pending: AtomicBool,
    terrain_pending: AtomicBool,
    changed: Notify,
}

impl CheckpointFence {
    pub async fn read(&self, cancelling: &CancellationToken) -> Option<OwnedRwLockReadGuard<()>> {
        self.read_when_ready(&self.access, &self.pending, cancelling)
            .await
    }

    pub async fn read_terrain(
        &self,
        cancelling: &CancellationToken,
    ) -> Option<OwnedRwLockReadGuard<()>> {
        self.read_when_ready(&self.terrain_reads, &self.terrain_pending, cancelling)
            .await
    }

    async fn read_when_ready(
        &self,
        lock: &Arc<RwLock<()>>,
        pending: &AtomicBool,
        cancelling: &CancellationToken,
    ) -> Option<OwnedRwLockReadGuard<()>> {
        loop {
            let changed = self.changed.notified();
            let access = tokio::select! {
                biased;
                guard = lock.clone().read_owned() => guard,
                () = cancelling.cancelled() => return None,
            };
            if !pending.load(Ordering::Acquire) {
                return Some(access);
            }
            drop(access);
            tokio::select! {
                () = changed => {}
                () = cancelling.cancelled() => return None,
            }
        }
    }

    pub async fn materialize_terrain(&self) -> OwnedRwLockWriteGuard<()> {
        let access = self.terrain_reads.clone().write_owned().await;
        self.terrain_pending.store(true, Ordering::Release);
        access
    }

    pub fn terrain_materialized(&self) {
        self.terrain_pending.store(false, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub fn try_read(&self) -> Option<OwnedRwLockReadGuard<()>> {
        let access = self.access.clone().try_read_owned().ok()?;
        (!self.pending.load(Ordering::Acquire)).then_some(access)
    }

    pub async fn read_ready(&self) -> Option<OwnedRwLockReadGuard<()>> {
        let access = self.access.clone().read_owned().await;
        (!self.is_pending()).then_some(access)
    }

    pub fn try_write(&self) -> Option<OwnedRwLockWriteGuard<()>> {
        self.access.clone().try_write_owned().ok()
    }

    pub async fn write(&self) -> OwnedRwLockWriteGuard<()> {
        self.access.clone().write_owned().await
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub fn set_pending(&self, pending: bool) {
        self.pending.store(pending, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub async fn pending(&self) {
        loop {
            let changed = self.changed.notified();
            if self.is_pending() {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Default)]
pub struct SaveCheckpoint {
    pub revision: u64,
    pub players: Vec<(Uuid, NbtCompound)>,
    pub worlds: TerrainImages,
}

impl SaveCheckpoint {
    /// Offline tools must treat the checkpoint as the latest state until replay has finished.
    pub fn require_materialized(root: &Path) -> io::Result<()> {
        match fs::symlink_metadata(root.join(JOURNAL_NAME)) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
            Ok(_) => Err(io::Error::other("world checkpoint is awaiting recovery")),
        }
    }

    pub fn read(root: &Path, format: RegionFormat) -> io::Result<Option<Self>> {
        match fs::read(root.join(JOURNAL_NAME)) {
            Ok(bytes) => decode(Bytes::from(bytes), format).map(Some),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn publish(&self, root: &Path, format: RegionFormat) -> io::Result<()> {
        let mut bytes = MAGIC.to_vec();
        bytes.put_u64(self.revision);
        put_bytes(&mut bytes, format.name().as_bytes())?;
        put_length(&mut bytes, self.players.len())?;
        for (uuid, nbt) in &self.players {
            bytes.put_u128(uuid.as_u128());
            put_bytes(&mut bytes, &Nbt::from(nbt.clone()).write())?;
        }
        put_length(&mut bytes, self.worlds.len())?;
        for (path, chunks) in &self.worlds {
            let path = path.to_str().ok_or_else(invalid)?;
            put_bytes(&mut bytes, path.as_bytes())?;
            put_length(&mut bytes, chunks.len())?;
            for (pos, raw) in chunks {
                bytes.put_i32(pos.x);
                bytes.put_i32(pos.y);
                put_bytes(&mut bytes, raw)?;
            }
        }
        let checksum = xxh64(&bytes, 0);
        bytes.put_u64(checksum);
        write_file_atomically(&root.join(JOURNAL_NAME), &bytes, true)
    }

    pub fn remove(root: &Path) -> io::Result<()> {
        let path = root.join(JOURNAL_NAME);
        fs::remove_file(&path)?;
        sync_parent(&path)
    }

    pub fn confirm_publication(root: &Path) -> io::Result<()> {
        sync_parent(&root.join(JOURNAL_NAME))
    }

    pub fn save_players(&self, storage: &PlayerDataStorage) -> Result<(), String> {
        if self.players.is_empty() {
            return Ok(());
        }
        storage
            .write_committed_batch(self.players.iter().cloned())
            .map_err(|err| err.to_string())
    }

    /// Checks and decodes all chunk images before any of the corresponding region files are changed.
    pub fn prepare_terrain(&self) -> Result<PreparedTerrain, String> {
        self.worlds
            .iter()
            .map(|(path, chunks)| {
                let chunks = chunks
                    .iter()
                    .map(|(pos, raw)| {
                        let chunk = ChunkData::internal_from_bytes(raw, *pos)
                            .map_err(|err| format!("Invalid checkpoint chunk {pos:?}: {err}"))?;
                        chunk.mark_dirty(true);
                        Ok((*pos, Arc::new(chunk)))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok((path.clone(), chunks))
            })
            .collect()
    }
}

pub async fn write_terrain(
    saver: &ChunkSaver,
    folder: &LevelFolder,
    chunks: Vec<(Vector2<i32>, SyncChunk)>,
) -> Result<(), String> {
    saver
        .save_chunks(folder, chunks)
        .await
        .map_err(|err| err.to_string())?;
    saver.flush_all().await.map_err(|err| err.to_string())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid world checkpoint")
}

fn put_length(out: &mut Vec<u8>, length: usize) -> io::Result<()> {
    out.put_u32(
        u32::try_from(length).map_err(|_| io::Error::other("checkpoint record is too large"))?,
    );
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    put_length(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn frame(data: &mut Bytes) -> io::Result<Bytes> {
    let length = data.try_get_u32().map_err(|_| invalid())? as usize;
    if length > data.len() {
        return Err(invalid());
    }
    Ok(data.split_to(length))
}

fn decode(mut bytes: Bytes, format: RegionFormat) -> io::Result<SaveCheckpoint> {
    let (payload, checksum) = bytes.split_last_chunk::<8>().ok_or_else(invalid)?;
    if *checksum != xxh64(payload, 0).to_be_bytes()
        || !(payload.starts_with(MAGIC) || payload.starts_with(LEGACY_MAGIC))
    {
        return Err(invalid());
    }
    let has_revision = payload.starts_with(MAGIC);
    bytes.truncate(payload.len());
    bytes.advance(MAGIC.len());
    let revision = if has_revision {
        bytes.try_get_u64().map_err(|_| invalid())?
    } else {
        0
    };
    if frame(&mut bytes)?.as_ref() != format.name().as_bytes() {
        return Err(io::Error::other(
            "storage format changed with a pending world checkpoint",
        ));
    }

    let mut result = SaveCheckpoint {
        revision,
        ..SaveCheckpoint::default()
    };
    let player_count = bytes.try_get_u32().map_err(|_| invalid())?;
    for _ in 0..player_count {
        let uuid = Uuid::from_u128(bytes.try_get_u128().map_err(|_| invalid())?);
        let nbt = frame(&mut bytes)?;
        let mut reader =
            pumpkin_nbt::deserializer::NbtReadHelperJava::new(io::Cursor::new(nbt.as_ref()));
        let nbt = Nbt::read(&mut reader).map_err(|_| invalid())?;
        result.players.push((uuid, nbt.root_tag));
    }
    let world_count = bytes.try_get_u32().map_err(|_| invalid())?;
    for _ in 0..world_count {
        let path = frame(&mut bytes)?;
        let path = PathBuf::from(std::str::from_utf8(&path).map_err(|_| invalid())?);
        if !path
            .components()
            .all(|item| matches!(item, Component::Normal(_)))
        {
            return Err(invalid());
        }
        let mut chunks = Vec::new();
        let count = bytes.try_get_u32().map_err(|_| invalid())?;
        for _ in 0..count {
            let x = bytes.try_get_i32().map_err(|_| invalid())?;
            let z = bytes.try_get_i32().map_err(|_| invalid())?;
            chunks.push((Vector2::new(x, z), frame(&mut bytes)?));
        }
        if result.worlds.insert(path, chunks).is_some() {
            return Err(invalid());
        }
    }
    if !bytes.is_empty() {
        return Err(invalid());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_preserves_both_kinds_of_records_and_checks_the_format() {
        let root = tempfile::tempdir().unwrap();
        let uuid = Uuid::from_u128(11);
        let mut inventory = NbtCompound::new();
        inventory.put_int("count", 7);
        let pos = Vector2::new(-5, 3);
        let path = PathBuf::from("dimensions/minecraft/overworld");
        let chunks = vec![(pos, Bytes::from_static(b"chunk image"))];
        let checkpoint = SaveCheckpoint {
            revision: 17,
            players: vec![(uuid, inventory.clone())],
            worlds: BTreeMap::from([(path.clone(), chunks.clone())]),
        };
        checkpoint.publish(root.path(), RegionFormat::Pump).unwrap();
        assert!(SaveCheckpoint::require_materialized(root.path()).is_err());
        assert!(SaveCheckpoint::read(root.path(), RegionFormat::Anvil).is_err());
        let decoded = SaveCheckpoint::read(root.path(), RegionFormat::Pump)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.players, [(uuid, inventory)]);
        assert_eq!(decoded.revision, 17);
        assert_eq!(decoded.worlds[&path], chunks);
        SaveCheckpoint::remove(root.path()).unwrap();
        SaveCheckpoint::require_materialized(root.path()).unwrap();
    }

    #[test]
    fn checkpoint_rejects_a_target_outside_the_world() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint = SaveCheckpoint {
            worlds: BTreeMap::from([(PathBuf::from("../outside"), Vec::new())]),
            ..SaveCheckpoint::default()
        };
        checkpoint.publish(root.path(), RegionFormat::Pump).unwrap();
        assert!(SaveCheckpoint::read(root.path(), RegionFormat::Pump).is_err());
    }

    #[test]
    fn checkpoint_reader_accepts_the_previous_version() {
        let root = tempfile::tempdir().unwrap();
        let checkpoint = SaveCheckpoint::default();
        checkpoint.publish(root.path(), RegionFormat::Pump).unwrap();
        let path = root.path().join(JOURNAL_NAME);
        let current = fs::read(&path).unwrap();
        let mut legacy = LEGACY_MAGIC.to_vec();
        legacy.extend_from_slice(&current[MAGIC.len() + 8..current.len() - 8]);
        let checksum = xxh64(&legacy, 0);
        legacy.put_u64(checksum);
        fs::write(&path, legacy).unwrap();
        assert_eq!(
            SaveCheckpoint::read(root.path(), RegionFormat::Pump)
                .unwrap()
                .unwrap()
                .revision,
            0
        );
    }
}
