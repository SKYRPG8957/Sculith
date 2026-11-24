use std::sync::atomic::{AtomicI64, Ordering};

use pumpkin_data::{effect::StatusEffect, potion::Effect};
use sculith_protocol::bedrock::client::set_actor_data::{
    CSetActorData, MetadataValue, PropertySyncData, SyncedActorDataList, entity_data_key,
};

const MAX_VISIBLE_EFFECTS: usize = 8;
const EFFECT_ENTRY_BITS: u32 = 7;
const PARTICLES_DIRTY: i64 = i64::MIN;
const PARTICLES_MASK: i64 = i64::MAX;

/// The packed value fits in 56 bits, leaving a bit for a pending metadata update.
#[derive(Default)]
pub struct BedrockEffectParticles(AtomicI64);

impl BedrockEffectParticles {
    pub(crate) fn pack<'a>(effects: impl Iterator<Item = &'a Effect>) -> i64 {
        let mut visible_ids = 0u64;
        let mut ambient_ids = 0u64;
        for effect in effects.filter(|effect| effect.show_particles) {
            let Some(id) = visible_effect_id(effect.effect_type) else {
                continue;
            };
            let bit = 1u64 << id;
            visible_ids |= bit;
            if effect.ambient {
                ambient_ids |= bit;
            }
        }

        let mut packed = 0;
        for _ in 0..MAX_VISIBLE_EFFECTS {
            if visible_ids == 0 {
                break;
            }
            let id = visible_ids.trailing_zeros();
            let bit = 1u64 << id;
            let ambient = i64::from(ambient_ids & bit != 0);
            packed = (packed << EFFECT_ENTRY_BITS) | (i64::from(id) << 1) | ambient;
            visible_ids &= !bit;
        }
        packed
    }

    pub(crate) fn packed(&self) -> i64 {
        self.0.load(Ordering::Acquire) & PARTICLES_MASK
    }

    /// The caller holds the active effects lock while changing the packed value.
    pub(crate) fn update(&self, packed: i64) {
        if self.packed() != packed {
            self.finish_publication(packed, false);
        }
    }

    pub(crate) fn pending(&self, packed: i64) -> Option<i64> {
        (self.0.load(Ordering::Acquire) != packed).then_some(packed)
    }

    pub(crate) fn finish_publication(&self, packed: i64, queued: bool) {
        self.0.store(
            if queued {
                packed
            } else {
                packed | PARTICLES_DIRTY
            },
            Ordering::Release,
        );
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.0.load(Ordering::Acquire) < 0
    }

    pub(crate) fn take_dirty(&self) -> Option<i64> {
        let previous = self.0.fetch_and(PARTICLES_MASK, Ordering::AcqRel);
        (previous < 0).then_some(previous & PARTICLES_MASK)
    }

    /// Effects that BDS also sends to mob viewers as `MobEffect` packets.
    pub(crate) fn viewer_effect_id(effect: &StatusEffect) -> Option<i32> {
        match effect.minecraft_name {
            "minecraft:speed"
            | "minecraft:slowness"
            | "minecraft:jump_boost"
            | "minecraft:levitation"
            | "minecraft:slow_falling"
            | "minecraft:weaving" => visible_effect_id(effect).map(|id| id as i32),
            _ => None,
        }
    }

    pub(crate) fn write_actor_data(actor_data: &mut SyncedActorDataList, packed: i64) {
        actor_data.set(
            entity_data_key::VISIBLE_MOB_EFFECTS,
            MetadataValue::Int64(packed),
        );
    }

    pub(crate) fn packet(entity_id: i32, packed: i64) -> CSetActorData {
        let mut actor_data = SyncedActorDataList::new();
        Self::write_actor_data(&mut actor_data, packed);
        CSetActorData {
            target_runtime_id: (entity_id as u64).into(),
            actor_data,
            synced_properties: PropertySyncData::default(),
            tick: 0u64.into(),
        }
    }
}

fn visible_effect_id(effect: &StatusEffect) -> Option<u32> {
    // Use the native visibility catalog for effects whose IDs differ from the
    // inherited MobEffect mapping.
    let id = match effect.minecraft_name {
        "minecraft:glowing"
        | "minecraft:dolphins_grace"
        | "minecraft:luck"
        | "minecraft:unluck" => {
            return None;
        }
        "minecraft:wind_charged" => 32,
        "minecraft:weaving" => 33,
        "minecraft:oozing" => 34,
        "minecraft:infested" => 35,
        "minecraft:raid_omen" => 36,
        "minecraft:breath_of_the_nautilus" => 37,
        _ => effect.to_bedrock_id(),
    };
    (1..(1 << (EFFECT_ENTRY_BITS - 1)))
        .contains(&id)
        .then_some(id as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(effect_type: &'static StatusEffect) -> Effect {
        Effect {
            effect_type,
            duration: 20,
            amplifier: 0,
            ambient: false,
            show_particles: true,
            show_icon: true,
            blend: false,
        }
    }

    #[test]
    fn bedrock_mob_effect_particles_follow_the_native_order_and_limit() {
        let mut fire = visible(&StatusEffect::FIRE_RESISTANCE);
        let night = visible(&StatusEffect::NIGHT_VISION);
        assert_eq!(
            BedrockEffectParticles::pack([&night, &fire].into_iter()),
            3104
        );
        fire.ambient = true;
        assert_eq!(BedrockEffectParticles::pack([&fire].into_iter()), 25);

        let effects = [
            night,
            visible(&StatusEffect::GLOWING),
            visible(&StatusEffect::BLINDNESS),
            visible(&StatusEffect::WATER_BREATHING),
            visible(&StatusEffect::RESISTANCE),
            visible(&StatusEffect::JUMP_BOOST),
            visible(&StatusEffect::STRENGTH),
            visible(&StatusEffect::MINING_FATIGUE),
            visible(&StatusEffect::HASTE),
            Effect {
                show_particles: false,
                ..visible(&StatusEffect::SPEED)
            },
            visible(&StatusEffect::FIRE_RESISTANCE),
        ];
        assert_eq!(
            BedrockEffectParticles::pack(effects.iter()),
            3_413_232_031_501_598
        );
        assert_eq!(
            BedrockEffectParticles::pack(
                [&visible(&StatusEffect::BREATH_OF_THE_NAUTILUS)].into_iter()
            ),
            74
        );
        assert_eq!(visible_effect_id(&StatusEffect::RAID_OMEN), Some(36));
        assert_eq!(visible_effect_id(&StatusEffect::INFESTED), Some(35));
        assert_eq!(
            BedrockEffectParticles::viewer_effect_id(&StatusEffect::WEAVING),
            Some(33)
        );
        assert_eq!(
            BedrockEffectParticles::viewer_effect_id(&StatusEffect::NIGHT_VISION),
            None
        );
    }

    #[test]
    fn bedrock_mob_effect_particles_keep_the_current_value_until_retried() {
        let particles = BedrockEffectParticles::default();
        assert_eq!(particles.take_dirty(), None);
        particles.update(3104);
        assert_eq!(particles.packed(), 3104);
        assert_eq!(particles.take_dirty(), Some(3104));
        particles.update(3104);
        assert!(!particles.is_dirty());

        assert_eq!(particles.pending(24), Some(24));
        particles.finish_publication(24, false);
        assert_eq!(particles.packed(), 24);
        assert_eq!(particles.pending(24), Some(24));
        assert_eq!(particles.take_dirty(), Some(24));
        assert_eq!(particles.pending(24), None);

        particles.finish_publication(0, true);
        assert_eq!(particles.packed(), 0);
        assert_eq!(particles.take_dirty(), None);
    }
}
