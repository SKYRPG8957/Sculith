use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use bytes::{Buf, BufMut, Bytes};
use rustc_hash::FxHashMap;
use ruzstd::{
    decoding::StreamingDecoder,
    encoding::{CompressionLevel, compress_to_vec},
};
use sculith_util::math::vector2::Vector2;
use tracing::warn;
use xxhash_rust::xxh64::xxh64;

use crate::chunk::{
    ChunkReadingError, ChunkSerializingError, ChunkWritingError,
    format::anvil::{CHUNK_COUNT, SectorBitmap, SingleChunkDataSerializer},
    io::{RegionFile, read_exact_at, sync_parent, write_all_at, write_file_atomically},
};

use super::header_journal as journal;

/// Magic signature and format version (`"PUMP"` + version 1).
const SIGNATURE: [u8; 8] = *b"PUMP\x00\x00\x00\x01";

/// Magic prefix for a native `.pump` binary chunk payload.
pub const PUMP_CHUNK_MAGIC: [u8; 4] = *b"PCHK";

/// Per-chunk frame header: `u32` uncompressed byte length + `u64` `XXH64` checksum.
const CHUNK_FRAME_HEADER_BYTES: usize = 12;

/// Upper bound on a single decompressed chunk payload (64 MiB) to guard against corrupt headers.
const MAX_DECOMPRESSED_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// Sector alignment in bytes. 256-byte sectors keep 32-bit sector offsets able to address up to
/// 1 TiB per region file while wasting 16x less tail padding than Anvil's 4 KiB sectors.
const SECTOR_BYTES: usize = 256;

/// Sector 0 holds the 256-byte superblock; sectors 1..33 hold 1024 `(offset: u32, length: u32)`
/// entries (8192 bytes).
const HEADER_SECTORS: u32 = 33;
const HEADER_BYTES: usize = HEADER_SECTORS as usize * SECTOR_BYTES;

const fn sector_count(byte_len: u32) -> u32 {
    byte_len.div_ceil(SECTOR_BYTES as u32)
}

/// A `.pump` region file.
///
/// Staged chunks are compressed and written into free sectors before the header is published.
/// Opening the region replays a header publication interrupted by a process crash.
pub struct PumpFile {
    path: PathBuf,
    region: Vector2<i32>,
    file: Option<File>,
    file_len: u64,
    offsets: Box<[u32]>,
    lengths: Box<[u32]>,
    used_sectors: SectorBitmap,
    staged: FxHashMap<usize, Bytes>,
    unsynced: bool,
    recovery_needed: bool,
}

impl PumpFile {
    fn load_header(&mut self, header: &[u8], file_len: u64) -> Result<(), ChunkReadingError> {
        if file_len != 0
            && (file_len < HEADER_BYTES as u64 || header[..SIGNATURE.len()] != SIGNATURE)
        {
            return Err(ChunkReadingError::InvalidHeader);
        }
        self.offsets.fill(0);
        self.lengths.fill(0);
        self.used_sectors = SectorBitmap::default();
        self.used_sectors.set(0, HEADER_SECTORS, true);
        self.file_len = file_len;
        let mut table = &header[SECTOR_BYTES..];
        for index in 0..CHUNK_COUNT {
            let offset = table.get_u32();
            let len = table.get_u32();
            if offset == 0 && len == 0 {
                continue;
            }
            let count = sector_count(len);
            let end_byte = u64::from(offset) * SECTOR_BYTES as u64 + u64::from(len);
            if offset < HEADER_SECTORS || len == 0 || end_byte > file_len {
                warn!(
                    "Pump region {} has an invalid entry at index {index}: offset {offset}, len {len}",
                    self.path.display()
                );
                continue;
            }
            self.offsets[index] = offset;
            self.lengths[index] = len;
            self.used_sectors.set(offset, count, true);
        }
        Ok(())
    }

    fn recover_before_write(&mut self) -> io::Result<()> {
        if self.recovery_needed {
            let file = self
                .file
                .as_ref()
                .ok_or_else(|| io::Error::other("region file is not open"))?;
            let (header, file_len) =
                journal::read(&self.path, self.region, file, HEADER_BYTES, true)?;
            self.load_header(&header, file_len)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            self.recovery_needed = false;
        }
        Ok(())
    }

    fn publish_header(
        &mut self,
        header: &[u8],
        sync: bool,
        sync_file: impl Fn(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("region file is not open"))?;
        journal::publish(
            &self.path,
            self.region,
            file,
            header,
            sync,
            sync_file,
            &mut self.recovery_needed,
        )
    }

    fn flush_with_sync(
        &mut self,
        sync: bool,
        sync_file: impl Fn(&File) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.recover_before_write()?;
        if self.staged.is_empty() {
            if sync
                && self.unsynced
                && let Some(file) = &self.file
            {
                sync_file(file)?;
                sync_parent(&self.path)?;
                self.unsynced = false;
            }
            return Ok(());
        }

        let mut used_sectors = self.used_sectors.clone();
        let mut free_runs = self.used_sectors.free_runs();
        let mut offsets = self.offsets.clone();
        let mut lengths = self.lengths.clone();
        let mut freed = Vec::new();
        let mut writes = Vec::with_capacity(self.staged.len());

        let mut staged: Vec<_> = self.staged.iter().collect();
        staged.sort_unstable_by_key(|(index, _)| **index);
        for (&index, chunk) in staged {
            let len = u32::try_from(chunk.len()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "chunk too large")
            })?;
            if len == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "encoded chunk cannot be empty",
                ));
            }
            let count = sector_count(len);
            let padded_len = count as usize * SECTOR_BYTES;
            let mut record = Vec::with_capacity(padded_len);
            record.extend_from_slice(chunk);
            record.resize(padded_len, 0);

            let offset = free_runs.allocate(count);
            used_sectors.set(offset, count, true);
            if offsets[index] != 0 {
                freed.push((offsets[index], sector_count(lengths[index])));
            }
            offsets[index] = offset;
            lengths[index] = len;
            writes.push((offset, record));
        }

        writes.sort_unstable_by_key(|(offset, _)| *offset);
        let file = self.file()?;
        let mut file_end = 0;
        let mut run_start = 0;
        let mut run: Vec<u8> = Vec::new();
        for (offset, record) in writes {
            let position = u64::from(offset) * SECTOR_BYTES as u64;
            if !run.is_empty() && run_start + run.len() as u64 != position {
                write_all_at(file, &run, run_start)?;
                run.clear();
            }
            if run.is_empty() {
                run_start = position;
            }
            run.extend_from_slice(&record);
            file_end = position + record.len() as u64;
        }
        if !run.is_empty() {
            write_all_at(file, &run, run_start)?;
        }

        let header = self.header_bytes(&offsets, &lengths);
        self.publish_header(&header, sync, sync_file)?;
        self.offsets = offsets;
        self.lengths = lengths;
        for (offset, count) in freed {
            used_sectors.set(offset, count, false);
        }
        self.file_len = self.file_len.max(file_end).max(HEADER_BYTES as u64);
        self.used_sectors = used_sectors;
        self.unsynced = !sync;
        self.truncate_unused_tail();
        self.staged.clear();
        Ok(())
    }

    fn truncate_unused_tail(&mut self) {
        let used_len = u64::from(self.used_sectors.end()) * SECTOR_BYTES as u64;
        if used_len >= self.file_len {
            return;
        }
        let Some(file) = &self.file else {
            return;
        };
        match file.set_len(used_len) {
            Ok(()) => self.file_len = used_len,
            Err(err) => warn!("Failed to shrink {}: {err}", self.path.display()),
        }
    }

    fn header_bytes(&self, offsets: &[u32], lengths: &[u32]) -> Vec<u8> {
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.put_slice(&SIGNATURE);
        header.put_i32(self.region.x);
        header.put_i32(self.region.y);
        header.resize(SECTOR_BYTES, 0);
        for (offset, length) in offsets.iter().zip(lengths) {
            header.put_u32(*offset);
            header.put_u32(*length);
        }
        header
    }

    fn file(&mut self) -> std::io::Result<&File> {
        if self.file.is_none() || self.file_len == 0 {
            // A first save can be interrupted while writing its payloads. Publish an empty
            // header before those writes so the region can reopen even without a journal.
            self.file = None;
            let header = self.header_bytes(&self.offsets, &self.lengths);
            write_file_atomically(&self.path, &header, false)?;
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            self.file_len = file.metadata()?.len();
            self.file = Some(file);
        }
        self.file
            .as_ref()
            .ok_or_else(|| std::io::Error::other("region file is not open"))
    }
}

impl RegionFile for PumpFile {
    type Config = ();
    type Encoded = Bytes;
    type Raw = Bytes;

    // Writes only touch the changed chunks, so there is nothing to gain from waiting.
    const FLUSH_DELAY: Duration = Duration::ZERO;
    const CACHE_CAPACITY: usize = 64;

    fn file_name(region: Vector2<i32>) -> String {
        format!("r.{}.{}.pump", region.x, region.y)
    }

    fn open(path: &Path, region: Vector2<i32>) -> Result<Self, ChunkReadingError> {
        let mut this = Self {
            path: path.to_path_buf(),
            region,
            file: None,
            file_len: 0,
            offsets: vec![0; CHUNK_COUNT].into_boxed_slice(),
            lengths: vec![0; CHUNK_COUNT].into_boxed_slice(),
            used_sectors: SectorBitmap::default(),
            staged: FxHashMap::default(),
            unsynced: false,
            recovery_needed: false,
        };
        this.used_sectors.set(0, HEADER_SECTORS, true);

        let Some(file) = journal::open_file(path, true).map_err(ChunkReadingError::IoError)? else {
            return Ok(this);
        };
        let (header, file_len) = journal::read(path, region, &file, HEADER_BYTES, true)
            .map_err(ChunkReadingError::IoError)?;
        this.load_header(&header, file_len)?;
        this.file = Some(file);
        Ok(this)
    }

    fn has_chunk(&self, index: usize) -> bool {
        self.staged.contains_key(&index) || (self.offsets[index] != 0 && self.lengths[index] != 0)
    }

    fn delete_chunk(&mut self, index: usize) -> std::io::Result<bool> {
        self.recover_before_write()?;
        let offset = self.offsets[index];
        let len = self.lengths[index];
        if offset == 0 || len == 0 {
            return Ok(self.staged.remove(&index).is_some());
        }
        if self.file.is_some() {
            let mut offsets = self.offsets.clone();
            let mut lengths = self.lengths.clone();
            offsets[index] = 0;
            lengths[index] = 0;
            let header = self.header_bytes(&offsets, &lengths);
            self.publish_header(&header, false, File::sync_data)?;
            self.unsynced = true;
        }
        self.staged.remove(&index);
        self.offsets[index] = 0;
        self.lengths[index] = 0;
        self.used_sectors.set(offset, sector_count(len), false);
        self.truncate_unused_tail();
        Ok(true)
    }

    fn read_chunk(&self, index: usize) -> Result<Option<Bytes>, ChunkReadingError> {
        if let Some(staged) = self.staged.get(&index) {
            return Ok(Some(staged.clone()));
        }
        let offset = self.offsets[index];
        let len = self.lengths[index];
        let Some(file) = &self.file else {
            return Ok(None);
        };
        if offset == 0 || len == 0 {
            return Ok(None);
        }

        let start = u64::from(offset) * SECTOR_BYTES as u64;
        let mut buf = vec![0; len as usize];
        read_exact_at(file, &mut buf, start).map_err(ChunkReadingError::IoError)?;
        Ok(Some(buf.into()))
    }

    fn decode_chunk(raw: Bytes) -> Result<Bytes, ChunkReadingError> {
        if raw.len() < CHUNK_FRAME_HEADER_BYTES {
            return Err(ChunkReadingError::IoError(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        let mut header = &raw[..CHUNK_FRAME_HEADER_BYTES];
        let uncompressed_len = header.get_u32() as usize;
        let expected_checksum = header.get_u64();
        if uncompressed_len > MAX_DECOMPRESSED_CHUNK_BYTES {
            return Err(ChunkReadingError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("pump chunk uncompressed length {uncompressed_len} exceeds limit"),
            )));
        }

        let mut decoder = StreamingDecoder::new(&raw[CHUNK_FRAME_HEADER_BYTES..])
            .map_err(|e| ChunkReadingError::IoError(std::io::Error::other(e.to_string())))?;
        let mut decompressed = Vec::with_capacity(uncompressed_len);
        decoder
            .read_to_end(&mut decompressed)
            .map_err(ChunkReadingError::IoError)?;
        if decompressed.len() != uncompressed_len || xxh64(&decompressed, 0) != expected_checksum {
            return Err(ChunkReadingError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pump chunk checksum or length mismatch",
            )));
        }
        Ok(decompressed.into())
    }

    fn encode_chunk(nbt: Bytes, _config: &()) -> Result<Bytes, ChunkWritingError> {
        let uncompressed_len = u32::try_from(nbt.len()).map_err(|_| {
            ChunkWritingError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "chunk payload too large",
            ))
        })?;
        let checksum = xxh64(&nbt, 0);
        let compressed = compress_to_vec(&nbt[..], CompressionLevel::Fastest);
        let mut out = Vec::with_capacity(CHUNK_FRAME_HEADER_BYTES + compressed.len());
        out.put_u32(uncompressed_len);
        out.put_u64(checksum);
        out.extend_from_slice(&compressed);
        Ok(out.into())
    }

    fn serialize_chunk<D: SingleChunkDataSerializer>(
        chunk: &D,
    ) -> Result<Bytes, ChunkSerializingError> {
        chunk.to_pump_bytes()
    }

    fn stage_chunk(&mut self, index: usize, chunk: Bytes) {
        self.staged.insert(index, chunk);
    }

    fn has_staged(&self) -> bool {
        !self.staged.is_empty() || self.recovery_needed
    }

    fn has_unsynced_writes(&self) -> bool {
        self.unsynced
    }

    fn flush(&mut self, sync: bool) -> std::io::Result<()> {
        self.flush_with_sync(sync, File::sync_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn noise(seed: u64, len: usize) -> Bytes {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn assert_no_overlap(region: &PumpFile) {
        let mut taken = vec![false; region.used_sectors.end() as usize];
        for index in 0..CHUNK_COUNT {
            let offset = region.offsets[index];
            let len = region.lengths[index];
            if offset == 0 {
                continue;
            }
            for sector in offset..offset + sector_count(len) {
                assert!(sector >= HEADER_SECTORS, "chunk inside the header");
                assert!(!taken[sector as usize], "sector {sector} used twice");
                taken[sector as usize] = true;
            }
        }
    }

    #[test]
    fn pump_file_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("r.0.0.pump");

        let mut pump_file = PumpFile::open(&file_path, Vector2::new(0, 0)).unwrap();
        let nbt = Bytes::from_static(&[1, 2, 3]);
        pump_file.stage_chunk(0, PumpFile::encode_chunk(nbt.clone(), &()).unwrap());
        pump_file.flush(false).unwrap();

        let read_file = PumpFile::open(&file_path, Vector2::new(0, 0)).unwrap();
        let raw = read_file.read_chunk(0).unwrap().unwrap();
        assert_eq!(PumpFile::decode_chunk(raw).unwrap(), nbt);
        assert!(read_file.read_chunk(1).unwrap().is_none());
    }

    #[test]
    fn corrupt_chunk_payload_fails_checksum() {
        let payload = Bytes::from(vec![42u8; 256]);
        let mut encoded = PumpFile::encode_chunk(payload, &()).unwrap().to_vec();
        encoded[4] ^= 0xFF;
        assert!(PumpFile::decode_chunk(encoded.into()).is_err());
    }

    #[test]
    fn a_chunk_pointing_into_the_header_is_skipped() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("r.0.0.pump");

        let mut header = Vec::with_capacity(HEADER_BYTES + SECTOR_BYTES);
        header.put_slice(&SIGNATURE);
        header.resize(SECTOR_BYTES, 0);
        header.put_u32(HEADER_SECTORS - 1);
        header.put_u32(16);
        header.resize(HEADER_BYTES + SECTOR_BYTES, 0);
        std::fs::write(&file_path, header).unwrap();

        let region = PumpFile::open(&file_path, Vector2::new(0, 0)).unwrap();
        assert!(region.read_chunk(0).unwrap().is_none());
    }

    #[test]
    fn sync_failure_keeps_live_sectors_allocated_for_retry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("r.0.0.pump");
        let mut region = PumpFile::open(&path, Vector2::new(0, 0)).unwrap();
        region.stage_chunk(
            0,
            PumpFile::encode_chunk(Bytes::from(vec![1; 64]), &()).unwrap(),
        );
        region.flush(false).unwrap();
        let expected = Bytes::from(vec![2; 64]);
        region.stage_chunk(0, PumpFile::encode_chunk(expected.clone(), &()).unwrap());

        let syncs = std::cell::Cell::new(0);
        assert!(
            region
                .flush_with_sync(true, |file| {
                    syncs.set(syncs.get() + 1);
                    if syncs.get() == 2 {
                        Err(std::io::Error::other("header sync failed"))
                    } else {
                        file.sync_data()
                    }
                })
                .is_err()
        );
        assert!(region.has_staged());
        assert!(super::journal::path(&path).is_file());
        region.flush(false).unwrap();
        assert!(!super::journal::path(&path).exists());
        region.stage_chunk(
            1,
            PumpFile::encode_chunk(Bytes::from(vec![3; 64]), &()).unwrap(),
        );
        region.flush(false).unwrap();

        let reopened = PumpFile::open(&path, Vector2::new(0, 0)).unwrap();
        assert_eq!(
            PumpFile::decode_chunk(reopened.read_chunk(0).unwrap().unwrap()).unwrap(),
            expected
        );
        assert_no_overlap(&reopened);
    }

    #[test]
    fn an_interrupted_first_payload_leaves_a_readable_empty_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("r.0.0.pump");
        let pos = Vector2::new(0, 0);
        let mut region = PumpFile::open(&path, pos).unwrap();
        let file = region.file().unwrap();
        crate::chunk::io::write_all_at(file, b"unpublished payload", HEADER_BYTES as u64).unwrap();
        drop(region);

        let mut recovered = PumpFile::open(&path, pos).unwrap();
        assert!(recovered.read_chunk(0).unwrap().is_none());
        let expected = Bytes::from_static(b"committed payload");
        recovered.stage_chunk(0, PumpFile::encode_chunk(expected.clone(), &()).unwrap());
        recovered.flush(true).unwrap();
        assert_eq!(
            PumpFile::decode_chunk(recovered.read_chunk(0).unwrap().unwrap()).unwrap(),
            expected,
        );
    }

    #[test]
    fn rewritten_chunks_reuse_sectors_and_survive_reopening() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("r.-1.2.pump");
        let mut expected = vec![None; CHUNK_COUNT];
        let mut region = PumpFile::open(&path, Vector2::new(-1, 2)).unwrap();

        for round in 0..6u64 {
            for index in (0..CHUNK_COUNT).step_by(37) {
                let len = 100 + ((index as u64 * 7919 + round * 104_729) % 4_000) as usize;
                let nbt = noise(index as u64 ^ (round << 32), len);
                region.stage_chunk(index, PumpFile::encode_chunk(nbt.clone(), &()).unwrap());
                expected[index] = Some(nbt);
            }
            region.flush(false).unwrap();
            assert_no_overlap(&region);
        }
        let file_len = std::fs::metadata(&path).unwrap().len();

        let reopened = PumpFile::open(&path, Vector2::new(-1, 2)).unwrap();
        for (index, nbt) in expected.iter().enumerate() {
            let read = reopened
                .read_chunk(index)
                .unwrap()
                .map(|raw| PumpFile::decode_chunk(raw).unwrap());
            assert_eq!(read.as_ref(), nbt.as_ref(), "chunk {index}");
        }
        let live: u64 = expected.iter().flatten().map(|nbt| nbt.len() as u64).sum();
        assert!(
            file_len < live * 3,
            "file {file_len} bytes for {live} live bytes"
        );
    }
}
