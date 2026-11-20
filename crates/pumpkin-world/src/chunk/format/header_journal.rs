use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use pumpkin_util::math::vector2::Vector2;
use tracing::warn;
use xxhash_rust::xxh64::xxh64;

use crate::chunk::io::{read_exact_at, sync_parent, write_all_at, write_file_atomically};

const MAGIC: &[u8; 8] = b"SCLREG01";
const PREFIX_BYTES: usize = 24;
const CHECKSUM_BYTES: usize = 8;

pub(super) fn path(region: &Path) -> PathBuf {
    let mut path = region.as_os_str().to_os_string();
    path.push(".sculith-journal");
    PathBuf::from(path)
}

pub(super) fn open_file(path: &Path, writable: bool) -> io::Result<Option<File>> {
    match OpenOptions::new().read(true).write(writable).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if self::path(path).try_exists()? {
                return Err(invalid(
                    path,
                    "missing region with a pending header journal",
                ));
            }
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn read(
    path: &Path,
    region: Vector2<i32>,
    file: &File,
    header_bytes: usize,
    writable: bool,
) -> io::Result<(Vec<u8>, u64)> {
    let (header, file_len) = read_header(file, header_bytes)?;
    let journal_path = self::path(path);
    let Some(recovered) = read_journal(&journal_path, region, file_len, &header)? else {
        return Ok((header, file_len));
    };
    if writable {
        // An earlier buffered save may have been interrupted before syncing its payloads.
        file.sync_data()?;
        write_all_at(file, &recovered, 0)?;
        file.sync_data()?;
        remove(&journal_path, true)?;
        warn!("Recovered the region header for {}", path.display());
    }
    Ok((recovered, file_len))
}

pub(super) fn publish(
    path: &Path,
    region: Vector2<i32>,
    file: &File,
    header: &[u8],
    sync: bool,
    sync_file: impl Fn(&File) -> io::Result<()>,
    recovery_needed: &mut bool,
) -> io::Result<()> {
    if sync {
        sync_file(file)?;
    }
    let (before, file_len) = read_header(file, header.len())?;
    let journal_path = self::path(path);
    *recovery_needed = true;
    write(&journal_path, region, file_len, &before, header, sync)?;
    write_all_at(file, header, 0)?;
    if sync {
        sync_file(file)?;
    }
    remove(&journal_path, sync)?;
    *recovery_needed = false;
    Ok(())
}

fn read_header(file: &File, header_bytes: usize) -> io::Result<(Vec<u8>, u64)> {
    let file_len = file.metadata()?.len();
    let mut header = vec![0; header_bytes];
    let read_len = file_len.min(header_bytes as u64) as usize;
    read_exact_at(file, &mut header[..read_len], 0)?;
    Ok((header, file_len))
}

fn write(
    path: &Path,
    region: Vector2<i32>,
    file_len: u64,
    before: &[u8],
    after: &[u8],
    sync: bool,
) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(PREFIX_BYTES + before.len() + after.len() + CHECKSUM_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&region.x.to_be_bytes());
    bytes.extend_from_slice(&region.y.to_be_bytes());
    bytes.extend_from_slice(&file_len.to_be_bytes());
    bytes.extend_from_slice(before);
    bytes.extend_from_slice(after);
    bytes.extend_from_slice(&xxh64(&bytes, 0).to_be_bytes());
    write_file_atomically(path, &bytes, sync)
}

fn read_journal(
    path: &Path,
    region: Vector2<i32>,
    file_len: u64,
    current: &[u8],
) -> io::Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let body_bytes = PREFIX_BYTES + 2 * current.len();
    let journal_bytes = body_bytes + CHECKSUM_BYTES;
    let mut bytes = Vec::with_capacity(journal_bytes);
    file.take((journal_bytes + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() != journal_bytes
        || &bytes[..8] != MAGIC
        || bytes[8..12] != region.x.to_be_bytes()
        || bytes[12..16] != region.y.to_be_bytes()
        || bytes[16..24] != file_len.to_be_bytes()
        || bytes[body_bytes..] != xxh64(&bytes[..body_bytes], 0).to_be_bytes()
    {
        return Err(invalid(path, "invalid or mismatched header journal"));
    }
    let (before, after) = bytes[PREFIX_BYTES..body_bytes].split_at(current.len());
    if current
        .iter()
        .zip(before.iter().zip(after))
        .any(|(byte, (before, after))| byte != before && byte != after)
    {
        return Err(invalid(
            path,
            "on-disk header differs from both recorded headers",
        ));
    }
    Ok(Some(after.to_vec()))
}

fn remove(path: &Path, sync: bool) -> io::Result<()> {
    fs::remove_file(path)?;
    if sync {
        sync_parent(path)?;
    }
    Ok(())
}

fn invalid(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} ({reason})", path.display()),
    )
}
