//! Protocol-independent flat terrain and an exclusively owned region.
//!
//! A [`Region`] is `Send` but not `Sync`; move it to its worker and submit edits to
//! that owner. Readers share immutable [`ChunkSnapshot`]s. This is a tested
//! single-region foundation, not a parallel region scheduler.

pub mod storage;

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    marker::PhantomData,
    sync::Arc,
};

pub const SECTION_EDGE: usize = 16;
pub const SECTION_BLOCKS: usize = SECTION_EDGE * SECTION_EDGE * SECTION_EDGE;
pub const MIN_Y: i32 = -64;
pub const MAX_Y: i32 = 319;
pub const MIN_SECTION_Y: i32 = -4;
pub const MAX_SECTION_Y: i32 = 19;
pub const SECTION_COUNT: usize = 24;
pub const WORLD_BORDER: i32 = 30_000_000;
pub const MAX_REGION_CHUNKS: usize = 64;
pub const MAX_EDITS_PER_BATCH: usize = SECTION_BLOCKS;
const BLOCK_KINDS: usize = 5;

/// Dense internal IDs. Neither Java state IDs nor Bedrock runtime IDs belong here.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BlockKind {
    Air = 0,
    Bedrock = 1,
    Stone = 2,
    Dirt = 3,
    Grass = 4,
}

impl TryFrom<u8> for BlockKind {
    type Error = WorldError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Air),
            1 => Ok(Self::Bedrock),
            2 => Ok(Self::Stone),
            3 => Ok(Self::Dirt),
            4 => Ok(Self::Grass),
            _ => Err(WorldError::InvalidSectionEncoding(
                "unknown internal block ID",
            )),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockPos {
    pub fn chunk(self) -> ChunkPos {
        ChunkPos {
            x: self.x.div_euclid(16),
            z: self.z.div_euclid(16),
        }
    }

    pub fn section_y(self) -> i32 {
        self.y.div_euclid(16)
    }

    pub fn local(self) -> (u8, u8, u8) {
        (
            self.x.rem_euclid(16) as u8,
            self.y.rem_euclid(16) as u8,
            self.z.rem_euclid(16) as u8,
        )
    }

    fn validate(self) -> Result<()> {
        if !(-WORLD_BORDER..WORLD_BORDER).contains(&self.x)
            || !(-WORLD_BORDER..WORLD_BORDER).contains(&self.z)
            || !(MIN_Y..=MAX_Y).contains(&self.y)
        {
            return Err(WorldError::InvalidBlockPosition(self));
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl ChunkPos {
    fn validate(self) -> Result<()> {
        let limit = WORLD_BORDER / 16;
        if !(-limit..limit).contains(&self.x) || !(-limit..limit).contains(&self.z) {
            return Err(WorldError::InvalidChunkPosition(self));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldError {
    InvalidBlockPosition(BlockPos),
    InvalidChunkPosition(ChunkPos),
    InvalidSectionY(i32),
    InvalidLocalPosition { x: u8, y: u8, z: u8 },
    InvalidSectionEncoding(&'static str),
    InvalidRegionBounds,
    OutsideRegion { region: RegionId, chunk: ChunkPos },
    RevisionConflict { expected: u64, actual: u64 },
    RevisionOverflow,
    TooManyEdits(usize),
    DuplicateEdit(BlockPos),
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBlockPosition(pos) => {
                write!(f, "block position outside world bounds: {pos:?}")
            }
            Self::InvalidChunkPosition(pos) => {
                write!(f, "chunk position outside world bounds: {pos:?}")
            }
            Self::InvalidSectionY(y) => {
                write!(f, "section Y {y} outside {MIN_SECTION_Y}..={MAX_SECTION_Y}")
            }
            Self::InvalidLocalPosition { x, y, z } => {
                write!(f, "section-local position ({x}, {y}, {z}) must be 0..=15")
            }
            Self::InvalidSectionEncoding(reason) => write!(f, "invalid section encoding: {reason}"),
            Self::InvalidRegionBounds => write!(
                f,
                "region bounds must contain 1..={MAX_REGION_CHUNKS} valid chunks"
            ),
            Self::OutsideRegion { region, chunk } => {
                write!(f, "region {region:?} does not own chunk {chunk:?}")
            }
            Self::RevisionConflict { expected, actual } => write!(
                f,
                "region revision conflict: expected {expected}, current {actual}"
            ),
            Self::RevisionOverflow => write!(f, "world revision would overflow"),
            Self::TooManyEdits(count) => write!(
                f,
                "edit batch has {count} entries; limit is {MAX_EDITS_PER_BATCH}"
            ),
            Self::DuplicateEdit(pos) => write!(f, "edit batch contains position twice: {pos:?}"),
        }
    }
}

impl Error for WorldError {}

pub type Result<T> = std::result::Result<T, WorldError>;

#[derive(Debug, Clone)]
enum Storage {
    Uniform(BlockKind),
    Palette {
        kinds: Vec<BlockKind>,
        counts: [u16; BLOCK_KINDS],
        bits: u8,
        words: Box<[u64]>,
    },
}

/// An immutable view of one 16³ section. The owner alone has its mutation API.
/// Linear order is X-fast, then Z, then Y: `x + 16*z + 256*y`.
#[derive(Debug, Clone)]
pub struct Section {
    storage: Storage,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SectionStorageStats {
    pub uniform: bool,
    pub palette_entries: usize,
    pub bits_per_block: u8,
    pub packed_bytes: usize,
}

impl Section {
    pub fn uniform(kind: BlockKind) -> Self {
        Self {
            storage: Storage::Uniform(kind),
        }
    }

    pub fn from_blocks(blocks: &[BlockKind]) -> Result<Self> {
        if blocks.len() != SECTION_BLOCKS {
            return Err(WorldError::InvalidSectionEncoding(
                "section must contain exactly 4096 blocks",
            ));
        }
        let mut kinds = Vec::new();
        let mut counts = [0; BLOCK_KINDS];
        for &kind in blocks {
            counts[kind as usize] += 1;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        if kinds.len() == 1 {
            return Ok(Self::uniform(kinds[0]));
        }
        let bits = palette_bits(kinds.len());
        let mut words = vec![0; word_count(bits)].into_boxed_slice();
        for (index, kind) in blocks.iter().enumerate() {
            let palette_index = kinds
                .iter()
                .position(|value| value == kind)
                .expect("palette contains every input kind");
            put_index(&mut words, bits, index, palette_index as u8);
        }
        Ok(Self {
            storage: Storage::Palette {
                kinds,
                counts,
                bits,
                words,
            },
        })
    }

    pub fn get(&self, x: u8, y: u8, z: u8) -> Result<BlockKind> {
        Ok(self.at_index(local_index(x, y, z)?))
    }

    /// Iterates in the explicitly documented internal X/Z/Y order.
    pub fn blocks(&self) -> impl ExactSizeIterator<Item = BlockKind> + '_ {
        (0..SECTION_BLOCKS).map(|index| self.at_index(index))
    }

    pub fn stats(&self) -> SectionStorageStats {
        match &self.storage {
            Storage::Uniform(_) => SectionStorageStats {
                uniform: true,
                palette_entries: 1,
                bits_per_block: 0,
                packed_bytes: 0,
            },
            Storage::Palette {
                kinds, bits, words, ..
            } => SectionStorageStats {
                uniform: false,
                palette_entries: kinds.len(),
                bits_per_block: *bits,
                packed_bytes: words.len() * size_of::<u64>(),
            },
        }
    }

    /// Small version-1 internal storage record; this is not a Minecraft wire format.
    /// Input/output is bounded to at most 1544 bytes for the current registry.
    pub fn encode(&self) -> Vec<u8> {
        match &self.storage {
            Storage::Uniform(kind) => vec![0, *kind as u8],
            Storage::Palette {
                kinds, bits, words, ..
            } => {
                let mut bytes = Vec::with_capacity(3 + kinds.len() + words.len() * 8);
                bytes.extend_from_slice(&[1, *bits, kinds.len() as u8]);
                bytes.extend(kinds.iter().map(|kind| *kind as u8));
                for word in words {
                    bytes.extend_from_slice(&word.to_le_bytes());
                }
                bytes
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let invalid = WorldError::InvalidSectionEncoding;
        match bytes.first() {
            Some(0) if bytes.len() == 2 => Ok(Self::uniform(BlockKind::try_from(bytes[1])?)),
            Some(1) if bytes.len() >= 3 => {
                let bits = bytes[1];
                let count = bytes[2] as usize;
                if !(2..=BLOCK_KINDS).contains(&count) || bits != palette_bits(count) {
                    return Err(invalid("invalid palette size or bit width"));
                }
                let word_bytes = word_count(bits) * 8;
                if bytes.len() != 3 + count + word_bytes {
                    return Err(invalid("incorrect packed byte length"));
                }
                let mut kinds = Vec::with_capacity(count);
                for &id in &bytes[3..3 + count] {
                    let kind = BlockKind::try_from(id)?;
                    if kinds.contains(&kind) {
                        return Err(invalid("duplicate palette kind"));
                    }
                    kinds.push(kind);
                }
                let words: Box<[_]> = bytes[3 + count..]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|word| u64::from_le_bytes(*word))
                    .collect();
                let mut counts = [0; BLOCK_KINDS];
                for index in 0..SECTION_BLOCKS {
                    let palette_index = index_at(&words, bits, index) as usize;
                    let kind = kinds
                        .get(palette_index)
                        .ok_or(invalid("packed index outside palette"))?;
                    counts[*kind as usize] += 1;
                }
                Ok(Self {
                    storage: Storage::Palette {
                        kinds,
                        counts,
                        bits,
                        words,
                    },
                })
            }
            _ => Err(invalid("unknown storage tag or record length")),
        }
    }

    fn at_index(&self, index: usize) -> BlockKind {
        match &self.storage {
            Storage::Uniform(kind) => *kind,
            Storage::Palette {
                kinds, bits, words, ..
            } => kinds[index_at(words, *bits, index) as usize],
        }
    }

    fn set_index(&mut self, index: usize, kind: BlockKind) {
        let old_kind = self.at_index(index);
        if old_kind == kind {
            return;
        }
        match &mut self.storage {
            Storage::Uniform(uniform) => {
                let kinds = vec![*uniform, kind];
                let mut counts = [0; BLOCK_KINDS];
                counts[*uniform as usize] = SECTION_BLOCKS as u16 - 1;
                counts[kind as usize] = 1;
                let mut words = vec![0; word_count(1)].into_boxed_slice();
                put_index(&mut words, 1, index, 1);
                self.storage = Storage::Palette {
                    kinds,
                    counts,
                    bits: 1,
                    words,
                };
            }
            Storage::Palette {
                kinds,
                counts,
                bits,
                words,
            } => {
                let palette_index =
                    if let Some(index) = kinds.iter().position(|&value| value == kind) {
                        index
                    } else {
                        kinds.push(kind);
                        let width = palette_bits(kinds.len());
                        if width != *bits {
                            let mut replacement = vec![0; word_count(width)].into_boxed_slice();
                            for block in 0..SECTION_BLOCKS {
                                put_index(
                                    &mut replacement,
                                    width,
                                    block,
                                    index_at(words, *bits, block),
                                );
                            }
                            *bits = width;
                            *words = replacement;
                        }
                        kinds.len() - 1
                    };
                put_index(words, *bits, index, palette_index as u8);
                counts[old_kind as usize] -= 1;
                counts[kind as usize] += 1;
                if counts[kind as usize] == SECTION_BLOCKS as u16 {
                    self.storage = Storage::Uniform(kind);
                }
            }
        }
    }
}

fn palette_bits(count: usize) -> u8 {
    (usize::BITS - (count - 1).leading_zeros()) as u8
}
fn word_count(bits: u8) -> usize {
    (SECTION_BLOCKS * bits as usize).div_ceil(64)
}

fn index_at(words: &[u64], bits: u8, index: usize) -> u8 {
    let offset = index * bits as usize;
    let word = offset / 64;
    let shift = offset % 64;
    let mut value = words[word] >> shift;
    if shift + bits as usize > 64 {
        value |= words[word + 1] << (64 - shift);
    }
    (value & ((1 << bits) - 1)) as u8
}

fn put_index(words: &mut [u64], bits: u8, index: usize, value: u8) {
    let offset = index * bits as usize;
    let word = offset / 64;
    let shift = offset % 64;
    let mask = (1u64 << bits) - 1;
    words[word] = (words[word] & !(mask << shift)) | (u64::from(value) << shift);
    if shift + bits as usize > 64 {
        let upper_bits = shift + bits as usize - 64;
        let upper_mask = (1u64 << upper_bits) - 1;
        words[word + 1] = (words[word + 1] & !upper_mask) | (u64::from(value) >> (64 - shift));
    }
}

fn local_index(x: u8, y: u8, z: u8) -> Result<usize> {
    if x >= 16 || y >= 16 || z >= 16 {
        return Err(WorldError::InvalidLocalPosition { x, y, z });
    }
    Ok(x as usize + 16 * z as usize + 256 * y as usize)
}

fn section_index(y: i32) -> Result<usize> {
    if !(MIN_SECTION_Y..=MAX_SECTION_Y).contains(&y) {
        return Err(WorldError::InvalidSectionY(y));
    }
    Ok((y - MIN_SECTION_Y) as usize)
}

#[derive(Debug, Clone)]
struct ChunkData {
    position: ChunkPos,
    revision: u64,
    sections: [Arc<Section>; SECTION_COUNT],
}

/// A cheap Arc clone that remains unchanged when its owner commits later edits.
#[derive(Debug, Clone)]
pub struct ChunkSnapshot {
    data: Arc<ChunkData>,
}

impl ChunkSnapshot {
    pub fn position(&self) -> ChunkPos {
        self.data.position
    }
    pub fn revision(&self) -> u64 {
        self.data.revision
    }

    pub fn section(&self, section_y: i32) -> Result<&Section> {
        Ok(&self.data.sections[section_index(section_y)?])
    }

    pub fn sections(&self) -> impl ExactSizeIterator<Item = (i32, &Section)> {
        self.data
            .sections
            .iter()
            .enumerate()
            .map(|(index, section)| (MIN_SECTION_Y + index as i32, section.as_ref()))
    }

    pub fn get(&self, local_x: u8, world_y: i32, local_z: u8) -> Result<BlockKind> {
        if !(MIN_Y..=MAX_Y).contains(&world_y) {
            return Err(WorldError::InvalidSectionY(world_y.div_euclid(16)));
        }
        self.section(world_y.div_euclid(16))?
            .get(local_x, world_y.rem_euclid(16) as u8, local_z)
    }
}

/// Deterministic flat terrain: bedrock at 0, dirt at 1..=2, grass at 3.
/// The other layers of the -64..=319 overworld are air.
#[derive(Debug, Clone)]
pub struct FlatGenerator {
    air: Arc<Section>,
    ground: Arc<Section>,
}

impl Default for FlatGenerator {
    fn default() -> Self {
        let blocks: Vec<_> = (0..SECTION_BLOCKS)
            .map(|index| flat_block((index / 256) as i32))
            .collect();
        Self {
            air: Arc::new(Section::uniform(BlockKind::Air)),
            ground: Arc::new(
                Section::from_blocks(&blocks).expect("fixed flat terrain has 4096 blocks"),
            ),
        }
    }
}

impl FlatGenerator {
    /// World-domain feet position on top of the grass layer, not a wire eye position.
    pub fn spawn(&self) -> (f32, f32, f32) {
        (0.5, 4.0, 0.5)
    }

    pub fn snapshot(&self, position: ChunkPos) -> Result<ChunkSnapshot> {
        position.validate()?;
        let sections = std::array::from_fn(|index| {
            if MIN_SECTION_Y + index as i32 == 0 {
                self.ground.clone()
            } else {
                self.air.clone()
            }
        });
        Ok(ChunkSnapshot {
            data: Arc::new(ChunkData {
                position,
                revision: 0,
                sections,
            }),
        })
    }
}

fn flat_block(y: i32) -> BlockKind {
    match y {
        0 => BlockKind::Bedrock,
        1 | 2 => BlockKind::Dirt,
        3 => BlockKind::Grass,
        _ => BlockKind::Air,
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(pub u64);

/// Explicit half-open chunk rectangle, bounded to 64 chunks. A future region
/// manager assigns non-overlapping rectangles; this type does not claim to be one.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RegionBounds {
    min: ChunkPos,
    max_exclusive: ChunkPos,
}

impl RegionBounds {
    pub fn new(min: ChunkPos, max_exclusive: ChunkPos) -> Result<Self> {
        min.validate()
            .map_err(|_| WorldError::InvalidRegionBounds)?;
        let last = ChunkPos {
            x: max_exclusive
                .x
                .checked_sub(1)
                .ok_or(WorldError::InvalidRegionBounds)?,
            z: max_exclusive
                .z
                .checked_sub(1)
                .ok_or(WorldError::InvalidRegionBounds)?,
        };
        last.validate()
            .map_err(|_| WorldError::InvalidRegionBounds)?;
        let width = i64::from(max_exclusive.x) - i64::from(min.x);
        let depth = i64::from(max_exclusive.z) - i64::from(min.z);
        if width <= 0
            || depth <= 0
            || width > MAX_REGION_CHUNKS as i64
            || depth > MAX_REGION_CHUNKS as i64
            || width * depth > MAX_REGION_CHUNKS as i64
        {
            return Err(WorldError::InvalidRegionBounds);
        }
        Ok(Self { min, max_exclusive })
    }

    pub fn min(self) -> ChunkPos {
        self.min
    }
    pub fn max_exclusive(self) -> ChunkPos {
        self.max_exclusive
    }
    pub fn contains(self, pos: ChunkPos) -> bool {
        (self.min.x..self.max_exclusive.x).contains(&pos.x)
            && (self.min.z..self.max_exclusive.z).contains(&pos.z)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BlockEdit {
    pub position: BlockPos,
    pub block: BlockKind,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BlockChange {
    pub position: BlockPos,
    pub before: BlockKind,
    pub after: BlockKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedBatch {
    pub region: RegionId,
    pub previous_revision: u64,
    pub revision: u64,
    /// One event per effective change, preserving input order. Duplicate inputs are refused.
    pub changes: Vec<BlockChange>,
}

/// An immutable, `Send + Sync` capture suitable for an I/O worker. Capturing does
/// not encode or access files, and later owner commits cannot change this state.
#[derive(Debug, Clone)]
pub struct RegionSnapshot {
    id: RegionId,
    bounds: RegionBounds,
    revision: u64,
    chunks: Vec<ChunkSnapshot>,
}

impl RegionSnapshot {
    pub fn id(&self) -> RegionId {
        self.id
    }
    pub fn bounds(&self) -> RegionBounds {
        self.bounds
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn chunks(&self) -> &[ChunkSnapshot] {
        &self.chunks
    }

    fn restore(self) -> Region {
        Region {
            id: self.id,
            bounds: self.bounds,
            revision: self.revision,
            generator: FlatGenerator::default(),
            chunks: self
                .chunks
                .into_iter()
                .map(|chunk| (chunk.position(), chunk))
                .collect(),
            _exclusive: PhantomData,
        }
    }
}

/// Exclusive mutable owner. It can move to a worker (`Send`) but cannot be
/// concurrently shared (`!Sync`); use immutable snapshots across worker boundaries.
///
/// ```compile_fail
/// use sculith_world::Region;
/// fn must_be_sync<T: Sync>() {}
/// must_be_sync::<Region>();
/// ```
#[derive(Debug)]
pub struct Region {
    id: RegionId,
    bounds: RegionBounds,
    generator: FlatGenerator,
    chunks: BTreeMap<ChunkPos, ChunkSnapshot>,
    revision: u64,
    _exclusive: PhantomData<Cell<()>>,
}

impl Region {
    pub fn new(id: RegionId, bounds: RegionBounds) -> Self {
        Self {
            id,
            bounds,
            generator: FlatGenerator::default(),
            chunks: BTreeMap::new(),
            revision: 0,
            _exclusive: PhantomData,
        }
    }

    /// A single owner around the initial spawn, chunk coordinates -4..=3.
    pub fn flat_origin() -> Self {
        Self::new(
            RegionId(0),
            RegionBounds::new(ChunkPos { x: -4, z: -4 }, ChunkPos { x: 4, z: 4 })
                .expect("fixed origin region is valid"),
        )
    }

    pub fn id(&self) -> RegionId {
        self.id
    }
    pub fn bounds(&self) -> RegionBounds {
        self.bounds
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn loaded_chunks(&self) -> usize {
        self.chunks.len()
    }
    pub fn spawn(&self) -> (f32, f32, f32) {
        self.generator.spawn()
    }

    pub fn capture(&self) -> RegionSnapshot {
        RegionSnapshot {
            id: self.id,
            bounds: self.bounds,
            revision: self.revision,
            chunks: self.chunks.values().cloned().collect(),
        }
    }

    fn require_owned(&self, pos: ChunkPos) -> Result<()> {
        pos.validate()?;
        if !self.bounds.contains(pos) {
            return Err(WorldError::OutsideRegion {
                region: self.id,
                chunk: pos,
            });
        }
        Ok(())
    }

    pub fn snapshot(&mut self, position: ChunkPos) -> Result<ChunkSnapshot> {
        self.require_owned(position)?;
        if let std::collections::btree_map::Entry::Vacant(entry) = self.chunks.entry(position) {
            entry.insert(self.generator.snapshot(position)?);
        }
        Ok(self
            .chunks
            .get(&position)
            .expect("chunk was loaded")
            .clone())
    }

    /// Unloaded flat terrain is read without allocating a chunk or changing revisions.
    pub fn get_block(&self, position: BlockPos) -> Result<BlockKind> {
        position.validate()?;
        self.require_owned(position.chunk())?;
        match self.chunks.get(&position.chunk()) {
            Some(chunk) => chunk.get(position.local().0, position.y, position.local().2),
            None => Ok(flat_block(position.y)),
        }
    }

    /// Atomically validates and commits at most 4096 unique block edits.
    /// Failed or all-no-op batches do not mutate blocks, allocate chunks, or advance revisions.
    pub fn commit(
        &mut self,
        expected_revision: u64,
        edits: &[BlockEdit],
    ) -> Result<CommittedBatch> {
        if self.revision != expected_revision {
            return Err(WorldError::RevisionConflict {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        if edits.len() > MAX_EDITS_PER_BATCH {
            return Err(WorldError::TooManyEdits(edits.len()));
        }
        let mut seen = BTreeSet::new();
        let mut changed_chunks = BTreeSet::new();
        let mut changes = Vec::new();
        for edit in edits {
            if !seen.insert(edit.position) {
                return Err(WorldError::DuplicateEdit(edit.position));
            }
            let before = self.get_block(edit.position)?;
            if before != edit.block {
                changed_chunks.insert(edit.position.chunk());
                changes.push(BlockChange {
                    position: edit.position,
                    before,
                    after: edit.block,
                });
            }
        }
        if changes.is_empty() {
            return Ok(CommittedBatch {
                region: self.id,
                previous_revision: self.revision,
                revision: self.revision,
                changes,
            });
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(WorldError::RevisionOverflow)?;
        let mut new_chunks = Vec::new();
        for position in &changed_chunks {
            match self.chunks.get(position) {
                Some(chunk) => {
                    chunk
                        .revision()
                        .checked_add(1)
                        .ok_or(WorldError::RevisionOverflow)?;
                }
                None => new_chunks.push((*position, self.generator.snapshot(*position)?)),
            }
        }
        for (position, chunk) in new_chunks {
            self.chunks.insert(position, chunk);
        }
        for position in changed_chunks {
            let chunk = self
                .chunks
                .get_mut(&position)
                .expect("all changed chunks loaded during preflight");
            Arc::make_mut(&mut chunk.data).revision += 1;
        }
        for change in &changes {
            let chunk = self
                .chunks
                .get_mut(&change.position.chunk())
                .expect("all changed chunks loaded during preflight");
            let data = Arc::make_mut(&mut chunk.data);
            let (x, y, z) = change.position.local();
            let section =
                &mut data.sections[(change.position.section_y() - MIN_SECTION_Y) as usize];
            Arc::make_mut(section).set_index(
                x as usize + 16 * z as usize + 256 * y as usize,
                change.after,
            );
        }
        let previous_revision = self.revision;
        self.revision = revision;
        Ok(CommittedBatch {
            region: self.id,
            previous_revision,
            revision,
            changes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [BlockKind; BLOCK_KINDS] = [
        BlockKind::Air,
        BlockKind::Bedrock,
        BlockKind::Stone,
        BlockKind::Dirt,
        BlockKind::Grass,
    ];

    #[test]
    fn flat_snapshots_cover_all_overworld_sections_and_negative_chunks() {
        let generator = FlatGenerator::default();
        assert_eq!(generator.spawn(), (0.5, 4.0, 0.5));
        for position in [ChunkPos { x: 0, z: 0 }, ChunkPos { x: -1, z: -17 }] {
            let snapshot = generator.snapshot(position).unwrap();
            assert_eq!(snapshot.position(), position);
            assert_eq!(snapshot.sections().len(), SECTION_COUNT);
            assert_eq!(snapshot.sections().next().unwrap().0, -4);
            assert_eq!(snapshot.sections().last().unwrap().0, 19);
            for y in MIN_Y..=MAX_Y {
                for (x, z) in [(0, 0), (15, 15), (7, 11)] {
                    assert_eq!(snapshot.get(x, y, z).unwrap(), flat_block(y));
                }
            }
            assert!(snapshot.get(16, 0, 0).is_err());
            assert!(snapshot.get(0, MIN_Y - 1, 0).is_err());
            assert!(snapshot.get(0, MAX_Y + 1, 0).is_err());
        }
        assert!(generator.snapshot(ChunkPos { x: i32::MAX, z: 0 }).is_err());
    }

    #[test]
    fn signed_coordinates_use_euclidean_chunk_section_and_local_coordinates() {
        for (input, chunk, section, local) in [
            ((-1, -64, -17), (-1, -2), -4, (15, 0, 15)),
            ((-16, -1, 16), (-1, 1), -1, (0, 15, 0)),
            ((-17, 319, -16), (-2, -1), 19, (15, 15, 0)),
        ] {
            let pos = BlockPos {
                x: input.0,
                y: input.1,
                z: input.2,
            };
            assert_eq!(
                pos.chunk(),
                ChunkPos {
                    x: chunk.0,
                    z: chunk.1
                }
            );
            assert_eq!(pos.section_y(), section);
            assert_eq!(pos.local(), local);
        }
    }

    #[test]
    fn palette_growth_and_three_bit_word_crossings_preserve_every_block() {
        let mut section = Section::uniform(BlockKind::Air);
        let mut reference = vec![BlockKind::Air; SECTION_BLOCKS];
        for (index, slot) in reference.iter_mut().enumerate() {
            let kind = KINDS[index % KINDS.len()];
            *slot = kind;
            section.set_index(index, kind);
        }
        assert_eq!(section.stats().bits_per_block, 3);
        assert_eq!(section.stats().packed_bytes, 1536);
        assert_eq!(section.blocks().collect::<Vec<_>>(), reference);
        let roundtrip = Section::decode(&section.encode()).unwrap();
        assert_eq!(roundtrip.blocks().collect::<Vec<_>>(), reference);
        for index in 0..SECTION_BLOCKS {
            section.set_index(index, BlockKind::Dirt);
        }
        assert!(section.stats().uniform);
        assert_eq!(section.encode(), [0, BlockKind::Dirt as u8]);
    }

    #[test]
    fn section_storage_roundtrip_rejects_truncation_forged_indices_and_palette_ids() {
        let blocks: Vec<_> = (0..SECTION_BLOCKS).map(|i| KINDS[i % 5]).collect();
        let section = Section::from_blocks(&blocks).unwrap();
        let encoded = section.encode();
        assert_eq!(encoded.len(), 1544);
        for length in 0..encoded.len() {
            assert!(Section::decode(&encoded[..length]).is_err());
        }
        let mut invalid = encoded.clone();
        invalid.push(0);
        assert!(Section::decode(&invalid).is_err());
        let mut invalid = encoded.clone();
        invalid[3] = 255;
        assert!(Section::decode(&invalid).is_err());
        let mut invalid = encoded.clone();
        invalid[4] = invalid[3];
        assert!(Section::decode(&invalid).is_err());
        let mut invalid = encoded;
        invalid[8] = 7; // first 3-bit palette index is outside a five-entry palette
        assert!(Section::decode(&invalid).is_err());
        assert!(Section::from_blocks(&blocks[..SECTION_BLOCKS - 1]).is_err());
    }

    #[test]
    fn snapshots_share_unchanged_sections_and_cow_only_edited_section() {
        let mut region = Region::flat_origin();
        let position = ChunkPos { x: -1, z: -1 };
        let old = region.snapshot(position).unwrap();
        let old_clone = old.clone();
        assert!(Arc::ptr_eq(&old.data, &old_clone.data));
        let block = BlockPos {
            x: -1,
            y: 20,
            z: -1,
        };
        let batch = region
            .commit(
                0,
                &[BlockEdit {
                    position: block,
                    block: BlockKind::Stone,
                }],
            )
            .unwrap();
        let new = region.snapshot(position).unwrap();
        assert_eq!(
            batch.changes,
            [BlockChange {
                position: block,
                before: BlockKind::Air,
                after: BlockKind::Stone
            }]
        );
        assert_eq!(
            (old.revision(), new.revision(), region.revision()),
            (0, 1, 1)
        );
        assert_eq!(old.get(15, 20, 15).unwrap(), BlockKind::Air);
        assert_eq!(new.get(15, 20, 15).unwrap(), BlockKind::Stone);
        assert!(!std::ptr::eq(
            old.section(1).unwrap(),
            new.section(1).unwrap()
        ));
        assert!(std::ptr::eq(
            old.section(0).unwrap(),
            new.section(0).unwrap()
        ));
        assert!(std::ptr::eq(
            old.section(-4).unwrap(),
            new.section(-4).unwrap()
        ));
    }

    #[test]
    fn commit_is_atomic_and_checks_ownership_revision_duplicates_and_batch_size() {
        let mut region = Region::flat_origin();
        let valid = BlockEdit {
            position: BlockPos { x: 0, y: 4, z: 0 },
            block: BlockKind::Stone,
        };
        let outside = BlockEdit {
            position: BlockPos { x: 64, y: 4, z: 0 },
            block: BlockKind::Stone,
        };
        assert!(matches!(
            region.commit(0, &[valid, outside]),
            Err(WorldError::OutsideRegion { .. })
        ));
        assert!(matches!(
            region.commit(0, &[valid, valid]),
            Err(WorldError::DuplicateEdit(_))
        ));
        assert!(matches!(
            region.commit(1, &[valid]),
            Err(WorldError::RevisionConflict { .. })
        ));
        assert!(matches!(
            region.commit(0, &vec![valid; MAX_EDITS_PER_BATCH + 1]),
            Err(WorldError::TooManyEdits(_))
        ));
        assert_eq!(region.revision(), 0);
        assert_eq!(region.loaded_chunks(), 0);
        assert_eq!(region.get_block(valid.position).unwrap(), BlockKind::Air);
        let batch = region.commit(0, &[valid]).unwrap();
        assert_eq!((batch.previous_revision, batch.revision), (0, 1));
        assert_eq!(region.loaded_chunks(), 1);
        assert_eq!(region.get_block(valid.position).unwrap(), BlockKind::Stone);
        assert!(region.snapshot(ChunkPos { x: -5, z: 0 }).is_err());
    }

    #[test]
    fn committed_changes_keep_order_and_noops_do_not_allocate_or_advance_revision() {
        let mut region = Region::flat_origin();
        let noop = BlockEdit {
            position: BlockPos { x: -1, y: 0, z: 0 },
            block: BlockKind::Bedrock,
        };
        let empty = region.commit(0, &[noop]).unwrap();
        assert!(empty.changes.is_empty());
        assert_eq!((region.revision(), region.loaded_chunks()), (0, 0));
        let edits = [
            BlockEdit {
                position: BlockPos {
                    x: 17,
                    y: -64,
                    z: 0,
                },
                block: BlockKind::Stone,
            },
            noop,
            BlockEdit {
                position: BlockPos {
                    x: -1,
                    y: 319,
                    z: -1,
                },
                block: BlockKind::Grass,
            },
            BlockEdit {
                position: BlockPos {
                    x: 18,
                    y: -64,
                    z: 0,
                },
                block: BlockKind::Dirt,
            },
        ];
        let batch = region.commit(0, &edits).unwrap();
        assert_eq!(
            batch
                .changes
                .iter()
                .map(|change| change.position)
                .collect::<Vec<_>>(),
            vec![edits[0].position, edits[2].position, edits[3].position]
        );
        assert_eq!(region.loaded_chunks(), 2);
        for pos in [edits[0].position.chunk(), edits[2].position.chunk()] {
            assert_eq!(region.snapshot(pos).unwrap().revision(), 1);
        }
    }

    #[test]
    fn bounds_and_revisions_cannot_overflow_and_fail_before_mutation() {
        for (min, max) in [
            (ChunkPos { x: 0, z: 0 }, ChunkPos { x: 0, z: 1 }),
            (ChunkPos { x: 0, z: 0 }, ChunkPos { x: 9, z: 8 }),
            (
                ChunkPos { x: i32::MIN, z: 0 },
                ChunkPos { x: i32::MAX, z: 1 },
            ),
        ] {
            assert!(RegionBounds::new(min, max).is_err());
        }
        let mut region = Region::flat_origin();
        let edit = BlockEdit {
            position: BlockPos { x: 0, y: 4, z: 0 },
            block: BlockKind::Stone,
        };
        region.revision = u64::MAX;
        assert_eq!(
            region.commit(u64::MAX, &[edit]),
            Err(WorldError::RevisionOverflow)
        );
        assert_eq!(region.loaded_chunks(), 0);
        region.revision = 0;
        let position = edit.position.chunk();
        let _ = region.snapshot(position).unwrap();
        Arc::make_mut(&mut region.chunks.get_mut(&position).unwrap().data).revision = u64::MAX;
        assert_eq!(region.commit(0, &[edit]), Err(WorldError::RevisionOverflow));
        assert_eq!(region.get_block(edit.position).unwrap(), BlockKind::Air);
        assert_eq!(region.revision(), 0);
    }

    #[test]
    fn region_moves_to_one_worker_and_immutable_snapshots_cross_threads() {
        fn must_be_send<T: Send>() {}
        fn must_be_sync<T: Sync>() {}
        must_be_send::<Region>();
        must_be_sync::<ChunkSnapshot>();
        let mut owner = Region::flat_origin();
        let before = owner.snapshot(ChunkPos { x: 0, z: 0 }).unwrap();
        let after = std::thread::spawn(move || {
            owner
                .commit(
                    0,
                    &[BlockEdit {
                        position: BlockPos { x: 0, y: 4, z: 0 },
                        block: BlockKind::Stone,
                    }],
                )
                .unwrap();
            owner.snapshot(ChunkPos { x: 0, z: 0 }).unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(before.get(0, 4, 0).unwrap(), BlockKind::Air);
        assert_eq!(after.get(0, 4, 0).unwrap(), BlockKind::Stone);
    }
}
