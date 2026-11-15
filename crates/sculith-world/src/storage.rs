//! Bounded, checksummed region snapshots and conservative atomic file saves.
//!
//! Format/registry/flat-generator versions are independent of Minecraft protocol
//! IDs. CRC32 detects accidental corruption; it is not a signature or a trust
//! boundary against somebody who can modify server files.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    ChunkData, ChunkPos, ChunkSnapshot, FlatGenerator, MAX_REGION_CHUNKS, Region, RegionBounds,
    RegionId, RegionSnapshot, SECTION_COUNT, Section, WorldError,
};

pub const FORMAT_VERSION: u16 = 1;
pub const BLOCK_REGISTRY_VERSION: u16 = 1;
pub const FLAT_GENERATOR_VERSION: u16 = 1;
const MAGIC: &[u8; 8] = b"SCULITHR";
const LOCK_MAGIC: &[u8; 16] = b"SCULITH-LOCK-v1\n";
const MAX_SECTION_BYTES: usize = 1544;
const MIN_FILE_BYTES: usize = 52;
pub const MAX_SNAPSHOT_BYTES: usize =
    MIN_FILE_BYTES + MAX_REGION_CHUNKS * (16 + SECTION_COUNT * (2 + MAX_SECTION_BYTES));

#[derive(Debug)]
pub enum StorageError {
    Io(io::Error),
    Invalid(&'static str),
    World(WorldError),
    Conflict(&'static str),
    ConcurrentChange,
    /// Rename committed a valid snapshot, but directory durability could not be
    /// confirmed. A previous snapshot is retained at `recovery` when one existed.
    CommittedButUnsynced {
        source: io::Error,
        recovery: Option<PathBuf>,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "snapshot I/O: {error}"),
            Self::Invalid(reason) => write!(f, "invalid Sculith region snapshot: {reason}"),
            Self::World(error) => write!(f, "invalid snapshot world state: {error}"),
            Self::Conflict(reason) => write!(f, "snapshot save refused: {reason}"),
            Self::ConcurrentChange => write!(f, "snapshot destination changed during save"),
            Self::CommittedButUnsynced { source, recovery } => {
                write!(f, "snapshot replaced, but directory sync failed: {source}")?;
                if let Some(path) = recovery {
                    write!(f, "; previous snapshot retained at {}", path.display())?;
                }
                Ok(())
            }
        }
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) | Self::CommittedButUnsynced { source: error, .. } => Some(error),
            Self::World(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<WorldError> for StorageError {
    fn from(error: WorldError) -> Self {
        Self::World(error)
    }
}
pub type Result<T> = std::result::Result<T, StorageError>;

/// Pure bounded serialization. Call this on an I/O worker after owner capture.
pub fn encode_snapshot(snapshot: &RegionSnapshot) -> Result<Vec<u8>> {
    validate_snapshot(snapshot)?;
    let mut bytes = Vec::with_capacity(MIN_FILE_BYTES + snapshot.chunks.len() * 2048);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&BLOCK_REGISTRY_VERSION.to_le_bytes());
    bytes.extend_from_slice(&FLAT_GENERATOR_VERSION.to_le_bytes());
    bytes.extend_from_slice(&snapshot.id.0.to_le_bytes());
    for value in [
        snapshot.bounds.min.x,
        snapshot.bounds.min.z,
        snapshot.bounds.max_exclusive.x,
        snapshot.bounds.max_exclusive.z,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&snapshot.revision.to_le_bytes());
    bytes.extend_from_slice(&(snapshot.chunks.len() as u16).to_le_bytes());
    for chunk in &snapshot.chunks {
        bytes.extend_from_slice(&chunk.position().x.to_le_bytes());
        bytes.extend_from_slice(&chunk.position().z.to_le_bytes());
        bytes.extend_from_slice(&chunk.revision().to_le_bytes());
        for (_, section) in chunk.sections() {
            let record = section.encode();
            if !(2..=MAX_SECTION_BYTES).contains(&record.len()) {
                return Err(StorageError::Invalid(
                    "section record exceeds registry bound",
                ));
            }
            bytes.extend_from_slice(&(record.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&record);
        }
    }
    if bytes.len() + 4 > MAX_SNAPSHOT_BYTES {
        return Err(StorageError::Invalid("snapshot exceeds size bound"));
    }
    bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
    Ok(bytes)
}

/// Validates the entire file before constructing an owner. No file I/O is done.
pub fn decode_snapshot(bytes: &[u8]) -> Result<RegionSnapshot> {
    if !(MIN_FILE_BYTES..=MAX_SNAPSHOT_BYTES).contains(&bytes.len()) {
        return Err(StorageError::Invalid("snapshot byte length out of bounds"));
    }
    let payload_end = bytes.len() - 4;
    let expected = u32::from_le_bytes(bytes[payload_end..].try_into().expect("four-byte trailer"));
    if crc32fast::hash(&bytes[..payload_end]) != expected {
        return Err(StorageError::Invalid("checksum mismatch"));
    }
    let mut reader = Reader {
        remaining: &bytes[..payload_end],
    };
    if reader.take(8)? != MAGIC {
        return Err(StorageError::Invalid("foreign file magic"));
    }
    if reader.u16()? != FORMAT_VERSION {
        return Err(StorageError::Invalid("unsupported file format version"));
    }
    if reader.u16()? != BLOCK_REGISTRY_VERSION {
        return Err(StorageError::Invalid("unsupported internal block registry"));
    }
    if reader.u16()? != FLAT_GENERATOR_VERSION {
        return Err(StorageError::Invalid("unsupported flat terrain version"));
    }
    let id = RegionId(reader.u64()?);
    let min = ChunkPos {
        x: reader.i32()?,
        z: reader.i32()?,
    };
    let max = ChunkPos {
        x: reader.i32()?,
        z: reader.i32()?,
    };
    let bounds = RegionBounds::new(min, max)?;
    let revision = reader.u64()?;
    let count = usize::from(reader.u16()?);
    if count > MAX_REGION_CHUNKS {
        return Err(StorageError::Invalid("too many loaded chunks"));
    }
    let mut chunks = Vec::with_capacity(count);
    for _ in 0..count {
        let position = ChunkPos {
            x: reader.i32()?,
            z: reader.i32()?,
        };
        let revision = reader.u64()?;
        let mut sections = Vec::with_capacity(SECTION_COUNT);
        for _ in 0..SECTION_COUNT {
            let length = usize::from(reader.u16()?);
            if !(2..=MAX_SECTION_BYTES).contains(&length) {
                return Err(StorageError::Invalid("section byte length out of bounds"));
            }
            sections.push(Arc::new(Section::decode(reader.take(length)?)?));
        }
        let sections = sections
            .try_into()
            .map_err(|_| StorageError::Invalid("incorrect section count"))?;
        chunks.push(ChunkSnapshot {
            data: Arc::new(ChunkData {
                position,
                revision,
                sections,
            }),
        });
    }
    if !reader.remaining.is_empty() {
        return Err(StorageError::Invalid("trailing bytes"));
    }
    let snapshot = RegionSnapshot {
        id,
        bounds,
        revision,
        chunks,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn validate_snapshot(snapshot: &RegionSnapshot) -> Result<()> {
    RegionBounds::new(snapshot.bounds.min, snapshot.bounds.max_exclusive)?;
    if snapshot.chunks.len() > MAX_REGION_CHUNKS {
        return Err(StorageError::Invalid("too many loaded chunks"));
    }
    let generator = FlatGenerator::default();
    let mut seen = BTreeSet::new();
    let mut previous = None;
    let mut revision_sum = 0u128;
    for chunk in &snapshot.chunks {
        let position = chunk.position();
        position.validate()?;
        if !snapshot.bounds.contains(position) {
            return Err(StorageError::Invalid("chunk outside region ownership"));
        }
        if !seen.insert(position) {
            return Err(StorageError::Invalid("duplicate chunk coordinates"));
        }
        if previous.is_some_and(|last| last >= position) {
            return Err(StorageError::Invalid(
                "chunks are not in canonical coordinate order",
            ));
        }
        previous = Some(position);
        if chunk.revision() > snapshot.revision {
            return Err(StorageError::Invalid(
                "chunk revision exceeds region revision",
            ));
        }
        revision_sum += u128::from(chunk.revision());
        if chunk.revision() == 0 {
            let flat = generator.snapshot(position)?;
            if chunk
                .sections()
                .zip(flat.sections())
                .any(|((_, a), (_, b))| a.encode() != b.encode())
            {
                return Err(StorageError::Invalid(
                    "revision-zero chunk differs from initial terrain",
                ));
            }
        }
    }
    if revision_sum < u128::from(snapshot.revision) {
        return Err(StorageError::Invalid(
            "region revision cannot be accounted for by loaded chunks",
        ));
    }
    Ok(())
}

struct Reader<'a> {
    remaining: &'a [u8],
}
impl Reader<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8]> {
        if self.remaining.len() < count {
            return Err(StorageError::Invalid("truncated snapshot"));
        }
        let (value, remaining) = self.remaining.split_at(count);
        self.remaining = remaining;
        Ok(value)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two-byte field"),
        ))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("four-byte field"),
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight-byte field"),
        ))
    }
}

/// Load validates a regular, non-symlink file, without creating or changing files.
pub fn load(path: impl AsRef<Path>) -> Result<Region> {
    let (bytes, _) = read_regular(path.as_ref())?;
    Ok(decode_snapshot(&bytes)?.restore())
}

/// Convenience API. Prefer `owner.capture()` followed by `save_snapshot` on a
/// blocking I/O worker so the region owner does not perform file operations.
pub fn save(path: impl AsRef<Path>, region: &Region) -> Result<()> {
    save_snapshot(path, &region.capture())
}

/// Saves through a same-directory tempfile, fsync and atomic rename/no-clobber.
/// The directory must be server-managed. A persistent format-marked OS file lock
/// coordinates cooperating writers; no portable API provides conditional rename
/// against arbitrary non-cooperating processes with the same filesystem rights.
pub fn save_snapshot(path: impl AsRef<Path>, snapshot: &RegionSnapshot) -> Result<()> {
    let bytes = encode_snapshot(snapshot)?;
    let destination = Destination::resolve(path.as_ref())?;
    // Refuse foreign paths before creating even our lock sidecar.
    let _ = existing(&destination.path)?;
    let _lock = acquire_lock(&destination.parent)?;
    let previous = existing(&destination.path)?;
    if let Some((old_bytes, _)) = &previous {
        let old = decode_snapshot(old_bytes)?;
        check_forward_update(&old, snapshot)?;
        if *old_bytes == bytes {
            return Ok(());
        }
    }
    let mut staged = tempfile::Builder::new()
        .prefix(".sculith-region-new-")
        .tempfile_in(&destination.parent)?;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    let backup = if let Some((old_bytes, _)) = &previous {
        let mut backup = tempfile::Builder::new()
            .prefix(".sculith-region-recovery-")
            .tempfile_in(&destination.parent)?;
        backup.write_all(old_bytes)?;
        backup.as_file().sync_all()?;
        Some(backup)
    } else {
        None
    };
    match &previous {
        Some((_, identity)) => {
            let current = fs::symlink_metadata(&destination.path)
                .map_err(|_| StorageError::ConcurrentChange)?;
            if !identity.matches(&current) {
                return Err(StorageError::ConcurrentChange);
            }
            staged
                .persist(&destination.path)
                .map_err(|error| StorageError::Io(error.error))?;
        }
        None => {
            staged
                .persist_noclobber(&destination.path)
                .map_err(|error| StorageError::Io(error.error))?;
        }
    }
    if let Err(source) = sync_directory(&destination.parent) {
        let recovery = backup.map(|file| match file.keep() {
            Ok((_, path)) => path,
            Err(mut error) => {
                let path = error.file.path().to_path_buf();
                error.file.disable_cleanup(true);
                path
            }
        });
        return Err(StorageError::CommittedButUnsynced { source, recovery });
    }
    // Target file + rename are durable before old recovery data is discarded.
    drop(backup);
    Ok(())
}

struct Destination {
    path: PathBuf,
    parent: PathBuf,
}
impl Destination {
    fn resolve(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or(StorageError::Invalid("destination must name a file"))?;
        if name == ".sculith-snapshot.lock" {
            return Err(StorageError::Invalid(
                "destination uses the reserved lock filename",
            ));
        }
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent.canonicalize()?;
        if !parent.is_dir() {
            return Err(StorageError::Invalid("snapshot parent is not a directory"));
        }
        Ok(Self {
            path: parent.join(name),
            parent,
        })
    }
}

struct Identity {
    metadata: Metadata,
}
impl Identity {
    fn matches(&self, other: &Metadata) -> bool {
        if !other.is_file()
            || other.file_type().is_symlink()
            || self.metadata.len() != other.len()
            || self.metadata.modified().ok() != other.modified().ok()
        {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.metadata.dev() == other.dev() && self.metadata.ino() == other.ino()
        }
        #[cfg(not(unix))]
        {
            self.metadata.created().ok() == other.created().ok()
        }
    }
}

fn open_no_follow(path: &Path, writable: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.file_type().is_symlink() {
        return Err(StorageError::Invalid(
            "snapshot/lock path is not a regular file",
        ));
    }
    Ok(file)
}

fn read_regular(path: &Path) -> Result<(Vec<u8>, Identity)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StorageError::Invalid(
            "snapshot path is a symlink or non-regular file",
        ));
    }
    let file = open_no_follow(path, false)?;
    let identity = Identity {
        metadata: file.metadata()?,
    };
    if !identity.matches(&metadata) {
        return Err(StorageError::ConcurrentChange);
    }
    if !(MIN_FILE_BYTES as u64..=MAX_SNAPSHOT_BYTES as u64).contains(&identity.metadata.len()) {
        return Err(StorageError::Invalid("snapshot file length out of bounds"));
    }
    let mut bytes = Vec::with_capacity(identity.metadata.len() as usize);
    file.take(MAX_SNAPSHOT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(StorageError::Invalid("snapshot grew beyond size bound"));
    }
    Ok((bytes, identity))
}

fn existing(path: &Path) -> Result<Option<(Vec<u8>, Identity)>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let (bytes, identity) = read_regular(path)?;
            decode_snapshot(&bytes)?;
            if identity.metadata.permissions().readonly() {
                return Err(StorageError::Conflict("existing snapshot is read-only"));
            }
            Ok(Some((bytes, identity)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn acquire_lock(parent: &Path) -> Result<File> {
    let path = parent.join(".sculith-snapshot.lock");
    if let Err(error) = fs::symlink_metadata(&path) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error.into());
        }
        let mut staged = tempfile::Builder::new()
            .prefix(".sculith-lock-init-")
            .tempfile_in(parent)?;
        staged.write_all(LOCK_MAGIC)?;
        staged.as_file().sync_all()?;
        match staged.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error.into()),
        }
    }
    let mut file = open_no_follow(&path, true)?;
    file.lock()?;
    if file.metadata()?.len() != LOCK_MAGIC.len() as u64 {
        return Err(StorageError::Invalid("foreign snapshot lock file"));
    }
    let mut bytes = [0; LOCK_MAGIC.len()];
    file.read_exact(&mut bytes)?;
    if &bytes != LOCK_MAGIC {
        return Err(StorageError::Invalid("foreign snapshot lock file"));
    }
    Ok(file)
}

fn check_forward_update(old: &RegionSnapshot, new: &RegionSnapshot) -> Result<()> {
    if old.id != new.id || old.bounds != new.bounds {
        return Err(StorageError::Conflict(
            "destination belongs to another region",
        ));
    }
    if old.revision > new.revision {
        return Err(StorageError::Conflict(
            "stale snapshot would move revision backwards",
        ));
    }
    for old_chunk in old.chunks.iter().filter(|chunk| chunk.revision() != 0) {
        let Some(new_chunk) = new
            .chunks
            .iter()
            .find(|chunk| chunk.position() == old_chunk.position())
        else {
            return Err(StorageError::Conflict(
                "updated snapshot lost a modified chunk",
            ));
        };
        if new_chunk.revision() < old_chunk.revision() {
            return Err(StorageError::Conflict(
                "chunk revision would move backwards",
            ));
        }
        if new_chunk.revision() == old_chunk.revision()
            && old_chunk
                .sections()
                .zip(new_chunk.sections())
                .any(|((_, a), (_, b))| a.encode() != b.encode())
        {
            return Err(StorageError::Conflict(
                "same chunk revision has different block contents",
            ));
        }
    }
    if old.revision == new.revision {
        for chunk in new.chunks.iter().filter(|chunk| chunk.revision() != 0) {
            if !old
                .chunks
                .iter()
                .any(|old| old.position() == chunk.position() && old.revision() == chunk.revision())
            {
                return Err(StorageError::Conflict(
                    "same region revision has different changed chunks",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Directory fsync is not exposed portably on Windows. File contents were
    // synced before rename; power-loss directory durability is unverified there.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockEdit, BlockKind, BlockPos};

    fn edited_region() -> Region {
        let mut region = Region::flat_origin();
        region.snapshot(ChunkPos { x: -2, z: 1 }).unwrap();
        region
            .commit(
                0,
                &[BlockEdit {
                    position: BlockPos {
                        x: -1,
                        y: 20,
                        z: -17,
                    },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        region
            .commit(
                1,
                &[BlockEdit {
                    position: BlockPos {
                        x: 17,
                        y: -64,
                        z: 33,
                    },
                    block: BlockKind::Grass,
                }],
            )
            .unwrap();
        region
    }

    fn reseal(bytes: &mut [u8]) {
        let end = bytes.len() - 4;
        let checksum = crc32fast::hash(&bytes[..end]);
        bytes[end..].copy_from_slice(&checksum.to_le_bytes());
    }

    fn chunk_offsets(bytes: &[u8]) -> Vec<usize> {
        let count = u16::from_le_bytes(bytes[46..48].try_into().unwrap()) as usize;
        let mut position = 48;
        let mut offsets = Vec::new();
        for _ in 0..count {
            offsets.push(position);
            position += 16;
            for _ in 0..SECTION_COUNT {
                let len =
                    u16::from_le_bytes(bytes[position..position + 2].try_into().unwrap()) as usize;
                position += 2 + len;
            }
        }
        offsets
    }

    #[test]
    fn immutable_capture_roundtrips_all_metadata_negative_blocks_and_cow() {
        fn must_be_send_sync<T: Send + Sync>() {}
        must_be_send_sync::<RegionSnapshot>();
        let mut owner = edited_region();
        let capture = owner.capture();
        let bytes = encode_snapshot(&capture).unwrap();
        owner
            .commit(
                2,
                &[BlockEdit {
                    position: BlockPos {
                        x: -1,
                        y: 20,
                        z: -17,
                    },
                    block: BlockKind::Dirt,
                }],
            )
            .unwrap();
        let restored = decode_snapshot(&bytes).unwrap().restore();
        assert_eq!(restored.id(), owner.id());
        assert_eq!(restored.bounds(), owner.bounds());
        assert_eq!(restored.revision(), 2);
        assert_eq!(restored.loaded_chunks(), 3);
        assert_eq!(
            restored
                .get_block(BlockPos {
                    x: -1,
                    y: 20,
                    z: -17
                })
                .unwrap(),
            BlockKind::Stone
        );
        assert_eq!(
            restored
                .get_block(BlockPos {
                    x: 17,
                    y: -64,
                    z: 33
                })
                .unwrap(),
            BlockKind::Grass
        );
        assert_eq!(
            owner
                .get_block(BlockPos {
                    x: -1,
                    y: 20,
                    z: -17
                })
                .unwrap(),
            BlockKind::Dirt
        );
        assert_eq!(encode_snapshot(&restored.capture()).unwrap(), bytes);
        assert_eq!(encode_snapshot(&capture).unwrap(), bytes);
        let empty = Region::flat_origin();
        assert_eq!(
            encode_snapshot(&empty.capture()).unwrap().len(),
            MIN_FILE_BYTES
        );
    }

    #[test]
    fn checksum_truncation_oversize_versions_and_trailing_bytes_are_rejected() {
        let bytes = encode_snapshot(&edited_region().capture()).unwrap();
        for length in 0..bytes.len() {
            assert!(
                decode_snapshot(&bytes[..length]).is_err(),
                "truncation {length}"
            );
        }
        let mut damaged = bytes.clone();
        damaged[70] ^= 1;
        assert!(matches!(
            decode_snapshot(&damaged),
            Err(StorageError::Invalid("checksum mismatch"))
        ));
        assert!(decode_snapshot(&vec![0; MAX_SNAPSHOT_BYTES + 1]).is_err());
        for offset in [0, 8, 10, 12] {
            let mut damaged = bytes.clone();
            damaged[offset] ^= 0x7f;
            reseal(&mut damaged);
            assert!(decode_snapshot(&damaged).is_err(), "header offset {offset}");
        }
        let mut trailing = bytes;
        trailing.insert(trailing.len() - 4, 0);
        reseal(&mut trailing);
        assert!(matches!(
            decode_snapshot(&trailing),
            Err(StorageError::Invalid("trailing bytes"))
        ));
    }

    #[test]
    fn forged_chunk_counts_coordinates_sections_and_revisions_are_rejected() {
        let bytes = encode_snapshot(&edited_region().capture()).unwrap();
        let offsets = chunk_offsets(&bytes);
        let mut invalid = bytes.clone();
        let first_position: [u8; 8] = invalid[offsets[0]..offsets[0] + 8].try_into().unwrap();
        invalid[offsets[1]..offsets[1] + 8].copy_from_slice(&first_position);
        reseal(&mut invalid);
        assert!(decode_snapshot(&invalid).is_err());
        let mut invalid = bytes.clone();
        invalid[46..48].copy_from_slice(&65u16.to_le_bytes());
        reseal(&mut invalid);
        assert!(decode_snapshot(&invalid).is_err());
        let mut invalid = bytes.clone();
        invalid[offsets[0]..offsets[0] + 4].copy_from_slice(&100i32.to_le_bytes());
        reseal(&mut invalid);
        assert!(decode_snapshot(&invalid).is_err());
        for revision in [0, u64::MAX] {
            let mut invalid = bytes.clone();
            invalid[38..46].copy_from_slice(&revision.to_le_bytes());
            reseal(&mut invalid);
            assert!(decode_snapshot(&invalid).is_err());
        }
        for length in [0, MAX_SECTION_BYTES as u16 + 1] {
            let mut invalid = bytes.clone();
            invalid[offsets[0] + 16..offsets[0] + 18].copy_from_slice(&length.to_le_bytes());
            reseal(&mut invalid);
            assert!(decode_snapshot(&invalid).is_err());
        }
        let mut invalid = bytes.clone();
        invalid[offsets[0] + 19] = 255; // unknown ID in an otherwise well-formed uniform section
        reseal(&mut invalid);
        assert!(decode_snapshot(&invalid).is_err());
        let mut invalid = bytes;
        invalid[offsets[1] + 8..offsets[1] + 16].copy_from_slice(&0u64.to_le_bytes());
        reseal(&mut invalid);
        assert!(decode_snapshot(&invalid).is_err());
    }

    #[test]
    fn disk_roundtrip_forward_updates_and_stale_snapshot_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.bin");
        let mut owner = edited_region();
        let old_capture = owner.capture();
        save_snapshot(&path, &old_capture).unwrap();
        assert_eq!(load(&path).unwrap().revision(), 2);
        let old_bytes = fs::read(&path).unwrap();
        save(&path, &owner).unwrap();
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
        owner
            .commit(
                2,
                &[BlockEdit {
                    position: BlockPos {
                        x: -1,
                        y: 20,
                        z: -17,
                    },
                    block: BlockKind::Dirt,
                }],
            )
            .unwrap();
        save(&path, &owner).unwrap();
        let committed = fs::read(&path).unwrap();
        let restored = load(&path).unwrap();
        assert_eq!(restored.revision(), 3);
        assert_eq!(
            restored
                .get_block(BlockPos {
                    x: -1,
                    y: 20,
                    z: -17
                })
                .unwrap(),
            BlockKind::Dirt
        );
        assert!(matches!(
            save_snapshot(&path, &old_capture),
            Err(StorageError::Conflict(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), committed);
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            2,
            "only destination and persistent lock remain"
        );
    }

    #[test]
    fn foreign_corrupt_directory_and_read_only_destinations_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let owner = edited_region();
        let path = directory.path().join("unrelated.dat");
        let foreign = b"An unrelated user file that must never be replaced by a world snapshot.";
        fs::write(&path, foreign).unwrap();
        assert!(save(&path, &owner).is_err());
        assert_eq!(fs::read(&path).unwrap(), foreign);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let mut corrupt = encode_snapshot(&owner.capture()).unwrap();
        corrupt[75] ^= 4;
        fs::write(&path, &corrupt).unwrap();
        assert!(save(&path, &owner).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        let folder = directory.path().join("folder");
        fs::create_dir(&folder).unwrap();
        assert!(save(&folder, &owner).is_err());
        assert!(folder.is_dir());
        fs::write(&path, encode_snapshot(&owner.capture()).unwrap()).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        assert!(save(&path, &owner).is_err());
        assert_eq!(
            load(&path).unwrap().revision(),
            2,
            "read-only snapshots remain loadable"
        );
    }

    #[test]
    fn foreign_lock_or_region_conflict_never_changes_a_valid_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.bin");
        let owner = edited_region();
        save(&path, &owner).unwrap();
        let original = fs::read(&path).unwrap();
        fs::write(
            directory.path().join(".sculith-snapshot.lock"),
            b"user lock contents",
        )
        .unwrap();
        assert!(save(&path, &owner).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::write(directory.path().join(".sculith-snapshot.lock"), LOCK_MAGIC).unwrap();
        let other = Region::new(RegionId(123), owner.bounds());
        assert!(save(&path, &other).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut other = Region::flat_origin();
        other
            .commit(
                0,
                &[BlockEdit {
                    position: BlockPos { x: 1, y: 20, z: 1 },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        other
            .commit(
                1,
                &[BlockEdit {
                    position: BlockPos { x: 2, y: 20, z: 1 },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        assert!(save(&path, &other).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_including_dangling_links_are_refused_without_touching_targets() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.bin");
        let link = directory.path().join("link.bin");
        let owner = edited_region();
        save(&target, &owner).unwrap();
        let original = fs::read(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(save(&link, &owner).is_err());
        assert!(load(&link).is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), original);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(directory.path().join("missing.bin"), &link).unwrap();
        assert!(save(&link, &owner).is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn concurrent_captures_cannot_regress_the_persisted_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("region.bin");
        let mut owner = Region::flat_origin();
        owner
            .commit(
                0,
                &[BlockEdit {
                    position: BlockPos { x: 0, y: 4, z: 0 },
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        let older = owner.capture();
        owner
            .commit(
                1,
                &[BlockEdit {
                    position: BlockPos { x: 1, y: 4, z: 0 },
                    block: BlockKind::Dirt,
                }],
            )
            .unwrap();
        let newer = owner.capture();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for capture in [older, newer] {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                save_snapshot(path, &capture)
            }));
        }
        barrier.wait();
        for handle in handles {
            match handle.join().unwrap() {
                Ok(()) | Err(StorageError::Conflict(_)) => {}
                Err(error) => panic!("unexpected save error: {error}"),
            }
        }
        let restored = load(&path).unwrap();
        assert_eq!(restored.revision(), 2);
        assert_eq!(
            restored.get_block(BlockPos { x: 1, y: 4, z: 0 }).unwrap(),
            BlockKind::Dirt
        );
    }
}
