//! Edition mappings for Bedrock item variants and enchantments.

use crate::{
    Enchantment, data_component_impl::PotionContentsImpl, item::BedrockItem, potion::Potion,
};
use pumpkin_nbt::{NbtCompound, tag::NbtTag};

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
    None, // Wither.
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

/// Returns the Bedrock auxiliary value for a standard shared potion.
#[must_use]
pub fn potion_aux(potion_id: i32) -> Option<u32> {
    POTIONS
        .iter()
        .position(|potion| potion.is_some_and(|potion| i32::from(potion.id) == potion_id))
        .map(|aux| aux as u32)
}

/// Restores shared potion contents from a Bedrock item variant.
#[must_use]
pub fn potion_contents_from_bedrock(id: i16, aux: u32) -> Option<PotionContentsImpl> {
    if !uses_potion_aux(id) {
        return None;
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
