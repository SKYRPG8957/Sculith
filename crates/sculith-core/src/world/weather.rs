use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError, TryLockError},
};

use super::World;
use rand::RngExt;
use sculith_protocol::bedrock::client::level_event::CLevelEvent;
use sculith_protocol::java::client::play::{CGameEvent, GameEvent};
use sculith_util::math::vector3::Vector3;
use sculith_world::world_info::data_files::WeatherData;

use crate::net::bedrock::BedrockClient;

// Weather timing constants
const RAIN_DELAY_MIN: i32 = 12_000;
const RAIN_DELAY_MAX: i32 = 180_000;
const RAIN_DURATION_MIN: i32 = 12_000;
const RAIN_DURATION_MAX: i32 = 24_000;
const THUNDER_DELAY_MIN: i32 = 12_000;
const THUNDER_DELAY_MAX: i32 = 180_000;
const THUNDER_DURATION_MIN: i32 = 3_600;
const THUNDER_DURATION_MAX: i32 = 15_600;

const WEATHER_TRANSITION_SPEED: f32 = 0.01;
const BEDROCK_WEATHER_STRENGTH: i32 = u16::MAX as i32;
const BEDROCK_START_RAIN: i32 = 3001;
const BEDROCK_START_THUNDER: i32 = 3002;
const BEDROCK_STOP_RAIN: i32 = 3003;
const BEDROCK_STOP_THUNDER: i32 = 3004;

#[derive(Default)]
pub(super) struct WeatherDelta {
    rain: Option<(bool, i32)>,
    thunder: Option<bool>,
    rain_level: Option<f32>,
    thunder_level: Option<f32>,
}

impl WeatherDelta {
    const fn is_empty(&self) -> bool {
        self.rain.is_none()
            && self.thunder.is_none()
            && self.rain_level.is_none()
            && self.thunder_level.is_none()
    }

    fn publish(self, world: &World) {
        if let Some((raining, stop_data)) = self.rain {
            let event = if raining {
                GameEvent::BeginRaining
            } else {
                GameEvent::EndRaining
            };
            world.broadcast_packet_all(&CGameEvent::new(event, 0.0));
            world.broadcast_bedrock_spawned(&bedrock_rain_event(raining, stop_data));
        }
        if let Some(thundering) = self.thunder {
            world.broadcast_bedrock_spawned(&bedrock_thunder_event(thundering));
        }
        if let Some(level) = self.rain_level {
            world.broadcast_packet_all(&CGameEvent::new(GameEvent::RainLevelChange, level));
        }
        if let Some(level) = self.thunder_level {
            world.broadcast_packet_all(&CGameEvent::new(GameEvent::ThunderLevelChange, level));
        }
    }
}

enum WeatherUpdate {
    Changed(WeatherDelta),
    Sync {
        client: Arc<BedrockClient>,
        raining: bool,
        thundering: bool,
    },
}

#[derive(Default)]
pub(super) struct WeatherPublications {
    pending: Mutex<VecDeque<WeatherUpdate>>,
    publisher: Mutex<()>,
}

impl WeatherPublications {
    pub(super) fn queue_change(&self, delta: WeatherDelta) -> bool {
        if delta.is_empty() {
            return false;
        }
        self.queue(WeatherUpdate::Changed(delta));
        true
    }

    pub(super) fn queue_sync(&self, client: Arc<BedrockClient>, weather: &Mutex<Weather>) {
        let weather = weather.lock().unwrap_or_else(PoisonError::into_inner);
        self.queue(WeatherUpdate::Sync {
            client,
            raining: weather.raining,
            thundering: weather.thundering,
        });
    }

    fn queue(&self, update: WeatherUpdate) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(update);
    }

    pub(super) fn publish(&self, world: &World) {
        self.drain(|update| match update {
            WeatherUpdate::Changed(delta) => delta.publish(world),
            WeatherUpdate::Sync {
                client,
                raining,
                thundering,
            } => {
                if client
                    .player
                    .load()
                    .as_ref()
                    .as_ref()
                    .is_some_and(|player| std::ptr::eq(player.world().as_ref(), world))
                {
                    client.try_enqueue_client_packet(&bedrock_rain_event(raining, 0));
                    client.try_enqueue_client_packet(&bedrock_thunder_event(thundering));
                }
            }
        });
    }

    fn drain(&self, mut publish: impl FnMut(WeatherUpdate)) {
        let publisher = match self.publisher.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        loop {
            let update = {
                let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
                let Some(update) = pending.pop_front() else {
                    // The next enqueue can take over after we release the queue.
                    drop(publisher);
                    return;
                };
                update
            };
            publish(update);
        }
    }
}

pub struct Weather {
    pub clear_weather_time: i32,
    pub raining: bool,
    pub rain_time: i32,
    pub thundering: bool,
    pub thunder_time: i32,

    pub rain_level: f32,
    pub old_rain_level: f32,
    pub thunder_level: f32,
    pub old_thunder_level: f32,

    pub weather_cycle_enabled: bool,
}

impl Default for Weather {
    fn default() -> Self {
        Self::new()
    }
}

impl Weather {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            clear_weather_time: 0,
            raining: false,
            rain_time: 0,
            thundering: false,
            thunder_time: 0,
            rain_level: 0.0,
            old_rain_level: 0.0,
            thunder_level: 0.0,
            old_thunder_level: 0.0,
            weather_cycle_enabled: true,
        }
    }

    pub(super) fn from_saved(data: &WeatherData) -> Self {
        let rain_level = f32::from(u8::from(data.raining));
        let thunder_level = f32::from(u8::from(data.thundering));
        Self {
            clear_weather_time: data.clear_weather_time,
            raining: data.raining,
            rain_time: data.rain_time,
            thundering: data.thundering,
            thunder_time: data.thunder_time,
            rain_level,
            old_rain_level: rain_level,
            thunder_level,
            old_thunder_level: thunder_level,
            weather_cycle_enabled: true,
        }
    }

    pub(crate) const fn saved_state(&self, data_version: i32) -> WeatherData {
        WeatherData {
            clear_weather_time: self.clear_weather_time,
            raining: self.raining,
            rain_time: self.rain_time,
            thundering: self.thundering,
            thunder_time: self.thunder_time,
            data_version,
        }
    }

    pub fn set_weather_parameters(
        &mut self,
        world: &World,
        clear_time: i32,
        rain_time: i32,
        raining: bool,
        thundering: bool,
    ) {
        self.update_parameters(clear_time, rain_time, raining, thundering)
            .publish(world);
    }

    pub(super) fn update_parameters(
        &mut self,
        clear_time: i32,
        rain_time: i32,
        raining: bool,
        thundering: bool,
    ) -> WeatherDelta {
        let previous = (self.raining, self.thundering);

        self.clear_weather_time = clear_time;
        self.rain_time = rain_time;
        self.thunder_time = if thundering { rain_time } else { 0 };
        self.raining = raining;
        self.thundering = thundering;

        self.changed_since(previous, 0)
    }

    pub(super) fn cycle_active(&self, world: &World) -> bool {
        self.weather_cycle_enabled && world.level_info.load().game_rules.advance_weather
    }

    pub fn tick_weather(&mut self, world: &World) {
        self.step(self.cycle_active(world)).publish(world);
    }

    pub(super) fn step(&mut self, cycle_active: bool) -> WeatherDelta {
        let previous = (self.raining, self.thundering);
        if cycle_active {
            self.advance_weather_cycle();
        }

        // Update visual transitions
        self.old_rain_level = self.rain_level;
        self.old_thunder_level = self.thunder_level;

        if self.raining {
            self.rain_level = (self.rain_level + WEATHER_TRANSITION_SPEED).min(1.0);
        } else {
            self.rain_level = (self.rain_level - WEATHER_TRANSITION_SPEED).max(0.0);
        }

        if self.thundering {
            self.thunder_level = (self.thunder_level + WEATHER_TRANSITION_SPEED).min(1.0);
        } else {
            self.thunder_level = (self.thunder_level - WEATHER_TRANSITION_SPEED).max(0.0);
        }

        // Bedrock distinguishes a rain timer ending from a command clearing the rain.
        let mut delta = self.changed_since(previous, i32::MIN);
        delta.rain_level = ((self.old_rain_level - self.rain_level).abs() > f32::EPSILON)
            .then_some(self.rain_level);
        delta.thunder_level = ((self.old_thunder_level - self.thunder_level).abs() > f32::EPSILON)
            .then_some(self.thunder_level);
        delta
    }

    fn changed_since(&self, previous: (bool, bool), stop_rain_data: i32) -> WeatherDelta {
        let (was_raining, was_thundering) = previous;
        WeatherDelta {
            rain: (was_raining != self.raining).then_some((self.raining, stop_rain_data)),
            thunder: (was_thundering != self.thundering).then_some(self.thundering),
            ..WeatherDelta::default()
        }
    }

    fn advance_weather_cycle(&mut self) {
        if self.clear_weather_time > 0 {
            self.clear_weather_time -= 1;
            self.thunder_time = i32::from(!self.thundering);
            self.rain_time = i32::from(!self.raining);
            self.thundering = false;
            self.raining = false;
        } else {
            // Handle thunder timing
            if self.thunder_time > 0 {
                self.thunder_time -= 1;
                if self.thunder_time == 0 {
                    self.thundering = !self.thundering;
                }
            } else if self.thundering {
                self.thunder_time =
                    rand::rng().random_range(THUNDER_DURATION_MIN..=THUNDER_DURATION_MAX);
            } else {
                self.thunder_time = rand::rng().random_range(THUNDER_DELAY_MIN..=THUNDER_DELAY_MAX);
            }

            // Handle rain timing
            if self.rain_time > 0 {
                self.rain_time -= 1;
                if self.rain_time == 0 {
                    self.raining = !self.raining;
                }
            } else if self.raining {
                self.rain_time = rand::rng().random_range(RAIN_DURATION_MIN..=RAIN_DURATION_MAX);
            } else {
                self.rain_time = rand::rng().random_range(RAIN_DELAY_MIN..=RAIN_DELAY_MAX);
            }
        }
    }

    pub fn reset_weather_cycle(&mut self, world: &World) {
        self.set_weather_parameters(world, 0, 0, false, false);
    }
}

fn bedrock_rain_event(raining: bool, stop_rain_data: i32) -> CLevelEvent {
    if raining {
        bedrock_weather_event(BEDROCK_START_RAIN, BEDROCK_WEATHER_STRENGTH)
    } else {
        bedrock_weather_event(BEDROCK_STOP_RAIN, stop_rain_data)
    }
}

fn bedrock_thunder_event(thundering: bool) -> CLevelEvent {
    if thundering {
        bedrock_weather_event(BEDROCK_START_THUNDER, BEDROCK_WEATHER_STRENGTH)
    } else {
        bedrock_weather_event(BEDROCK_STOP_THUNDER, 0)
    }
}

fn bedrock_weather_event(event: i32, data: i32) -> CLevelEvent {
    CLevelEvent {
        event_id: event.into(),
        position: Vector3::new(0.0, 0.0, 0.0),
        data: data.into(),
    }
}

impl Clone for Weather {
    fn clone(&self) -> Self {
        Self {
            clear_weather_time: self.clear_weather_time,
            raining: self.raining,
            rain_time: self.rain_time,
            thundering: self.thundering,
            thunder_time: self.thunder_time,
            rain_level: self.rain_level,
            old_rain_level: self.old_rain_level,
            thunder_level: self.thunder_level,
            old_thunder_level: self.old_thunder_level,
            weather_cycle_enabled: self.weather_cycle_enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_queued_by_a_packet_callback_publish_in_state_change_order() {
        let mut weather = Weather::new();
        let updates = WeatherPublications::default();
        assert!(updates.queue_change(weather.update_parameters(0, 40, true, true)));
        let mut seen = Vec::new();
        updates.drain(|update| {
            let WeatherUpdate::Changed(delta) = update else {
                panic!("unexpected weather synchronization");
            };
            seen.push(delta.rain);
            if delta.rain == Some((true, 0)) {
                assert!(updates.queue_change(weather.update_parameters(40, 0, false, false)));
                updates.drain(|_| panic!("another publisher is still delivering weather"));
            }
        });
        assert_eq!(seen, [Some((true, 0)), Some((false, 0))]);
    }
}
