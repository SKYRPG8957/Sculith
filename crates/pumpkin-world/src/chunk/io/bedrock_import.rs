//! Imports saved Bedrock terrain and supported storage into a new Anvil world.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Cursor, Read},
    num::NonZeroU8,
    path::{Path, PathBuf},
    sync::Arc,
};

use leveldb::{CompactionMode, Compressor, CompressorList, DB, Options, Status, StatusCode};
use pumpkin_config::chunk::AnvilChunkConfig;
use pumpkin_data::{Block, BlockStateId, biome::Biome, dimension::Dimension};
use pumpkin_nbt::{
    Nbt,
    deserializer::{NbtReadHelper, NbtReadHelperBedrockDisk},
};
use pumpkin_util::{
    math::{position::BlockPos, vector2::Vector2},
    world_seed::Seed,
};

use crate::{
    chunk::{
        ChunkData, ChunkHeightmapType, ChunkReadingError, ChunkSections, ChunkWritingError,
        format::anvil::AnvilRegion, palette::BlockPalette,
    },
    world_info::{
        FlatPresetLayer, FlatPresetSettings, GeneratorSettings, LevelData, StructureOverrides,
        WorldInfoError, WorldInfoWriter, anvil::AnvilLevelInfo,
    },
};

use super::{RegionFile, region_chunk_index, region_of};

mod storage;
mod storage_export;
mod terrain;
#[cfg(test)]
mod tests;

pub use storage_export::{StorageExportStats, export_storage};

const OVERWORLD: Dimension = Dimension::OVERWORLD;
const STORAGE_VERSION: u32 = 10;
const CHUNK_VERSION: u8 = 42;
const AUTOMATIC_SPAWN_Y: i32 = i16::MAX as i32;
const CHUNK_COORDINATE_LIMIT: u32 = 30_000_000 / 16;
const MAX_LEVEL_DAT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TABLE_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const DIAGNOSTIC_LIMIT: usize = 16;

const BIOMES: u8 = 0x2b;
const VERSION: u8 = 0x2c;
const SUBCHUNK: u8 = 0x2f;
const BLOCK_ENTITY: u8 = 0x31;
const ENTITY: u8 = 0x32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("LevelDB: {0}")]
    Database(#[from] Status),
    #[error("Bedrock NBT: {0}")]
    Nbt(#[from] pumpkin_nbt::Error),
    #[error("World settings: {0}")]
    Settings(#[from] serde_json::Error),
    #[error("Anvil metadata: {0}")]
    WorldInfo(#[from] WorldInfoError),
    #[error("Anvil region: {0}")]
    Read(#[from] ChunkReadingError),
    #[error("Anvil chunk: {0}")]
    Write(#[from] ChunkWritingError),
    #[error("chunk {x}, {z}: {error}")]
    Chunk {
        x: i32,
        z: i32,
        #[source]
        error: Box<Self>,
    },
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

impl Error {
    fn at_chunk(self, x: i32, z: i32) -> Self {
        Self::Chunk {
            x,
            z,
            error: Box::new(self),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum GeneratorPolicy {
    RequireSource,
    NormalIfUnspecified,
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub regions: usize,
    pub chunks: usize,
    pub containers_imported: usize,
    pub block_entity_records_excluded: usize,
    pub block_entities_skipped: usize,
    pub skipped_block_entity_examples: Vec<SkippedBlockEntity>,
    pub entity_records_excluded: usize,
    pub other_records_excluded: usize,
    pub default_generator_selected: bool,
}

#[derive(Debug)]
pub struct SkippedBlockEntity {
    pub kind: String,
    pub position: Option<BlockPos>,
    pub reason: String,
}

impl ImportStats {
    fn skip_block_entity(&mut self, kind: &str, position: Option<BlockPos>, reason: String) {
        self.block_entities_skipped += 1;
        if self.skipped_block_entity_examples.len() < DIAGNOSTIC_LIMIT {
            self.skipped_block_entity_examples.push(SkippedBlockEntity {
                kind: kind.to_owned(),
                position,
                reason,
            });
        }
    }

    fn add_storage_results(&mut self, parsed: Self) {
        self.containers_imported += parsed.containers_imported;
        self.block_entities_skipped += parsed.block_entities_skipped;
        self.skipped_block_entity_examples.extend(
            parsed
                .skipped_block_entity_examples
                .into_iter()
                .take(DIAGNOSTIC_LIMIT - self.skipped_block_entity_examples.len()),
        );
    }
}

#[derive(Debug, Default)]
pub struct Inspection {
    pub stats: ImportStats,
    pub blockers: usize,
    pub blocker_examples: Vec<Error>,
}

impl Inspection {
    fn record_blocker(&mut self, error: Error) {
        self.blockers += 1;
        if self.blocker_examples.len() < DIAGNOSTIC_LIMIT {
            self.blocker_examples.push(error);
        }
    }
}

/// Checks the same saved data and generator settings used by the importer.
/// Reads through a temporary `LevelDB` snapshot.
pub fn inspect_terrain(source: &Path, policy: GeneratorPolicy) -> Result<Inspection, Error> {
    let source = source.canonicalize()?;
    let temp = std::env::temp_dir().canonicalize()?;
    let scratch = if temp.starts_with(&source) {
        source
            .parent()
            .ok_or_else(|| invalid("no scratch directory outside the source world"))?
    } else {
        &temp
    };
    let staging = tempfile::tempdir_in(scratch)?;
    let registry = terrain::TerrainRegistry::load()?;
    let items = storage::item_registry();
    let mut inspection = Inspection::default();
    let mut stats = ImportStats::default();
    match read_level(&source, &registry, policy) {
        Ok((_, default_selected)) => stats.default_generator_selected = default_selected,
        Err(error) => inspection.record_blocker(error),
    }
    let db = open_database_copy(&source, staging.path())?;
    let regions = index_chunks(&db, &mut stats, |error| {
        inspection.record_blocker(error);
        Ok(())
    })?;
    if regions.is_empty() {
        inspection.record_blocker(invalid("the source contains no saved Overworld terrain"));
    }
    for columns in regions.into_values() {
        let before = stats.chunks;
        for ((x, z), sections) in columns {
            match read_chunk(&db, x, z, sections, &registry, &items, &mut stats) {
                Ok(_) => stats.chunks += 1,
                Err(error) => inspection.record_blocker(error.at_chunk(x, z)),
            }
        }
        if stats.chunks > before {
            stats.regions += 1;
        }
    }
    inspection.stats = stats;
    Ok(inspection)
}

/// Imports saved Overworld terrain and supported storage into a new directory.
/// The completed world is moved into place after conversion succeeds.
pub fn import_terrain(source: &Path, destination: &Path) -> Result<ImportStats, Error> {
    import_terrain_with_generator(source, destination, GeneratorPolicy::RequireSource)
}

pub fn import_terrain_with_generator(
    source: &Path,
    destination: &Path,
    policy: GeneratorPolicy,
) -> Result<ImportStats, Error> {
    let source = source.canonicalize()?;
    let destination = new_destination(destination, &source)?;
    let registry = terrain::TerrainRegistry::load()?;
    let items = storage::item_registry();
    let (mut info, default_generator_selected) = read_level(&source, &registry, policy)?;

    let staging = tempfile::tempdir_in(
        destination
            .parent()
            .ok_or_else(|| invalid("invalid destination"))?,
    )?;
    let db = open_database_copy(&source, staging.path())?;

    let mut stats = ImportStats {
        default_generator_selected,
        ..ImportStats::default()
    };
    let regions = index_chunks(&db, &mut stats, Err)?;
    if regions.is_empty() {
        return Err(invalid("the source contains no saved Overworld terrain"));
    }

    let output = staging.path().join("world");
    let region_folder = output.join("region");
    fs::create_dir_all(&region_folder)?;
    let config = AnvilChunkConfig::default();
    for ((rx, rz), columns) in regions {
        let region_pos = Vector2::new(rx, rz);
        let path = region_folder.join(AnvilRegion::file_name(region_pos));
        let mut region = AnvilRegion::open(&path, region_pos)?;
        for ((x, z), sections) in columns {
            let chunk = read_chunk(&db, x, z, sections, &registry, &items, &mut stats)
                .map_err(|error| error.at_chunk(x, z))?;
            update_spawn_height(&mut info, &chunk);
            let encoded = AnvilRegion::encode_chunk(chunk.internal_to_bytes(), &config)?;
            region.stage_chunk(region_chunk_index(Vector2::new(x, z)), encoded);
            stats.chunks += 1;
        }
        region.flush(true)?;
        stats.regions += 1;
    }
    if info.spawn_y == AUTOMATIC_SPAWN_Y {
        info.spawn_y = LevelData::default(Seed(0)).spawn_y;
    }
    AnvilLevelInfo.write_world_info(&info, &output)?;
    if crate::world_info::data_files::read_world_gen_settings(&output).is_none() {
        return Err(invalid("failed to save imported world generation settings"));
    }
    ensure_absent(&destination)?;
    fs::rename(output, destination)?;
    Ok(stats)
}

fn open_database_copy(source: &Path, staging: &Path) -> Result<DB, Error> {
    // Recovery during a LevelDB open may update files even when only reading.
    let database = staging.join("db");
    copy_database(&source.join("db"), &database)?;
    Ok(DB::open(database, bedrock_db_options())?)
}

fn new_destination(destination: &Path, source: &Path) -> Result<PathBuf, Error> {
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("destination must name a new world directory"))?;
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let destination = parent.join(name);
    if destination.starts_with(source) {
        return Err(invalid("destination must be outside the Bedrock world"));
    }
    ensure_absent(&destination)?;
    Ok(destination)
}

fn ensure_absent(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(invalid(format!(
            "destination already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn copy_database(source: &Path, destination: &Path) -> Result<(), Error> {
    let mut current = String::new();
    File::open(source.join("CURRENT"))?
        .take(128)
        .read_to_string(&mut current)?;
    if !current.strip_suffix('\n').is_some_and(manifest_name) {
        return Err(invalid("invalid LevelDB CURRENT file"));
    }
    fs::create_dir(destination)?;
    fs::write(destination.join("CURRENT"), current)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let relevant = name.to_str().is_some_and(|name| {
            manifest_name(name)
                || name.rsplit_once('.').is_some_and(|(id, extension)| {
                    numeric_name(id) && matches!(extension, "log" | "ldb" | "sst")
                })
        });
        if relevant {
            if !entry.file_type()?.is_file() {
                return Err(invalid("LevelDB snapshot contains a non-regular file"));
            }
            fs::copy(entry.path(), destination.join(name))?;
        }
    }
    Ok(())
}

fn manifest_name(name: &str) -> bool {
    name.strip_prefix("MANIFEST-").is_some_and(numeric_name)
}

fn numeric_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
}

// Bedrock uses zlib (2) and raw deflate (4) table blocks. The same compressor
// selection is used by bedrock-rs. Recovery writes uncompressed temporary files.
struct Deflate {
    zlib: bool,
}

impl Compressor for Deflate {
    fn encode(&self, _: &[u8]) -> leveldb::Result<Vec<u8>> {
        Err(compression_error(
            "Bedrock compression is only enabled for reads",
        ))
    }

    fn decode(&self, block: &[u8]) -> leveldb::Result<Vec<u8>> {
        use miniz_oxide::inflate::{
            decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit,
        };
        let decoded = if self.zlib {
            decompress_to_vec_zlib_with_limit(block, MAX_TABLE_BLOCK_BYTES)
        } else {
            decompress_to_vec_with_limit(block, MAX_TABLE_BLOCK_BYTES)
        };
        decoded.map_err(|err| compression_error(err.to_string()))
    }
}

fn compression_error(error: impl Into<String>) -> Status {
    Status {
        code: StatusCode::CompressionError,
        err: error.into(),
    }
}

fn bedrock_db_options() -> Options {
    let mut compressors = CompressorList::new();
    compressors.set_with_id(
        NonZeroU8::new(2).unwrap_or(NonZeroU8::MIN),
        Deflate { zlib: true },
    );
    compressors.set_with_id(
        NonZeroU8::new(4).unwrap_or(NonZeroU8::MIN),
        Deflate { zlib: false },
    );
    Options {
        create_if_missing: false,
        paranoid_checks: true,
        compressor_list: Arc::new(compressors),
        compaction_mode: CompactionMode::Manual,
        ..Options::default()
    }
}

type ChunkIndex = BTreeMap<(i32, i32), BTreeMap<(i32, i32), u32>>;

fn index_chunks(
    db: &DB,
    stats: &mut ImportStats,
    mut on_blocker: impl FnMut(Error) -> Result<(), Error>,
) -> Result<ChunkIndex, Error> {
    let mut regions = ChunkIndex::new();
    for entry in db.new_iter()?.entries() {
        let (key, _) = entry?;
        let Some(record) = ChunkKey::read(&key) else {
            if key.starts_with(b"actorprefix") {
                stats.entity_records_excluded += 1;
            } else {
                stats.other_records_excluded += 1;
            }
            continue;
        };
        let (x, z) = record.pos;
        if let Err(error) = record.validate() {
            on_blocker(error.at_chunk(x, z))?;
            continue;
        }
        match record.tag {
            BIOMES | VERSION | SUBCHUNK => {
                let section = match record.subchunk.map(section_index).transpose() {
                    Ok(section) => section,
                    Err(error) => {
                        on_blocker(error.at_chunk(x, z))?;
                        continue;
                    }
                };
                let region = region_of(Vector2::new(x, z));
                let sections = regions
                    .entry((region.x, region.y))
                    .or_default()
                    .entry(record.pos)
                    .or_default();
                if let Some(section) = section {
                    *sections |= 1 << section;
                }
            }
            BLOCK_ENTITY => stats.block_entity_records_excluded += 1,
            ENTITY => stats.entity_records_excluded += 1,
            0x30 | 0x76 => {
                on_blocker(invalid("legacy Bedrock terrain is unsupported").at_chunk(x, z))?;
            }
            _ => stats.other_records_excluded += 1,
        }
    }
    Ok(regions)
}

struct ChunkKey {
    pos: (i32, i32),
    dimension: i32,
    tag: u8,
    subchunk: Option<i8>,
}

impl ChunkKey {
    fn read(key: &[u8]) -> Option<Self> {
        let (coordinates, record) = key.split_first_chunk::<8>()?;
        let [x0, x1, x2, x3, z0, z1, z2, z3] = *coordinates;
        let pos = (
            i32::from_le_bytes([x0, x1, x2, x3]),
            i32::from_le_bytes([z0, z1, z2, z3]),
        );
        let (dimension, tag, subchunk) = match record {
            [tag] => (0, *tag, None),
            [SUBCHUNK, y] => (0, SUBCHUNK, Some(y.cast_signed())),
            [d0, d1, d2, d3, tag] => (i32::from_le_bytes([*d0, *d1, *d2, *d3]), *tag, None),
            [d0, d1, d2, d3, SUBCHUNK, y] => (
                i32::from_le_bytes([*d0, *d1, *d2, *d3]),
                SUBCHUNK,
                Some(y.cast_signed()),
            ),
            _ => return None,
        };
        if !matches!(tag, 0x2b..=0x41 | 0x76 | 0x77) || key.iter().all(u8::is_ascii_graphic) {
            return None;
        }
        Some(Self {
            pos,
            dimension,
            tag,
            subchunk,
        })
    }

    fn validate(&self) -> Result<(), Error> {
        if self.pos.0.unsigned_abs() >= CHUNK_COORDINATE_LIMIT
            || self.pos.1.unsigned_abs() >= CHUNK_COORDINATE_LIMIT
        {
            return Err(invalid("Bedrock chunk is out of world bounds"));
        }
        if self.dimension != 0 {
            return Err(invalid("only Overworld terrain is supported"));
        }
        if self.tag == SUBCHUNK && self.subchunk.is_none() {
            return Err(invalid("subchunk key is missing its vertical index"));
        }
        Ok(())
    }
}

fn section_index(y: i8) -> Result<usize, Error> {
    usize::try_from(i32::from(y) - OVERWORLD.min_y.div_euclid(16))
        .ok()
        .filter(|index| *index < section_count())
        .ok_or_else(|| invalid(format!("subchunk {y} is out of world bounds")))
}

const fn section_count() -> usize {
    (OVERWORLD.height / 16) as usize
}

fn read_chunk(
    db: &DB,
    x: i32,
    z: i32,
    sections: u32,
    registry: &terrain::TerrainRegistry,
    items: &storage::StorageItemRegistry,
    stats: &mut ImportStats,
) -> Result<ChunkData, Error> {
    let mut key = [0; 10];
    key[..4].copy_from_slice(&x.to_le_bytes());
    key[4..8].copy_from_slice(&z.to_le_bytes());
    key[8] = VERSION;
    match db.try_get(&key[..9])?.as_deref() {
        Some([CHUNK_VERSION]) => {}
        Some(_) => return Err(invalid("unsupported Bedrock chunk version")),
        None => return Err(invalid("missing Bedrock chunk version")),
    }
    key[8] = BIOMES;
    let data = db
        .try_get(&key[..9])?
        .ok_or_else(|| invalid("missing biome record"))?;
    let biomes = terrain::biomes(&data, section_count(), registry)?;
    key[8] = SUBCHUNK;
    let mut blocks =
        vec![BlockPalette::Homogeneous(BlockStateId::AIR); section_count()].into_boxed_slice();
    for (index, palette) in blocks.iter_mut().enumerate() {
        if sections & (1 << index) != 0 {
            let y = (OVERWORLD.min_y / 16 + index as i32) as i8;
            key[9] = y.cast_unsigned();
            let bytes = db
                .try_get(&key)?
                .ok_or_else(|| invalid("missing subchunk record"))?;
            *palette = terrain::subchunk(&bytes, y, registry)?;
        }
    }
    let mut chunk = ChunkData::empty(x, z);
    chunk.section = ChunkSections::from_palettes(blocks, biomes, OVERWORLD.min_y);
    chunk.heightmap = std::sync::Mutex::new(chunk.calculate_heightmap());
    key[8] = BLOCK_ENTITY;
    if let Some(record) = db.try_get(&key[..9])? {
        let mut parsed = ImportStats::default();
        if storage::import_record(&record, &mut chunk, registry, items, &mut parsed)? {
            stats.block_entity_records_excluded -= 1;
        }
        stats.add_storage_results(parsed);
    }
    Ok(chunk)
}

fn read_level(
    source: &Path,
    registry: &terrain::TerrainRegistry,
    policy: GeneratorPolicy,
) -> Result<(LevelData, bool), Error> {
    let mut file = File::open(source.join("level.dat"))?;
    let mut header = [0; 8];
    file.read_exact(&mut header)?;
    let [a, b, c, d, e, f, g, h] = header;
    let version = u32::from_le_bytes([a, b, c, d]);
    let length = u64::from(u32::from_le_bytes([e, f, g, h]));
    if version != STORAGE_VERSION || length > MAX_LEVEL_DAT_BYTES {
        return Err(invalid("unsupported Bedrock level.dat version or length"));
    }
    let mut bytes = Vec::new();
    file.take(length + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return Err(invalid("Bedrock level.dat size does not match its header"));
    }
    let mut reader = NbtReadHelperBedrockDisk::new(Cursor::new(bytes.as_slice()));
    let data = Nbt::read(&mut reader)?;
    if reader.reader().position() != length {
        return Err(invalid("trailing data in Bedrock level.dat"));
    }
    let seed = Seed(
        data.get_long("RandomSeed")
            .ok_or_else(|| invalid("world seed is missing"))?
            .cast_unsigned(),
    );
    let mut default_generator_selected = false;
    let flat_layers = data.get_string("FlatWorldLayers");
    let mut info = match data.get_int("Generator") {
        Some(1) => LevelData::default(seed),
        Some(2) if !data.has("FlatWorldLayers") || flat_layers == Some("") => {
            if !matches!(policy, GeneratorPolicy::NormalIfUnspecified) {
                return Err(invalid(
                    "source has no flat generator layers. Use --new-chunks=normal to set the generator for new chunks",
                ));
            }
            default_generator_selected = true;
            LevelData::default(seed)
        }
        Some(2) => flat_level(seed, flat_layers, registry)?,
        _ => return Err(invalid("unsupported Bedrock world generator")),
    };
    if let Some(name) = data.get_string("LevelName") {
        name.clone_into(&mut info.level_name);
    }
    info.spawn_x = data
        .get_int("SpawnX")
        .ok_or_else(|| invalid("missing SpawnX"))?;
    info.spawn_y = data
        .get_int("SpawnY")
        .ok_or_else(|| invalid("missing SpawnY"))?;
    info.spawn_z = data
        .get_int("SpawnZ")
        .ok_or_else(|| invalid("missing SpawnZ"))?;
    if info.spawn_x.unsigned_abs() >= CHUNK_COORDINATE_LIMIT * 16
        || info.spawn_z.unsigned_abs() >= CHUNK_COORDINATE_LIMIT * 16
        || (info.spawn_y != AUTOMATIC_SPAWN_Y
            && !(OVERWORLD.min_y..OVERWORLD.min_y + OVERWORLD.height).contains(&info.spawn_y))
    {
        return Err(invalid("Bedrock spawn is out of world bounds"));
    }
    Ok((info, default_generator_selected))
}

fn flat_level(
    seed: Seed,
    json: Option<&str>,
    registry: &terrain::TerrainRegistry,
) -> Result<LevelData, Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Layer {
        block_name: String,
        count: i32,
    }
    #[derive(serde::Deserialize)]
    struct BedrockFlat {
        biome_id: u32,
        block_layers: Vec<Layer>,
        encoding_version: u8,
        structure_options: Option<serde_json::Value>,
        world_version: String,
    }
    let flat: BedrockFlat =
        serde_json::from_str(json.ok_or_else(|| invalid("missing flat world layers"))?)?;
    if flat.encoding_version != 6
        || flat.world_version != "version.post_1_18"
        || flat.structure_options.is_some()
    {
        return Err(invalid("unsupported Bedrock flat world settings"));
    }
    let biome = Biome::from_id(registry.biome(flat.biome_id)?)
        .ok_or_else(|| invalid("unknown flat world biome"))?;
    let mut layers = Vec::new();
    let mut height = 0i32;
    for layer in flat.block_layers {
        let block = Block::from_name(&layer.block_name).ok_or_else(|| {
            invalid(format!(
                "flat world block {} has no mapping",
                layer.block_name
            ))
        })?;
        height = height.saturating_add(layer.count);
        if layer.count <= 0 || height > OVERWORLD.height {
            return Err(invalid("invalid flat world layer height"));
        }
        layers.push(FlatPresetLayer {
            block: format!("minecraft:{}", block.name),
            height: layer.count,
        });
    }
    let mut info = LevelData::default_with_preset(seed, "flat");
    let dimension = info
        .world_gen_settings
        .dimensions
        .get_mut(OVERWORLD.minecraft_name)
        .ok_or_else(|| invalid("missing flat overworld preset"))?;
    let settings = FlatPresetSettings {
        biome: biome.registry_id.to_owned(),
        features: false,
        lakes: false,
        layers,
        structure_overrides: Some(StructureOverrides::Multiple(Vec::new())),
    };
    dimension.generator.settings =
        Some(GeneratorSettings::Compound(serde_json::to_value(settings)?));
    Ok(info)
}

fn update_spawn_height(info: &mut LevelData, chunk: &ChunkData) {
    if info.spawn_y == AUTOMATIC_SPAWN_Y
        && (chunk.x, chunk.z) == (info.spawn_x.div_euclid(16), info.spawn_z.div_euclid(16))
    {
        let heightmap = chunk
            .heightmap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let top = heightmap.get(
            ChunkHeightmapType::MotionBlocking,
            info.spawn_x.rem_euclid(16),
            info.spawn_z.rem_euclid(16),
            OVERWORLD.min_y,
        );
        info.spawn_y = (top + 1).min(OVERWORLD.min_y + OVERWORLD.height - 1);
    }
}
