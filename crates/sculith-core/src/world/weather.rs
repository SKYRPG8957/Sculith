use super::World;
use rand::RngExt;
use sculith_protocol::bedrock::client::level_event::CLevelEvent;
use sculith_protocol::java::client::play::{CGameEvent, GameEvent};
use sculith_util::math::vector3::Vector3;

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

    pub fn set_weather_parameters(
        &mut self,
        world: &World,
        clear_time: i32,
        rain_time: i32,
        raining: bool,
        thundering: bool,
    ) {
        let previous = (self.raining, self.thundering);

        self.clear_weather_time = clear_time;
        self.rain_time = rain_time;
        self.thunder_time = if thundering { rain_time } else { 0 };
        self.raining = raining;
        self.thundering = thundering;

        self.publish_weather_change(world, previous, 0);
    }

    pub(super) fn cycle_active(&self, world: &World) -> bool {
        self.weather_cycle_enabled && world.level_info.load().game_rules.advance_weather
    }

    pub fn tick_weather(&mut self, world: &World) {
        if self.cycle_active(world) {
            let previous = (self.raining, self.thundering);
            self.advance_weather_cycle();
            // Bedrock distinguishes a rain timer ending from a command clearing the rain.
            self.publish_weather_change(world, previous, i32::MIN);
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

        // Broadcast level changes if needed
        if (self.old_rain_level - self.rain_level).abs() > f32::EPSILON {
            world.broadcast_packet_all(&CGameEvent::new(
                GameEvent::RainLevelChange,
                self.rain_level,
            ));
        }

        if (self.old_thunder_level - self.thunder_level).abs() > f32::EPSILON {
            world.broadcast_packet_all(&CGameEvent::new(
                GameEvent::ThunderLevelChange,
                self.thunder_level,
            ));
        }
    }

    fn publish_weather_change(&self, world: &World, previous: (bool, bool), stop_rain_data: i32) {
        let (was_raining, was_thundering) = previous;
        if was_raining != self.raining {
            let java_event = if self.raining {
                GameEvent::BeginRaining
            } else {
                GameEvent::EndRaining
            };
            world.broadcast_packet_all(&CGameEvent::new(java_event, 0.0));
            world.broadcast_bedrock_spawned(&self.bedrock_rain_event(stop_rain_data));
        }
        if was_thundering != self.thundering {
            world.broadcast_bedrock_spawned(&self.bedrock_thunder_event());
        }
    }

    pub(crate) fn sync_bedrock(&self, client: &BedrockClient) {
        client.try_enqueue_client_packet(&self.bedrock_rain_event(0));
        client.try_enqueue_client_packet(&self.bedrock_thunder_event());
    }

    fn bedrock_rain_event(&self, stop_rain_data: i32) -> CLevelEvent {
        if self.raining {
            bedrock_weather_event(BEDROCK_START_RAIN, BEDROCK_WEATHER_STRENGTH)
        } else {
            bedrock_weather_event(BEDROCK_STOP_RAIN, stop_rain_data)
        }
    }

    fn bedrock_thunder_event(&self) -> CLevelEvent {
        if self.thundering {
            bedrock_weather_event(BEDROCK_START_THUNDER, BEDROCK_WEATHER_STRENGTH)
        } else {
            bedrock_weather_event(BEDROCK_STOP_THUNDER, 0)
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
