use std::sync::{Arc, PoisonError, atomic::Ordering};

use pumpkin_config::PVPConfig;
use pumpkin_data::game_rules::{GameRule, GameRuleValue};
use sculith_protocol::bedrock::client::{
    CGamerulesChanged, GameRule as BedrockGameRule, RuleValue,
};

use crate::{net::bedrock::BedrockClient, server::Server};

use super::World;

impl World {
    pub fn get_game_rule(&self, rule: &GameRule) -> GameRuleValue<i64, bool> {
        let level_info = self.level_info.load();
        match level_info.game_rules.get(rule) {
            GameRuleValue::Int(value) => GameRuleValue::Int(*value),
            GameRuleValue::Bool(value) => GameRuleValue::Bool(*value),
        }
    }

    pub fn set_game_rule(&self, rule: &GameRule, value: GameRuleValue<i64, bool>) {
        let previous = self.level_info.rcu(|current| {
            let mut updated = (**current).clone();
            match (updated.game_rules.get_mut(rule), &value) {
                (GameRuleValue::Int(target), GameRuleValue::Int(value)) if *target != *value => {
                    *target = *value;
                }
                (GameRuleValue::Bool(target), GameRuleValue::Bool(value)) if *target != *value => {
                    *target = *value;
                }
                _ => return Arc::clone(current),
            }
            Arc::new(updated)
        });

        match (rule, value) {
            (GameRule::Pvp, GameRuleValue::Bool(value)) if previous.game_rules.pvp != value => {
                if let Some(server) = self.server.upgrade()
                    && server.advanced_config.pvp.enabled
                {
                    self.publish_bedrock_pvp(&server.advanced_config.pvp, |packet| {
                        self.for_shared_level_worlds(Some(&server), |world| {
                            let players = world.players.load();
                            Self::broadcast_bedrock_grouped(
                                packet,
                                players.iter().filter_map(|player| {
                                    if player.bedrock_spawned.load(Ordering::SeqCst) {
                                        player.client.bedrock()
                                    } else {
                                        None
                                    }
                                }),
                            );
                        });
                    });
                }
            }
            (GameRule::AdvanceTime, GameRuleValue::Bool(_)) => {
                self.for_shared_level_worlds(self.server.upgrade().as_deref(), |world| {
                    let time = world
                        .level_time
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    time.send_time(world);
                });
            }
            (GameRule::SpectatorsGenerateChunks, GameRuleValue::Bool(_)) => {
                self.for_shared_level_worlds(self.server.upgrade().as_deref(), |world| {
                    for player in world
                        .players
                        .load()
                        .iter()
                        .filter(|player| player.is_spectator())
                    {
                        player.update_chunk_tickets_for_gamemode();
                    }
                });
            }
            _ => {}
        }
    }

    pub(crate) fn pvp_enabled(&self, config: &PVPConfig) -> bool {
        config.enabled && self.level_info.load().game_rules.pvp
    }

    pub(super) fn bedrock_pvp_rule(&self, config: &PVPConfig) -> BedrockGameRule {
        Self::pvp_rule(self.pvp_enabled(config), config.enabled)
    }

    pub(crate) fn sync_bedrock_pvp_after_spawn(&self, client: &BedrockClient) {
        if let Some(server) = self.server.upgrade() {
            self.publish_bedrock_pvp(&server.advanced_config.pvp, |packet| {
                client.try_enqueue_client_packet(packet);
            });
        }
    }

    fn pvp_rule(enabled: bool, can_be_modified: bool) -> BedrockGameRule {
        BedrockGameRule {
            rule_name: "pvp".to_owned(),
            rule_can_be_modified: can_be_modified,
            rule_value: RuleValue::Boolean(enabled),
        }
    }

    fn publish_bedrock_pvp(&self, config: &PVPConfig, mut send: impl FnMut(&CGamerulesChanged)) {
        loop {
            let enabled = self.pvp_enabled(config);
            send(&CGamerulesChanged {
                rule_data: vec![Self::pvp_rule(enabled, config.enabled)],
            });
            // Concurrent changes can enqueue first. Repair a stale send with the latest value.
            if enabled == self.pvp_enabled(config) {
                break;
            }
        }
    }

    fn for_shared_level_worlds(&self, server: Option<&Server>, mut apply: impl FnMut(&Self)) {
        apply(self);
        if let Some(server) = server {
            for world in server.worlds.load().iter() {
                if !std::ptr::eq(self, world.as_ref())
                    && Arc::ptr_eq(&self.level_info, &world.level_info)
                {
                    apply(world);
                }
            }
        }
    }
}
