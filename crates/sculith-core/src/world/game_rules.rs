use std::sync::{Arc, PoisonError};

use pumpkin_data::{
    bedrock_game_rules::BEDROCK_RULES,
    game_rules::{GameRule, GameRuleRegistry, GameRuleValue},
};
use sculith_config::PVPConfig;
use sculith_protocol::bedrock::client::{
    CGamerulesChanged, GameRule as BedrockGameRule, RuleValue,
};

use crate::{net::bedrock::BedrockClient, server::Server};

use super::World;

fn bedrock_rule(rule: &GameRule) -> Option<&'static (GameRule, &'static str)> {
    BEDROCK_RULES.iter().find(|(known, _)| known == rule)
}

pub fn bedrock_rule_name(rule: &GameRule) -> Option<&'static str> {
    bedrock_rule(rule).map(|(_, name)| *name)
}

fn pack_bedrock_rules(
    registry: &GameRuleRegistry,
    rules: &[(GameRule, &str)],
    config: &PVPConfig,
) -> Vec<BedrockGameRule> {
    rules
        .iter()
        .map(|(rule, name)| {
            let editable = *rule != GameRule::Pvp || config.enabled;
            BedrockGameRule {
                rule_name: (*name).to_owned(),
                rule_can_be_modified: editable,
                rule_value: match registry.get(rule) {
                    GameRuleValue::Bool(value) => RuleValue::Boolean(*value && editable),
                    GameRuleValue::Int(value) => {
                        RuleValue::Integer((*value).clamp(0, i64::from(u32::MAX)) as u32)
                    }
                },
            }
        })
        .collect()
}

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

        let changed = match (previous.game_rules.get(rule), &value) {
            (GameRuleValue::Int(previous), GameRuleValue::Int(value)) => previous != value,
            (GameRuleValue::Bool(previous), GameRuleValue::Bool(value)) => previous != value,
            _ => false,
        };
        if changed
            && let Some(bedrock_rule) = bedrock_rule(rule)
            && let Some(server) = self.server.upgrade()
            && (*rule != GameRule::Pvp || server.advanced_config.pvp.enabled)
        {
            self.publish_bedrock_rules(
                &server.advanced_config.pvp,
                std::slice::from_ref(bedrock_rule),
                |packet| {
                    self.for_shared_level_worlds(Some(&server), |world| {
                        world.broadcast_bedrock_spawned(packet);
                    });
                },
            );
        }

        match (rule, value) {
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

    pub(super) fn bedrock_game_rules(
        registry: &GameRuleRegistry,
        config: &PVPConfig,
    ) -> Vec<BedrockGameRule> {
        pack_bedrock_rules(registry, BEDROCK_RULES, config)
    }

    pub(crate) fn sync_bedrock_game_rules(&self, client: &BedrockClient) {
        if let Some(server) = self.server.upgrade() {
            self.publish_bedrock_rules(&server.advanced_config.pvp, BEDROCK_RULES, |packet| {
                client.try_enqueue_client_packet(packet);
            });
        }
    }

    fn publish_bedrock_rules(
        &self,
        config: &PVPConfig,
        rules: &[(GameRule, &str)],
        mut send: impl FnMut(&CGamerulesChanged),
    ) {
        loop {
            let snapshot = self.level_info.load();
            send(&CGamerulesChanged {
                rule_data: pack_bedrock_rules(&snapshot.game_rules, rules, config),
            });
            // Repair an outdated send if a concurrent change queued its update first.
            if snapshot.game_rules == self.level_info.load().game_rules {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_rules_use_bedrock_names_and_respect_the_server_pvp_override() {
        let registry = GameRuleRegistry {
            keep_inventory: true,
            pvp: true,
            random_tick_speed: 7,
            ..GameRuleRegistry::default()
        };
        let config = PVPConfig {
            enabled: false,
            ..PVPConfig::default()
        };
        let rows = World::bedrock_game_rules(&registry, &config);
        assert!(rows.iter().any(|r| r.rule_name == "keepInventory"
            && r.rule_can_be_modified
            && matches!(r.rule_value, RuleValue::Boolean(true))));
        assert!(rows.iter().any(|r| r.rule_name == "randomTickSpeed"
            && r.rule_can_be_modified
            && matches!(r.rule_value, RuleValue::Integer(7))));
        assert!(rows.iter().any(|r| r.rule_name == "pvp"
            && !r.rule_can_be_modified
            && matches!(r.rule_value, RuleValue::Boolean(false))));
        assert_eq!(
            bedrock_rule_name(&GameRule::AdvanceTime),
            Some("doDayLightCycle")
        );
    }
}
