use bytes::{Buf, BufMut};
use pumpkin_nbt::{
    compound::NbtCompound,
    nbt_compress::{read_gzip_compound_tag, write_gzip_compound_tag_to_bytes},
};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::{error, info};
use uuid::Uuid;
use xxhash_rust::xxh64::xxh64;

use crate::chunk::io::{sync_parent, write_file_atomically};

const JOURNAL_NAME: &str = ".sculith-player-save";
const JOURNAL_MAGIC: &[u8; 8] = b"SCPDATA\x01";
const RECORD_HEADER_SIZE: usize = size_of::<u128>() + size_of::<u32>();
const CHECKSUM_SIZE: usize = size_of::<u64>();

/// Stores gzip-compressed NBT files with one journal for the current player save.
pub struct PlayerDataStorage {
    data_path: PathBuf,
    save_enabled: bool,
    access: Mutex<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerDataError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("NBT error: {0}")]
    Nbt(String),
}

impl PlayerDataStorage {
    /// Creates a player store in the specified directory.
    pub fn new(data_path: impl Into<PathBuf>, enabled: bool) -> Self {
        let path = data_path.into();
        if !path.exists()
            && let Err(e) = fs::create_dir_all(&path)
        {
            error!(
                "Failed to create player data directory at {}: {e}",
                path.display()
            );
        }

        Self {
            data_path: path,
            save_enabled: enabled,
            access: Mutex::new(()),
        }
    }

    #[must_use]
    pub const fn get_data_path(&self) -> &PathBuf {
        &self.data_path
    }

    #[must_use]
    pub const fn is_save_enabled(&self) -> bool {
        self.save_enabled
    }

    pub const fn set_save_enabled(&mut self, enabled: bool) {
        self.save_enabled = enabled;
    }

    /// Returns the path for a player's data file based on their UUID.
    #[must_use]
    pub fn get_player_data_path(&self, uuid: &Uuid) -> PathBuf {
        self.get_data_path().join(format!("{uuid}.dat"))
    }

    /// Loads a player's latest committed NBT, including batches awaiting replay.
    pub fn load_player_data(&self, uuid: &Uuid) -> Result<(bool, NbtCompound), PlayerDataError> {
        if !self.is_save_enabled() {
            return Ok((false, NbtCompound::new()));
        }

        let _access = self
            .access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(journal) = self.read_pending()?
            && let Some(data) = read_records(&journal)?.get(uuid)
        {
            return read_gzip_compound_tag(Cursor::new(data))
                .map(|nbt| (true, nbt))
                .map_err(|err| PlayerDataError::Nbt(err.to_string()));
        }

        let file = match File::open(self.get_player_data_path(uuid)) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((false, NbtCompound::new()));
            }
            Err(err) => return Err(err.into()),
        };

        read_gzip_compound_tag(file)
            .map(|nbt| (true, nbt))
            .map_err(|err| PlayerDataError::Nbt(err.to_string()))
    }

    /// Saves one player's data through the same writer as a larger snapshot.
    pub fn save_player_data(&self, uuid: &Uuid, data: NbtCompound) -> Result<(), PlayerDataError> {
        self.save_batch([(*uuid, data)])
    }

    /// Persists every player in the batch before removing its journal.
    pub fn save_batch(
        &self,
        players: impl IntoIterator<Item = (Uuid, NbtCompound)>,
    ) -> Result<(), PlayerDataError> {
        if !self.is_save_enabled() {
            return Ok(());
        }

        let _access = self
            .access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.recover_pending()?;
        let players: BTreeMap<_, _> = players.into_iter().collect();
        if players.is_empty() {
            return Ok(());
        }
        let journal = encode_records(players)?;
        write_file_atomically(&self.data_path.join(JOURNAL_NAME), &journal, true)?;
        self.finish_batch(&journal)
    }

    /// Replays a committed batch before the server admits players.
    pub fn recover(&self) -> Result<(), PlayerDataError> {
        let _access = self
            .access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.recover_pending()
    }

    fn read_pending(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.data_path.join(JOURNAL_NAME)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn recover_pending(&self) -> Result<(), PlayerDataError> {
        if let Some(journal) = self.read_pending()? {
            // Also covers a prior save whose publication succeeded but parent sync failed.
            sync_parent(&self.data_path.join(JOURNAL_NAME))?;
            self.finish_batch(&journal)?;
            info!("Recovered player data save in {}", self.data_path.display());
        }
        Ok(())
    }

    fn finish_batch(&self, journal: &[u8]) -> Result<(), PlayerDataError> {
        let records = read_records(journal)?;
        for (uuid, data) in records {
            write_file_atomically(&self.get_player_data_path(&uuid), data, true)?;
        }
        let path = self.data_path.join(JOURNAL_NAME);
        fs::remove_file(&path)?;
        sync_parent(&path)?;
        Ok(())
    }
}

fn encode_records(players: BTreeMap<Uuid, NbtCompound>) -> Result<Vec<u8>, PlayerDataError> {
    let mut journal = JOURNAL_MAGIC.to_vec();
    for (uuid, nbt) in players {
        let data = write_gzip_compound_tag_to_bytes(nbt)
            .map_err(|err| PlayerDataError::Nbt(err.to_string()))?;
        let length = u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "player data is too large"))?;
        journal.put_u128(uuid.as_u128());
        journal.put_u32(length);
        journal.extend_from_slice(&data);
    }
    let checksum = xxh64(&journal, 0);
    journal.put_u64(checksum);
    Ok(journal)
}

fn read_records(journal: &[u8]) -> io::Result<BTreeMap<Uuid, &[u8]>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid player save journal");
    let (payload, checksum) = journal
        .split_last_chunk::<CHECKSUM_SIZE>()
        .ok_or_else(invalid)?;
    if *checksum != xxh64(payload, 0).to_be_bytes() {
        return Err(invalid());
    }
    let mut payload = payload.strip_prefix(JOURNAL_MAGIC).ok_or_else(invalid)?;
    let mut records = BTreeMap::new();
    while !payload.is_empty() {
        if payload.len() < RECORD_HEADER_SIZE {
            return Err(invalid());
        }
        let uuid = Uuid::from_u128(payload.get_u128());
        let length = payload.get_u32() as usize;
        let (data, rest) = payload.split_at_checked(length).ok_or_else(invalid)?;
        if data.is_empty() || records.insert(uuid, data).is_some() {
            return Err(invalid());
        }
        payload = rest;
    }
    if records.is_empty() {
        return Err(invalid());
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(count: i32) -> NbtCompound {
        let mut nbt = NbtCompound::new();
        nbt.put_int("inventory_count", count);
        nbt
    }

    #[test]
    fn partially_applied_batch_remains_visible_and_replays_after_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PlayerDataStorage::new(dir.path(), true);
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        store.save_player_data(&first, inventory(3)).unwrap();
        let blocked = store.get_player_data_path(&second);
        fs::create_dir(&blocked).unwrap();

        assert!(
            store
                .save_batch([(first, inventory(7)), (second, inventory(9))])
                .is_err()
        );
        let pending = dir.path().join(JOURNAL_NAME);
        assert!(pending.is_file());
        assert_eq!(
            store.load_player_data(&first).unwrap(),
            (true, inventory(7))
        );
        assert_eq!(
            store.load_player_data(&second).unwrap(),
            (true, inventory(9))
        );
        drop(store);

        fs::remove_dir(blocked).unwrap();
        let reopened = PlayerDataStorage::new(dir.path(), true);
        reopened.recover().unwrap();
        reopened.recover().unwrap();
        assert!(!pending.exists());
        assert_eq!(
            reopened.load_player_data(&first).unwrap(),
            (true, inventory(7))
        );
        assert_eq!(
            reopened.load_player_data(&second).unwrap(),
            (true, inventory(9))
        );
    }

    #[test]
    fn damaged_journal_is_rejected_before_replacing_player_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = PlayerDataStorage::new(dir.path(), true);
        let uuid = Uuid::from_u128(1);
        store.save_player_data(&uuid, inventory(3)).unwrap();
        let original = fs::read(store.get_player_data_path(&uuid)).unwrap();
        let mut damaged = encode_records(BTreeMap::from([(uuid, inventory(4))])).unwrap();
        damaged[RECORD_HEADER_SIZE] ^= 1;
        fs::write(dir.path().join(JOURNAL_NAME), &damaged).unwrap();

        assert!(store.recover().is_err());
        assert!(store.save_player_data(&uuid, inventory(5)).is_err());
        assert_eq!(
            fs::read(store.get_player_data_path(&uuid)).unwrap(),
            original
        );
        assert_eq!(fs::read(dir.path().join(JOURNAL_NAME)).unwrap(), damaged);
    }
}
