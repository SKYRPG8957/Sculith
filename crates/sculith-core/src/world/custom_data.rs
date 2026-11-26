use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use pumpkin_nbt::{
    COMPOUND_ID, Nbt,
    compound::NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperJava},
    serializer::{NbtWriteHelper, NbtWriteHelperJava},
};
use sculith_world::level::LevelFolder;

const CUSTOM_DATA_FILE: &str = "sculith_custom_data.nbt";
const LEGACY_CUSTOM_DATA_FILE: &str = "pumpkin_custom_data.nbt";

pub(super) struct CustomDataFile {
    path: PathBuf,
    loaded_legacy: bool,
    read_failure: Option<String>,
    save_order: Arc<tokio::sync::Mutex<()>>,
}

impl CustomDataFile {
    pub(super) fn load(folder: &LevelFolder) -> (Self, NbtCompound) {
        let path = folder.dim_folder.join(CUSTOM_DATA_FILE);
        let legacy = folder.root_folder.join(LEGACY_CUSTOM_DATA_FILE);
        let read = || {
            if let Some(data) = read_if_present(&path, false)? {
                return Ok((data, false));
            }
            let data = read_if_present(&legacy, true)?;
            let loaded_legacy = data.is_some();
            Ok::<_, io::Error>((data.unwrap_or_default(), loaded_legacy))
        };
        let (data, loaded_legacy, read_failure) = match read() {
            Ok((data, legacy)) => (data, legacy, None),
            Err(err) => {
                tracing::error!(%err, "Failed to load custom world data");
                (NbtCompound::new(), false, Some(err.to_string()))
            }
        };
        (
            Self {
                path,
                loaded_legacy,
                read_failure,
                save_order: Arc::default(),
            },
            data,
        )
    }

    pub(super) async fn save(&self, data: &Mutex<NbtCompound>) -> Result<(), String> {
        if let Some(error) = &self.read_failure {
            return Err(format!("Failed to save custom world data: {error}"));
        }
        let save = self.save_order.clone().lock_owned().await;
        let data = data.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let path = self.path.clone();
        let loaded_legacy = self.loaded_legacy;
        tokio::task::spawn_blocking(move || {
            // Keep the writer slot until the work ends even if the save caller leaves.
            let _save = save;
            if data.is_empty() && !loaded_legacy && !path.try_exists()? {
                return Ok(());
            }
            write_to_file(&path, data)
        })
        .await
        .map_err(|error| format!("Custom world data save task failed: {error}"))?
        .map_err(|error| format!("Failed to save {}: {error}", self.path.display()))
    }
}

fn read_if_present(path: &Path, legacy: bool) -> io::Result<Option<NbtCompound>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ));
        }
    };
    read_named(&bytes)
        .or_else(|| legacy.then(|| read_unnamed(&bytes)).flatten())
        .map(Some)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid custom world data in {}", path.display()),
            )
        })
}

fn read_named(bytes: &[u8]) -> Option<NbtCompound> {
    let mut reader = NbtReadHelperJava::new(io::Cursor::new(bytes));
    let data = Nbt::read(&mut reader).ok()?;
    (reader.reader().position() == bytes.len() as u64).then_some(data.root_tag)
}

fn read_unnamed(bytes: &[u8]) -> Option<NbtCompound> {
    let mut reader = NbtReadHelperJava::new(io::Cursor::new(bytes));
    let data = Nbt::read_unnamed(&mut reader).ok()?;
    (reader.reader().position() == bytes.len() as u64).then_some(data.root_tag)
}

fn write_to_file(path: &Path, data: NbtCompound) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Missing world data directory")
    })?;
    fs::create_dir_all(directory)?;
    let mut pending = tempfile::NamedTempFile::new_in(directory)?;
    let mut writer = NbtWriteHelperJava::new(pending.as_file_mut());
    writer
        .write_u8(COMPOUND_ID)
        .and_then(|()| writer.write_string(""))
        .and_then(|()| data.serialize_content(&mut writer))
        .map_err(io::Error::other)?;
    pending.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrates_saved_data_and_keeps_a_removed_value_removed() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().to_path_buf();
        let folder = LevelFolder::new(primary.clone(), primary.join("nether"));
        let legacy = primary.join(LEGACY_CUSTOM_DATA_FILE);
        let mut values = NbtCompound::new();
        values.put_int("sample", 17);
        let original = Nbt::from(values.clone()).write();
        fs::write(&legacy, &original).unwrap();

        let (file, loaded) = CustomDataFile::load(&folder);
        assert_eq!(loaded, values);
        file.save(&Mutex::new(NbtCompound::new())).await.unwrap();
        let (_, reloaded) = CustomDataFile::load(&folder);
        assert!(reloaded.is_empty());
        assert_eq!(fs::read(legacy).unwrap().as_slice(), original.as_ref());
    }
}
