use std::{fs, path::Path};

use pumpkin_data::{bedrock_game_rules::BEDROCK_RULES, game_rules::GameRuleValue};
use pumpkin_nbt::compound::NbtCompound;

use crate::world_info::LevelData;

use super::{Error, MAX_LEVEL_DAT_BYTES, STORAGE_VERSION, invalid, read_level_nbt};

pub(super) fn export(output: &Path, world: &LevelData) -> Result<(), Error> {
    let mut native = read_level_nbt(output)?;
    update(&mut native.root_tag, world)?;
    let mut bytes = vec![0; 8];
    native.write_to_writer_bedrock_disk(&mut bytes)?;
    let length = (bytes.len() - 8) as u64;
    if length > MAX_LEVEL_DAT_BYTES {
        return Err(invalid("exported Bedrock level.dat is too large"));
    }
    bytes[..4].copy_from_slice(&STORAGE_VERSION.to_le_bytes());
    bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    fs::write(output.join("level.dat"), bytes)?;
    Ok(())
}

fn update(native: &mut NbtCompound, world: &LevelData) -> Result<(), Error> {
    for (rule, wire_name) in BEDROCK_RULES {
        let saved_name = wire_name.to_ascii_lowercase();
        match world.game_rules.get(rule) {
            GameRuleValue::Bool(value) => native.put_bool(&saved_name, *value),
            GameRuleValue::Int(value) => {
                let value = i32::try_from(*value).map_err(|_| {
                    invalid(format!("Bedrock game rule {saved_name} is out of range"))
                })?;
                native.put_int(&saved_name, value);
            }
        }
    }
    native.put_long("Time", world.day_time);

    let clear = world.clear_weather_time;
    let (raining, thundering, rain_time, thunder_time) =
        world
            .weather
            .as_ref()
            .map_or((false, false, 0, 0), |weather| {
                (
                    weather.raining,
                    weather.thundering,
                    weather.rain_time,
                    weather.thunder_time,
                )
            });
    // Bedrock saves a timed clear as two inactive precipitation countdowns.
    let (rain_time, thunder_time) = if clear > 0 {
        if raining || thundering {
            return Err(invalid("active weather conflicts with a clear countdown"));
        }
        (clear, clear)
    } else {
        (rain_time, thunder_time)
    };
    put_level(native, "rainLevel", raining);
    put_level(native, "lightningLevel", thundering);
    native.put_int("rainTime", rain_time);
    native.put_int("lightningTime", thunder_time);
    Ok(())
}

fn put_level(native: &mut NbtCompound, name: &str, active: bool) {
    let level = if active {
        // Keep an existing active level. Stock BDS records 0.02 when
        // starting precipitation with its weather cycle paused.
        native
            .get_float(name)
            .filter(|value| *value > 0.0)
            .unwrap_or(0.02)
    } else {
        0.0
    };
    native.put_float(name, level);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sculith_util::world_seed::Seed;

    #[test]
    fn writes_native_precipitation_countdowns_for_timed_clear() {
        let mut native = NbtCompound::new();
        native.put_float("rainLevel", 0.02);
        native.put_float("lightningLevel", 0.02);
        native.put_long("currentTick", 12_000);
        let mut world = LevelData::default(Seed(42));
        world.day_time = 9876;
        world.clear_weather_time = 800;
        world.game_rules.advance_weather = false;
        world.game_rules.random_tick_speed = 7;

        update(&mut native, &world).unwrap();
        assert_eq!(native.get_float("rainLevel"), Some(0.0));
        assert_eq!(native.get_float("lightningLevel"), Some(0.0));
        assert_eq!(native.get_int("rainTime"), Some(800));
        assert_eq!(native.get_int("lightningTime"), Some(800));
        assert_eq!(native.get_long("Time"), Some(9876));
        assert_eq!(native.get_long("currentTick"), Some(12_000));
        assert_eq!(native.get_byte("doweathercycle"), Some(0));
        assert_eq!(native.get_int("randomtickspeed"), Some(7));
    }
}
