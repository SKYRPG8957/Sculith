//! Durable simulation time for one loaded dimension.

use std::{
    fs::File,
    io::{self, ErrorKind},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::chunk::io::write_file_atomically;

const FILE_NAME: &str = ".sculith-world-clock.json";
const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorldClockSnapshot {
    pub time_of_day: i64,
    pub world_age: i64,
    pub partial_tick: f32,
    pub rate: f32,
    pub paused: bool,
}

#[derive(Deserialize, Serialize)]
struct ClockFile {
    version: u32,
    clock: WorldClockSnapshot,
}

impl Default for WorldClockSnapshot {
    fn default() -> Self {
        Self {
            time_of_day: 0,
            world_age: 0,
            partial_tick: 0.0,
            rate: 1.0,
            paused: false,
        }
    }
}

impl WorldClockSnapshot {
    pub fn read(dimension_folder: &Path) -> io::Result<Option<Self>> {
        let path = dimension_folder.join(FILE_NAME);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(clock_io_error(&path, &err)),
        };
        let saved: ClockFile = serde_json::from_reader(std::io::BufReader::new(file))
            .map_err(|err| clock_error(&path, err))?;
        if saved.version != VERSION {
            return Err(clock_error(
                &path,
                format_args!("unsupported version {}", saved.version),
            ));
        }
        saved.clock.validate(&path)?;
        Ok(Some(saved.clock))
    }

    pub fn write(self, dimension_folder: &Path) -> io::Result<()> {
        let path = dimension_folder.join(FILE_NAME);
        self.validate(&path)?;
        let bytes = serde_json::to_vec(&ClockFile {
            version: VERSION,
            clock: self,
        })
        .map_err(|err| clock_error(&path, err))?;
        write_file_atomically(&path, &bytes, true).map_err(|err| clock_io_error(&path, &err))
    }

    fn validate(self, path: &Path) -> io::Result<()> {
        if self.world_age < 0 || !(0.0..1.0).contains(&self.partial_tick) || !self.rate.is_finite()
        {
            return Err(clock_error(path, "invalid clock state"));
        }
        Ok(())
    }
}

fn clock_error(path: &Path, err: impl std::fmt::Display) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("{}: {err}", path.display()))
}

fn clock_io_error(path: &Path, err: &io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_clock_write_preserves_the_previous_snapshot() {
        let folder = tempfile::tempdir().unwrap();
        let clock = WorldClockSnapshot {
            time_of_day: 7_817,
            world_age: 12_803,
            partial_tick: 0.25,
            rate: 1.75,
            paused: true,
        };
        clock.write(folder.path()).unwrap();
        let invalid = WorldClockSnapshot {
            partial_tick: f32::NAN,
            ..clock
        };
        assert_eq!(
            invalid.write(folder.path()).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert_eq!(
            WorldClockSnapshot::read(folder.path()).unwrap(),
            Some(clock)
        );
    }
}
