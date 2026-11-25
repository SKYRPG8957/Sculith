use crate::game_rules::GameRule;

/// Shared rules with their Bedrock 1.26.63 packet names. The disk names use lowercase.
pub const BEDROCK_RULES: &[(GameRule, &str)] = &[
    (GameRule::AdvanceTime, "doDayLightCycle"),
    (GameRule::AdvanceWeather, "doWeatherCycle"),
    (GameRule::BlockDrops, "doTileDrops"),
    (GameRule::CommandBlockOutput, "commandBlockOutput"),
    (GameRule::CommandBlocksWork, "commandBlocksEnabled"),
    (GameRule::DrowningDamage, "drowningDamage"),
    (GameRule::EntityDrops, "doEntityDrops"),
    (GameRule::FallDamage, "fallDamage"),
    (GameRule::FireDamage, "fireDamage"),
    (GameRule::FreezeDamage, "freezeDamage"),
    (GameRule::KeepInventory, "keepInventory"),
    (GameRule::MobDrops, "doMobLoot"),
    (GameRule::MobGriefing, "mobGriefing"),
    (GameRule::NaturalHealthRegeneration, "naturalRegeneration"),
    (
        GameRule::PlayersSleepingPercentage,
        "playersSleepingPercentage",
    ),
    (
        GameRule::ProjectilesCanBreakBlocks,
        "projectilesCanBreakBlocks",
    ),
    (GameRule::Pvp, "pvp"),
    (GameRule::RandomTickSpeed, "randomTickSpeed"),
    (GameRule::SendCommandFeedback, "sendCommandFeedback"),
    (GameRule::ShowDeathMessages, "showDeathMessages"),
    (GameRule::SpawnMobs, "doMobSpawning"),
    (GameRule::SpawnPhantoms, "doInsomnia"),
    (GameRule::TntExplodes, "tntExplodes"),
    (GameRule::TntExplosionDropDecay, "tntExplosionDropDecay"),
];
