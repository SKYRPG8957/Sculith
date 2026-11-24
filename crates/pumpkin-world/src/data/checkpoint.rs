//! Journal for a checkpoint spanning players, terrain and entity regions.

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
use rustc_hash::{FxHashMap, FxHashSet};
use sculith_util::math::vector2::Vector2;
use tokio::sync::{Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xxhash_rust::xxh64::xxh64;

use crate::{
    chunk::{
        ChunkData, ChunkEntityData,
        format::anvil::SingleChunkDataSerializer,
        io::{
            Dirtiable, FileIO, LoadedData, converter::RegionFormat, sync_parent,
            write_file_atomically,
        },
    },
    level::{ChunkSaver, EntitySaver, LevelFolder, SyncChunk},
};

use super::player_data::PlayerDataStorage;

const JOURNAL_NAME: &str = ".sculith-world-checkpoint";
const ENTITY_MAGIC: &[u8; 8] = b"SCWORLD\x03";
const MAGIC: &[u8; 8] = b"SCWORLD\x02";
const LEGACY_MAGIC: &[u8; 8] = b"SCWORLD\x01";

type ChunkGroups<T> = BTreeMap<PathBuf, Vec<(Vector2<i32>, T)>>;
pub type TerrainImages = ChunkGroups<Bytes>;
pub type PreparedTerrain = ChunkGroups<SyncChunk>;
pub type EntityImages = ChunkGroups<EntityChunkImage>;

pub struct EntityChunkImage {
    /// All saved actors were loaded, so even an empty image replaces the old file records.
    pub replace: bool,
    pub records: Vec<NbtCompound>,
}

impl EntityChunkImage {
    fn check(&self) -> io::Result<()> {
        // A merge can be interrupted and repeated. Every added actor needs a stable identity.
        if !self.replace
            && self
                .records
                .iter()
                .any(|record| record.get_uuid("UUID").is_none())
        {
            return Err(invalid());
        }
        Ok(())
    }
}

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
    pub entities: EntityImages,
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
        let has_entities = !self.entities.is_empty();
        let mut bytes = if has_entities { ENTITY_MAGIC } else { MAGIC }.to_vec();
        bytes.put_u64(self.revision);
        put_bytes(&mut bytes, format.name().as_bytes())?;
        put_length(&mut bytes, self.players.len())?;
        for (uuid, nbt) in &self.players {
            bytes.put_u128(uuid.as_u128());
            put_bytes(&mut bytes, &Nbt::from(nbt.clone()).write())?;
        }
        put_chunk_groups(&mut bytes, &self.worlds, |out, _, raw| put_bytes(out, raw))?;
        if has_entities {
            put_chunk_groups(&mut bytes, &self.entities, |out, pos, image| {
                image.check()?;
                out.put_u8(u8::from(image.replace));
                put_bytes(out, &ChunkEntityData::encode_records(pos, &image.records))
            })?;
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

/// Resolves dormant actor records through the same region cache as normal entity I/O.
pub async fn write_entities(
    saver: &EntitySaver,
    folder: &LevelFolder,
    images: &[(Vector2<i32>, EntityChunkImage)],
) -> Result<(), String> {
    let merge_positions: Vec<_> = images
        .iter()
        .filter_map(|(pos, image)| (!image.replace).then_some(*pos))
        .collect();
    let mut existing = FxHashMap::default();
    if !merge_positions.is_empty() {
        let (send, mut receive) = tokio::sync::mpsc::channel(merge_positions.len());
        saver.fetch_chunks(folder, &merge_positions, send).await;
        let mut remaining = merge_positions.into_iter().collect::<FxHashSet<_>>();
        while let Some(result) = receive.recv().await {
            match result {
                LoadedData::Loaded(chunk) => {
                    let pos = Vector2::new(chunk.x, chunk.z);
                    remaining.remove(&pos);
                    let records = chunk
                        .data
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    existing.insert(pos, records.clone());
                }
                LoadedData::Missing(pos) => {
                    remaining.remove(&pos);
                }
                LoadedData::Error((pos, err)) => {
                    return Err(format!("Failed to read entity chunk {pos:?}: {err}"));
                }
            }
        }
        if !remaining.is_empty() {
            return Err("Checkpoint entity reads did not finish".to_string());
        }
    }
    let chunks = images
        .iter()
        .map(|(pos, image)| {
            let mut data = existing.remove(pos).unwrap_or_default();
            ChunkEntityData::merge_records(&mut data, image.replace, image.records.clone());
            let chunk = ChunkEntityData {
                x: pos.x,
                z: pos.y,
                data: std::sync::Mutex::new(data),
                live: AtomicBool::new(false),
                dirty: AtomicBool::new(true),
            };
            (*pos, Arc::new(chunk))
        })
        .collect();
    saver
        .save_chunks(folder, chunks)
        .await
        .map_err(|err| format!("Failed to save entities: {err}"))?;
    saver
        .flush_all()
        .await
        .map_err(|err| format!("Failed to flush entities: {err}"))
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

fn put_chunk_groups<T>(
    out: &mut Vec<u8>,
    groups: &ChunkGroups<T>,
    encode: impl Fn(&mut Vec<u8>, Vector2<i32>, &T) -> io::Result<()>,
) -> io::Result<()> {
    put_length(out, groups.len())?;
    for (path, chunks) in groups {
        put_bytes(out, path.to_str().ok_or_else(invalid)?.as_bytes())?;
        put_length(out, chunks.len())?;
        for (pos, image) in chunks {
            out.put_i32(pos.x);
            out.put_i32(pos.y);
            encode(out, *pos, image)?;
        }
    }
    Ok(())
}

fn read_chunk_groups<T>(
    bytes: &mut Bytes,
    decode_image: impl Fn(&mut Bytes, Vector2<i32>) -> io::Result<T>,
) -> io::Result<ChunkGroups<T>> {
    let mut groups = BTreeMap::new();
    let world_count = bytes.try_get_u32().map_err(|_| invalid())?;
    for _ in 0..world_count {
        let path = frame(bytes)?;
        let path = PathBuf::from(std::str::from_utf8(&path).map_err(|_| invalid())?);
        if !path
            .components()
            .all(|item| matches!(item, Component::Normal(_)))
        {
            return Err(invalid());
        }
        let mut seen = FxHashSet::default();
        let mut chunks = Vec::new();
        let count = bytes.try_get_u32().map_err(|_| invalid())?;
        for _ in 0..count {
            let x = bytes.try_get_i32().map_err(|_| invalid())?;
            let z = bytes.try_get_i32().map_err(|_| invalid())?;
            let pos = Vector2::new(x, z);
            if !seen.insert(pos) {
                return Err(invalid());
            }
            chunks.push((pos, decode_image(bytes, pos)?));
        }
        if groups.insert(path, chunks).is_some() {
            return Err(invalid());
        }
    }
    Ok(groups)
}

fn decode(mut bytes: Bytes, format: RegionFormat) -> io::Result<SaveCheckpoint> {
    let (payload, checksum) = bytes.split_last_chunk::<8>().ok_or_else(invalid)?;
    let has_entities = payload.starts_with(ENTITY_MAGIC);
    if *checksum != xxh64(payload, 0).to_be_bytes()
        || !(has_entities || payload.starts_with(MAGIC) || payload.starts_with(LEGACY_MAGIC))
    {
        return Err(invalid());
    }
    let has_revision = !payload.starts_with(LEGACY_MAGIC);
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
    result.worlds = read_chunk_groups(&mut bytes, |data, _| frame(data))?;
    if has_entities {
        result.entities = read_chunk_groups(&mut bytes, |data, pos| {
            let replace = match data.try_get_u8().map_err(|_| invalid())? {
                0 => false,
                1 => true,
                _ => return Err(invalid()),
            };
            let chunk = ChunkEntityData::from_bytes(&frame(data)?, pos).map_err(|_| invalid())?;
            let records = chunk.data.into_inner().map_err(|_| invalid())?;
            let image = EntityChunkImage { replace, records };
            image.check()?;
            Ok(image)
        })?;
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
            ..SaveCheckpoint::default()
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
        assert!(decoded.entities.is_empty());
        SaveCheckpoint::remove(root.path()).unwrap();
        SaveCheckpoint::require_materialized(root.path()).unwrap();
    }

    #[tokio::test]
    async fn checkpoint_replay_clears_live_chunks_and_preserves_dormant_actors() {
        let root = tempfile::tempdir().unwrap();
        let folder = LevelFolder::new(root.path().to_path_buf(), root.path().to_path_buf());
        let saver = EntitySaver::from_config(&sculith_config::chunk::ChunkConfig::Pump);
        let live = Vector2::new(0, 0);
        let dormant = Vector2::new(1, 0);
        let record = |id| {
            let mut actor = NbtCompound::new();
            actor.put_uuid("UUID", Uuid::from_u128(id));
            actor
        };
        let image = |replace, records| EntityChunkImage { replace, records };
        write_entities(
            &saver,
            &folder,
            &[
                (live, image(true, vec![record(1)])),
                (dormant, image(true, vec![record(2)])),
            ],
        )
        .await
        .unwrap();
        let checkpoint = SaveCheckpoint {
            entities: BTreeMap::from([(
                PathBuf::new(),
                vec![
                    (live, image(true, Vec::new())),
                    (dormant, image(false, vec![record(3)])),
                ],
            )]),
            ..SaveCheckpoint::default()
        };
        checkpoint.publish(root.path(), RegionFormat::Pump).unwrap();
        let recovered = SaveCheckpoint::read(root.path(), RegionFormat::Pump)
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            write_entities(&saver, &folder, &recovered.entities[Path::new("")])
                .await
                .unwrap();
        }
        let (send, mut receive) = tokio::sync::mpsc::channel(2);
        saver.fetch_chunks(&folder, &[live, dormant], send).await;
        let mut saved = FxHashMap::default();
        while let Some(result) = receive.recv().await {
            let LoadedData::Loaded(chunk) = result else {
                panic!("checkpoint did not save its entity chunk");
            };
            saved.insert(
                Vector2::new(chunk.x, chunk.z),
                chunk.data.lock().unwrap().clone(),
            );
        }
        assert!(saved[&live].is_empty());
        assert_eq!(saved[&dormant], [record(2), record(3)]);
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
