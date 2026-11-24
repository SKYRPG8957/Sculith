use super::surface::{MATERIALS, Summary, Surface, summarize};
use bytes::Bytes;
use pumpkin_world::{
    chunk::{
        ChunkData,
        format::{
            anvil::AnvilRegion,
            linear::LinearV2File,
            pump::{PUMP_CHUNK_MAGIC, PumpFile},
        },
        io::{RegionFile, converter::RegionFormat},
    },
    level::LevelFolder,
};
use sculith_util::math::vector2::Vector2;
use std::{
    collections::BTreeMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
pub(super) type Grid = (i32, i32);
pub(super) type Atlas = BTreeMap<Grid, Summary>;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    bytes: u64,
    nanos: u128,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let stat = fs::metadata(path).ok()?;
    Some(Stamp {
        bytes: stat.len(),
        nanos: stat
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    })
}

enum Reader {
    Anvil(Box<AnvilRegion>),
    Linear(Box<LinearV2File>),
    Pump(Box<PumpFile>),
}

impl Reader {
    fn open(kind: RegionFormat, path: &Path, r: Grid) -> io::Result<Self> {
        let pos = Vector2::new(r.0, r.1);
        match kind {
            RegionFormat::Anvil => AnvilRegion::open(path, pos).map(Box::new).map(Self::Anvil),
            RegionFormat::Linear => LinearV2File::open(path, pos)
                .map(Box::new)
                .map(Self::Linear),
            RegionFormat::Pump => PumpFile::open(path, pos).map(Box::new).map(Self::Pump),
        }
        .map_err(io::Error::other)
    }

    fn occupancy(&self) -> Vec<usize> {
        (0..1024)
            .rev()
            .filter(|&i| match self {
                Self::Anvil(r) => r.has_chunk(i),
                Self::Linear(r) => r.has_chunk(i),
                Self::Pump(r) => r.has_chunk(i),
            })
            .collect()
    }

    fn read(&self, i: usize) -> io::Result<Option<Bytes>> {
        fn read<R: RegionFile>(r: &R, i: usize) -> io::Result<Option<Bytes>> {
            r.read_chunk(i)
                .and_then(|raw| raw.map(R::decode_chunk).transpose())
                .map_err(io::Error::other)
        }
        match self {
            Self::Anvil(r) => read(&**r, i),
            Self::Linear(r) => read(&**r, i),
            Self::Pump(r) => read(&**r, i),
        }
    }
}

struct Scan {
    reader: Reader,
    pending: Vec<usize>,
    rows: Atlas,
    clean: bool,
}

pub(super) struct Region {
    kind: RegionFormat,
    coord: Grid,
    source: PathBuf,
    cache: PathBuf,
    seed: u64,
    stamp: Option<Stamp>,
    scan: Option<Scan>,
    pub rows: Atlas,
}

impl Region {
    pub fn new(folder: &LevelFolder, seed: u64, kind: RegionFormat, coord: Grid) -> Self {
        let name = kind.file_name(Vector2::new(coord.0, coord.1));
        Self {
            kind,
            coord,
            source: folder.region_folder.join(&name),
            cache: folder.dim_folder.join("sculith-lod").join(name + ".tiles"),
            seed,
            stamp: None,
            scan: None,
            rows: Atlas::new(),
        }
    }

    pub fn refresh(&mut self) {
        let now = stamp(&self.source);
        if now == self.stamp {
            return;
        }
        self.stamp = now;
        self.scan = None;
        let Some(source_stamp) = now else {
            self.rows.clear();
            return;
        };
        if let Some(rows) = self.restore(source_stamp) {
            self.rows = rows;
            return;
        }
        match Reader::open(self.kind, &self.source, self.coord) {
            Ok(reader) => {
                let pending = reader.occupancy();
                self.scan = Some(Scan {
                    reader,
                    pending,
                    rows: Atlas::new(),
                    clean: true,
                });
            }
            Err(error) => {
                self.stamp = None;
                tracing::debug!(%error, path=%self.source.display(),"Could not read distant terrain region");
            }
        }
    }

    pub const fn pending(&self) -> bool {
        self.scan.is_some()
    }

    /// Decodes at most one saved chunk and never installs it in the live level.
    pub fn step(&mut self) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        if let Some(i) = scan.pending.pop() {
            let pos = (
                self.coord.0 * 32 + (i % 32) as i32,
                self.coord.1 * 32 + (i / 32) as i32,
            );
            let result = scan.reader.read(i).and_then(|bytes| {
                let Some(bytes) = bytes else { return Ok(None) };
                let pos = Vector2::new(pos.0, pos.1);
                let chunk = if bytes.starts_with(&PUMP_CHUNK_MAGIC) {
                    ChunkData::internal_from_pump_bytes(&bytes, pos)
                } else {
                    ChunkData::internal_from_bytes(&bytes, pos)
                }
                .map_err(io::Error::other)?;
                Ok(Some(summarize(&chunk)))
            });
            match result {
                Ok(Some(summary)) => {
                    if summary.iter().any(Option::is_some) {
                        scan.rows.insert(pos, summary);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    scan.clean = false;
                    tracing::debug!(%error,?pos,"Skipped unreadable distant terrain chunk");
                }
            }
        }
        if scan.pending.is_empty() {
            let Some(finished) = self.scan.take() else {
                return;
            };
            if self.stamp != stamp(&self.source) {
                self.refresh();
                return;
            }
            self.rows = finished.rows;
            if finished.clean
                && let Some(s) = self.stamp
                && let Err(error) = self.persist(s)
            {
                tracing::debug!(%error, path=%self.cache.display(),"Could not save distant terrain cache");
            }
        }
    }

    fn header(&self, stamp: Stamp) -> Vec<u8> {
        let mut b = b"SCLLOD01".to_vec();
        b.extend(self.seed.to_le_bytes());
        b.extend(self.coord.0.to_le_bytes());
        b.extend(self.coord.1.to_le_bytes());
        b.extend(stamp.bytes.to_le_bytes());
        b.extend(stamp.nanos.to_le_bytes());
        b
    }
    fn checksum(data: &[u8]) -> u64 {
        let mut hash = DefaultHasher::new();
        data.hash(&mut hash);
        hash.finish()
    }
    fn persist(&self, stamp: Stamp) -> io::Result<()> {
        let mut b = self.header(stamp);
        for (&(x, z), cells) in &self.rows {
            let i = (z.rem_euclid(32) * 32 + x.rem_euclid(32)) as u16;
            b.extend(i.to_le_bytes());
            for s in cells {
                b.push(s.map_or(255, |v| v.material));
                b.extend(s.map_or(0, |v| v.y).to_le_bytes());
            }
        }
        b.extend(Self::checksum(&b).to_le_bytes());
        let dir = self
            .cache
            .parent()
            .ok_or_else(|| io::Error::other("missing cache directory"))?;
        fs::create_dir_all(dir)?;
        let tmp = self
            .cache
            .with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, &b)?;
        if let Err(e) = fs::rename(&tmp, &self.cache) {
            let _ = fs::remove_file(tmp);
            return Err(e);
        }
        Ok(())
    }
    fn restore(&self, stamp: Stamp) -> Option<Atlas> {
        const ROW: usize = 2 + 5 * 5;
        let head = self.header(stamp);
        if fs::metadata(&self.cache).ok()?.len() > (head.len() + ROW * 1024 + 8) as u64 {
            return None;
        }
        let b = fs::read(&self.cache).ok()?;
        let (body, tail) = b.split_at_checked(b.len().checked_sub(8)?)?;
        if tail != Self::checksum(body).to_le_bytes() {
            return None;
        }
        let rows = body.strip_prefix(head.as_slice())?;
        if rows.len() % ROW != 0 {
            return None;
        }
        let mut result = Atlas::new();
        for row in rows.as_chunks::<ROW>().0 {
            let i = u16::from_le_bytes(row[..2].try_into().ok()?);
            if i >= 1024 {
                return None;
            }
            let pos = (
                self.coord.0 * 32 + i32::from(i % 32),
                self.coord.1 * 32 + i32::from(i / 32),
            );
            let mut cells = [None; 5];
            for (cell, data) in cells.iter_mut().zip(row[2..].as_chunks::<5>().0) {
                if data[0] != 255 {
                    if usize::from(data[0]) >= MATERIALS.len() {
                        return None;
                    }
                    *cell = Some(Surface {
                        y: i32::from_le_bytes(data[1..].try_into().ok()?),
                        material: data[0],
                    });
                }
            }
            if result.insert(pos, cells).is_some() {
                return None;
            }
        }
        Some(result)
    }
}

impl Region {
    pub fn add_rows(&self, target: &mut Atlas) {
        target.extend(self.rows.iter().map(|(&k, &v)| (k, v)));
        if let Some(scan) = &self.scan {
            target.extend(scan.rows.iter().map(|(&k, &v)| (k, v)));
        }
    }
}
