use std::sync::{Arc, MutexGuard, PoisonError};

use pumpkin_nbt::compound::NbtCompound;
use sculith_protocol::{
    bedrock::client::update_attributes::{AttributeData, CUpdateAttributes},
    codec::var_ulong::VarULong,
    java::client::play::CSetExperience,
};
use sculith_util::math::experience;

use crate::{
    net::bedrock::BedrockClient,
    plugin::{
        player::{exp_change::PlayerExpChangeEvent, player_level_change::PlayerLevelChangeEvent},
        server::packet::{BedrockConnectionPacketSentEvent, PacketSentEvent},
    },
};

use super::Player;

#[derive(Default, Clone, Copy)]
struct Experience {
    level: i32,
    progress: f32,
    points: i32,
}

impl Experience {
    fn total_points(self) -> i32 {
        experience::points_to_level(self.level).saturating_add(self.points)
    }

    fn from_total(total: i32) -> Self {
        let (level, points) = experience::total_to_level_and_points(total.max(0));
        Self {
            level,
            progress: experience::progress_in_level(points, level),
            points,
        }
    }

    fn with_levels(self, level: i32, keep_progress: bool) -> Self {
        let points = if keep_progress {
            let old_size = experience::points_in_level(self.level);
            let new_size = experience::points_in_level(level);
            (self.points as f32 * (new_size as f32 / old_size as f32)) as i32
        } else {
            self.points
        };
        Self {
            level,
            points,
            ..self
        }
    }

    fn bedrock_packet(self, entity_id: i32) -> CUpdateAttributes {
        let attribute = |name: &str, current_value, max_value| AttributeData {
            min_value: 0.0,
            max_value,
            current_value,
            default_min_value: 0.0,
            default_max_value: max_value,
            default_value: 0.0,
            name: name.to_string(),
            modifiers: Vec::new(),
        };
        CUpdateAttributes {
            target_runtime_id: VarULong(entity_id as u64),
            attribute_list: vec![
                attribute(
                    "minecraft:player.experience",
                    self.progress.clamp(0.0, 1.0),
                    1.0,
                ),
                attribute("minecraft:player.level", self.level.max(0) as f32, 24_791.0),
            ],
            tick: VarULong(0),
        }
    }

    fn queue_bedrock(self, client: &BedrockClient, entity_id: i32) -> bool {
        let mut bytes = Vec::new();
        BedrockClient::write_raw_packet(&self.bedrock_packet(entity_id), &mut bytes).is_ok()
            && client.try_enqueue_packet_data_unobserved(bytes.into())
    }
}

pub(super) struct PlayerExperienceState {
    value: Experience,
    pending: bool,
}

impl Default for PlayerExperienceState {
    fn default() -> Self {
        Self {
            value: Experience::default(),
            pending: true,
        }
    }
}

impl PlayerExperienceState {
    const fn replace(&mut self, mut value: Experience) {
        value.progress = value.progress.clamp(0.0, 1.0);
        self.value = value;
        self.pending = true;
    }
}

impl Player {
    fn lock_experience(&self) -> MutexGuard<'_, PlayerExperienceState> {
        self.experience
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn add_experience(self: &Arc<Self>, points: i32) {
        self.add_experience_points(points);
    }

    pub fn add_levels(&self, levels: i32) {
        self.add_experience_levels(levels);
    }

    pub fn get_experience_level(&self) -> i32 {
        self.lock_experience().value.level
    }

    pub fn get_experience_progress(&self) -> f32 {
        self.lock_experience().value.progress
    }

    pub fn get_total_experience(&self) -> i32 {
        self.lock_experience().value.points
    }

    pub fn set_experience_progress(&self, progress: f32) {
        self.update_experience(|current| {
            let max = experience::points_in_level(current.level);
            Some(Experience {
                progress,
                points: (progress.clamp(0.0, 1.0) * max as f32) as i32,
                ..current
            })
        });
    }

    pub fn set_total_experience(&self, points: i32) {
        self.set_experience_points(points);
    }

    pub fn set_experience(&self, level: i32, progress: f32, points: i32) {
        self.update_experience(|_| {
            Some(Experience {
                level,
                progress,
                points,
            })
        });
    }

    pub fn set_experience_level(&self, level: i32, keep_progress: bool) {
        self.update_experience(|current| Some(current.with_levels(level, keep_progress)));
    }

    /// Sets the displayed progress without changing the level or stored points.
    pub fn set_experience_progress_value(&self, progress: f32) {
        self.update_experience(|current| {
            Some(Experience {
                progress,
                ..current
            })
        });
    }

    /// Sets the points without changing the level or displayed progress.
    pub fn set_experience_points_value(&self, points: i32) {
        self.update_experience(|current| Some(Experience { points, ..current }));
    }

    pub fn add_experience_levels(&self, levels: i32) {
        self.update_experience(|current| {
            Some(current.with_levels(current.level.saturating_add(levels), true))
        });
    }

    pub fn set_experience_points(&self, points: i32) -> bool {
        self.update_experience(|current| {
            if points == current.points {
                return Some(current);
            }
            (0..=experience::points_in_level(current.level))
                .contains(&points)
                .then(|| Experience {
                    points,
                    progress: experience::progress_in_level(points, current.level),
                    ..current
                })
        })
    }

    pub fn add_experience_points(self: &Arc<Self>, mut points: i32) {
        if let Some(server) = self.world().server.upgrade() {
            let mut event = PlayerExpChangeEvent::new(self.clone(), points);
            server.plugin_manager.fire_blocking(&server, &mut event);
            points = event.amount;
        }
        self.update_experience(|current| {
            Some(Experience::from_total(
                current.total_points().saturating_add(points),
            ))
        });
    }

    fn update_experience(&self, change: impl Fn(Experience) -> Option<Experience>) -> bool {
        let mut state = self.lock_experience();
        let Some(mut next) = change(state.value) else {
            return false;
        };
        let old_level = state.value.level;
        if old_level != next.level
            && let Some(server) = self.world().server.upgrade()
            && server
                .plugin_manager
                .has_handlers::<PlayerLevelChangeEvent>()
        {
            // The listener can read or edit this player's XP.
            drop(state);
            if let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) {
                let mut event = PlayerLevelChangeEvent::new(player, old_level, next.level);
                server.plugin_manager.fire_blocking(&server, &mut event);
            }
            state = self.lock_experience();
            let Some(current) = change(state.value) else {
                return false;
            };
            next = current;
        }
        state.replace(next);
        drop(state);
        self.tick_experience();
        true
    }

    pub fn tick_experience(&self) {
        if !self.has_client_loaded() {
            return;
        }
        let mut state = self.lock_experience();
        if !state.pending {
            return;
        }
        let experience = state.value;
        if let Some(client) = self
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
            && self.world().server.upgrade().is_some_and(|server| {
                !server.plugin_manager.has_handlers::<PacketSentEvent>()
                    && !server
                        .plugin_manager
                        .has_handlers::<BedrockConnectionPacketSentEvent>()
            })
        {
            state.pending = !experience.queue_bedrock(client, self.entity_id());
            return;
        }
        state.pending = false;
        drop(state);
        self.try_enqueue_packet_editioned(
            &CSetExperience::new(
                experience.progress.clamp(0.0, 1.0),
                experience.level.into(),
                experience.points.into(),
            ),
            &experience.bedrock_packet(self.entity_id()),
        );
    }

    pub(super) fn write_experience_nbt(&self, nbt: &mut NbtCompound) {
        let value = self.lock_experience().value;
        nbt.put_float("XpP", value.progress);
        nbt.put_int("XpLevel", value.level);
        nbt.put_int("XpTotal", value.total_points());
    }

    pub(super) fn read_experience_nbt(&self, nbt: &NbtCompound) {
        let value = nbt.get_int("XpLevel").map_or_else(
            || Experience::from_total(nbt.get_int("XpTotal").unwrap_or_default()),
            |level| {
                let progress = nbt.get_float("XpP").unwrap_or_default();
                let points = (progress * experience::points_in_level(level) as f32).round() as i32;
                Experience {
                    level,
                    progress,
                    points,
                }
            },
        );
        self.lock_experience().replace(value);
    }

    pub(crate) fn try_award_regional_experience(
        &self,
        amount: i32,
        remove: impl FnOnce() -> bool,
    ) -> Option<()> {
        let mut state = self.experience.try_lock().ok()?;
        let next = Experience::from_total(state.value.total_points().saturating_add(amount));
        remove().then(|| state.replace(next))
    }

    pub(crate) fn try_send_regional_experience(&self, client: &BedrockClient) {
        if let Ok(mut state) = self.experience.try_lock()
            && state.pending
        {
            state.pending = !state.value.queue_bedrock(client, self.entity_id());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Experience;

    #[test]
    fn awards_preserve_levels_points_and_progress() {
        let start = Experience::from_total(6);
        assert_eq!((start.level, start.points), (0, 6));
        let after = Experience::from_total(start.total_points() + 3);
        assert_eq!((after.level, after.points), (1, 2));
        assert!((after.progress - 2.0 / 9.0).abs() < f32::EPSILON);
        assert_eq!(after.total_points(), 9);
        assert_eq!(Experience::from_total(i32::MAX).total_points(), i32::MAX);
    }
}
