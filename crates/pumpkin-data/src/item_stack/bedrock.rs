//! Edition mappings for Bedrock item variants and enchantments.

use std::borrow::Cow;

use crate::{
    Enchantment,
    data_component_impl::{PotionContentsImpl, StatusEffectInstance},
    effect::StatusEffect,
    item::BedrockItem,
    potion::Potion,
};
use pumpkin_nbt::{NbtCompound, tag::NbtTag};

const DECAY_AUX: u32 = 36;
const FIRST_TIPPED_ARROW_AUX: u32 = 6;
const DECAY_EFFECT: StatusEffectInstance = StatusEffectInstance {
    effect_id: Cow::Borrowed(StatusEffect::WITHER.minecraft_name),
    amplifier: 1,
    duration: 800,
    ambient: false,
    show_particles: true,
    show_icon: true,
};

// Bedrock aux values, checked against Dragonfly's potion registry and the
// newer variants in Geyser. Bedrock's wither potion has no shared registry entry.
const POTIONS: [Option<&Potion>; 47] = [
    Some(&Potion::WATER),
    Some(&Potion::MUNDANE),
    Some(&Potion::MUNDANE), // Bedrock's extended mundane potion.
    Some(&Potion::THICK),
    Some(&Potion::AWKWARD),
    Some(&Potion::NIGHT_VISION),
    Some(&Potion::LONG_NIGHT_VISION),
    Some(&Potion::INVISIBILITY),
    Some(&Potion::LONG_INVISIBILITY),
    Some(&Potion::LEAPING),
    Some(&Potion::LONG_LEAPING),
    Some(&Potion::STRONG_LEAPING),
    Some(&Potion::FIRE_RESISTANCE),
    Some(&Potion::LONG_FIRE_RESISTANCE),
    Some(&Potion::SWIFTNESS),
    Some(&Potion::LONG_SWIFTNESS),
    Some(&Potion::STRONG_SWIFTNESS),
    Some(&Potion::SLOWNESS),
    Some(&Potion::LONG_SLOWNESS),
    Some(&Potion::WATER_BREATHING),
    Some(&Potion::LONG_WATER_BREATHING),
    Some(&Potion::HEALING),
    Some(&Potion::STRONG_HEALING),
    Some(&Potion::HARMING),
    Some(&Potion::STRONG_HARMING),
    Some(&Potion::POISON),
    Some(&Potion::LONG_POISON),
    Some(&Potion::STRONG_POISON),
    Some(&Potion::REGENERATION),
    Some(&Potion::LONG_REGENERATION),
    Some(&Potion::STRONG_REGENERATION),
    Some(&Potion::STRENGTH),
    Some(&Potion::LONG_STRENGTH),
    Some(&Potion::STRONG_STRENGTH),
    Some(&Potion::WEAKNESS),
    Some(&Potion::LONG_WEAKNESS),
    None, // Decay uses a custom effect.
    Some(&Potion::TURTLE_MASTER),
    Some(&Potion::LONG_TURTLE_MASTER),
    Some(&Potion::STRONG_TURTLE_MASTER),
    Some(&Potion::SLOW_FALLING),
    Some(&Potion::LONG_SLOW_FALLING),
    Some(&Potion::STRONG_SLOWNESS),
    Some(&Potion::WIND_CHARGED),
    Some(&Potion::WEAVING),
    Some(&Potion::OOZING),
    Some(&Potion::INFESTED),
];

#[must_use]
pub const fn uses_potion_aux(id: i16) -> bool {
    id == BedrockItem::POTION.id
        || id == BedrockItem::SPLASH_POTION.id
        || id == BedrockItem::LINGERING_POTION.id
}

/// Tests whether the native arrow variant appears in the effect-arrow catalogue.
#[must_use]
pub const fn is_tipped_arrow(id: i16, aux: u32) -> bool {
    id == BedrockItem::ARROW.id && aux >= FIRST_TIPPED_ARROW_AUX && aux <= POTIONS.len() as u32
}

/// Returns the Bedrock auxiliary value for a standard shared potion.
#[must_use]
pub fn potion_aux(potion_id: i32) -> Option<u32> {
    POTIONS
        .iter()
        .position(|potion| potion.is_some_and(|potion| i32::from(potion.id) == potion_id))
        .map(|aux| aux as u32)
}

/// Encodes a shared potion or tipped arrow in the Bedrock item auxiliary field.
#[must_use]
pub fn potion_aux_for_contents(id: i16, contents: &PotionContentsImpl) -> Option<u32> {
    let arrow = id == BedrockItem::ARROW.id;
    if !arrow && !uses_potion_aux(id) {
        return None;
    }
    let potion = if let Some(potion_id) = contents.potion_id {
        potion_aux(potion_id)?
    } else if contents.custom_color.is_none()
        && contents.custom_name.is_none()
        && contents.custom_effects == [DECAY_EFFECT]
    {
        DECAY_AUX
    } else {
        return None;
    };
    let aux = potion + u32::from(arrow);
    (!arrow || is_tipped_arrow(id, aux)).then_some(aux)
}

/// Restores potion contents from a Bedrock item variant.
#[must_use]
pub fn potion_contents_from_bedrock(id: i16, aux: u32) -> Option<PotionContentsImpl> {
    let arrow = is_tipped_arrow(id, aux);
    let aux = if arrow {
        // Bedrock's plain arrow occupies the zero auxiliary value.
        aux - 1
    } else if uses_potion_aux(id) {
        aux
    } else {
        return None;
    };
    if aux == DECAY_AUX {
        return Some(PotionContentsImpl {
            potion_id: None,
            custom_color: None,
            custom_effects: vec![DECAY_EFFECT],
            custom_name: None,
        });
    }
    let potion = (*POTIONS.get(aux as usize)?)?;
    Some(PotionContentsImpl {
        potion_id: Some(i32::from(potion.id)),
        custom_color: None,
        custom_effects: Vec::new(),
        custom_name: None,
    })
}

// Bedrock enchantment IDs, cross-checked with Dragonfly and Geyser.
const ENCHANTMENTS: [&Enchantment; 42] = [
    &Enchantment::PROTECTION,
    &Enchantment::FIRE_PROTECTION,
    &Enchantment::FEATHER_FALLING,
    &Enchantment::BLAST_PROTECTION,
    &Enchantment::PROJECTILE_PROTECTION,
    &Enchantment::THORNS,
    &Enchantment::RESPIRATION,
    &Enchantment::DEPTH_STRIDER,
    &Enchantment::AQUA_AFFINITY,
    &Enchantment::SHARPNESS,
    &Enchantment::SMITE,
    &Enchantment::BANE_OF_ARTHROPODS,
    &Enchantment::KNOCKBACK,
    &Enchantment::FIRE_ASPECT,
    &Enchantment::LOOTING,
    &Enchantment::EFFICIENCY,
    &Enchantment::SILK_TOUCH,
    &Enchantment::UNBREAKING,
    &Enchantment::FORTUNE,
    &Enchantment::POWER,
    &Enchantment::PUNCH,
    &Enchantment::FLAME,
    &Enchantment::INFINITY,
    &Enchantment::LUCK_OF_THE_SEA,
    &Enchantment::LURE,
    &Enchantment::FROST_WALKER,
    &Enchantment::MENDING,
    &Enchantment::BINDING_CURSE,
    &Enchantment::VANISHING_CURSE,
    &Enchantment::IMPALING,
    &Enchantment::RIPTIDE,
    &Enchantment::LOYALTY,
    &Enchantment::CHANNELING,
    &Enchantment::MULTISHOT,
    &Enchantment::PIERCING,
    &Enchantment::QUICK_CHARGE,
    &Enchantment::SOUL_SPEED,
    &Enchantment::SWIFT_SNEAK,
    &Enchantment::WIND_BURST,
    &Enchantment::DENSITY,
    &Enchantment::BREACH,
    &Enchantment::LUNGE,
];

#[must_use]
pub fn enchantment_by_id(id: i16) -> Option<&'static Enchantment> {
    ENCHANTMENTS.get(usize::try_from(id).ok()?).copied()
}

#[must_use]
pub fn enchantment_tag(enchantment: &Enchantment, level: i32) -> Option<NbtTag> {
    let id = ENCHANTMENTS
        .iter()
        .position(|shared| shared.registry_key == enchantment.registry_key)?;
    let level = i16::try_from(level).ok().filter(|&level| level > 0)?;
    let mut entry = NbtCompound::new();
    entry.put_short("id", id as i16);
    entry.put_short("lvl", level);
    Some(NbtTag::Compound(entry))
}
