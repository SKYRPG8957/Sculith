use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::Path,
};

use leveldb::DB;
use zip::ZipArchive;

use super::{
    Error, MAX_LEVEL_DAT_BYTES, bedrock_db_options, database_current, database_entry_name, invalid,
    open_database_copy,
};

pub(super) enum WorldInput<'a> {
    Directory(&'a Path),
    Archive(&'a Path),
}

impl<'a> WorldInput<'a> {
    pub(super) fn prepare(source: &'a Path, staging: &'a Path) -> Result<Self, Error> {
        if source.is_dir() {
            return Ok(Self::Directory(source));
        }
        if !source.is_file()
            || !source
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mcworld"))
        {
            return Err(invalid(
                "the Bedrock source must be a world directory or .mcworld file",
            ));
        }
        stage_archive(source, staging)?;
        Ok(Self::Archive(staging))
    }

    pub(super) const fn directory(&self) -> &Path {
        match self {
            Self::Directory(path) | Self::Archive(path) => path,
        }
    }

    pub(super) fn open_database(&self, staging: &Path) -> Result<DB, Error> {
        match self {
            Self::Directory(source) => open_database_copy(source, staging),
            Self::Archive(source) => Ok(DB::open(source.join("db"), bedrock_db_options())?),
        }
    }
}

fn stage_archive(path: &Path, staging: &Path) -> Result<(), Error> {
    let mut archive = ZipArchive::new(File::open(path)?).map_err(io::Error::other)?;
    if archive.has_duplicate_names() {
        return Err(invalid("duplicate entries in Bedrock world archive"));
    }
    if ["level.dat", "db/CURRENT"]
        .iter()
        .any(|name| archive.index_for_name(name).is_none())
    {
        return Err(invalid(
            "the archive must contain level.dat and db/CURRENT at its root",
        ));
    }
    let files: Vec<_> = archive
        .file_names_raw()
        .filter_map(|name| std::str::from_utf8(name).ok())
        .filter(|name| {
            *name == "level.dat"
                || name
                    .strip_prefix("db/")
                    .is_some_and(|name| name == "CURRENT" || database_entry_name(name))
        })
        .map(str::to_owned)
        .collect();
    let database = staging.join("db");
    fs::create_dir(&database)?;
    for name in files {
        let mut member = archive.by_name(&name).map_err(io::Error::other)?;
        if member.is_dir()
            || member.is_symlink()
            || member.encrypted()
            || member
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170_000, 0 | 0o100_000))
        {
            return Err(invalid(format!(
                "the archive member must be a regular file: {name}"
            )));
        }
        let size = member.size();
        if (name == "level.dat" && size > MAX_LEVEL_DAT_BYTES + 8)
            || (name == "db/CURRENT" && size > 128)
        {
            return Err(invalid(format!("oversized archive member: {name}")));
        }
        let limit = size
            .checked_add(1)
            .ok_or_else(|| invalid("invalid archive member size"))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.join(&name))?;
        if io::copy(&mut member.by_ref().take(limit), &mut output)? != size {
            return Err(invalid(format!(
                "archive member size does not match the header: {name}"
            )));
        }
    }
    database_current(&database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn stages_only_bedrock_metadata_and_records() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Saved World.MCWORLD");
        let staging = tempfile::tempdir_in(root.path()).unwrap();
        let files: [(&str, &[u8]); 6] = [
            ("level.dat", b"level fixture"),
            ("db/CURRENT", b"MANIFEST-000001\n"),
            ("db/MANIFEST-000001", b"manifest fixture"),
            ("db/000002.ldb", b"database fixture"),
            ("db/LOCK", b"source lock"),
            (
                "resource_packs/pack/manifest.json",
                b"external resource metadata",
            ),
        ];
        let mut zip = ZipWriter::new(File::create(&source).unwrap());
        for (name, bytes) in files {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        let input = WorldInput::prepare(&source, staging.path()).unwrap();
        assert_eq!(input.directory(), staging.path());
        for (name, bytes) in files.into_iter().take(4) {
            assert_eq!(fs::read(staging.path().join(name)).unwrap(), bytes);
        }
        assert!(!staging.path().join("db/LOCK").exists());
        assert!(!staging.path().join("resource_packs").exists());
    }
}
