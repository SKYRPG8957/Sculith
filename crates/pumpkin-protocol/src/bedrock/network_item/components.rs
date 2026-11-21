use pumpkin_data::{
    data_component_impl::{EnchantmentsImpl, PotionContentsImpl, StoredEnchantmentsImpl},
    item::{BedrockItem, Item},
    item_stack::ItemStack,
    potion::Potion,
};
use pumpkin_nbt::{Nbt, NbtCompound, tag::NbtTag};

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

fn uses_potion_aux(id: i16) -> bool {
    id == BedrockItem::POTION.id
        || id == BedrockItem::SPLASH_POTION.id
        || id == BedrockItem::LINGERING_POTION.id
}

pub(super) fn potion_aux(stack: &ItemStack, bedrock_id: i16) -> Option<u32> {
    if !uses_potion_aux(bedrock_id) {
        return None;
    }
    let id = stack
        .get_data_component::<PotionContentsImpl>()?
        .potion_id?;
    POTIONS
        .iter()
        .position(|potion| potion.is_some_and(|potion| i32::from(potion.id) == id))
        .map(|aux| aux as u32)
}

/// Restores shared potion contents from a server-provided Bedrock creative entry.
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
fn enchantment_id(name: &str) -> Option<i16> {
    Some(match name {
        "protection" => 0,
        "fire_protection" => 1,
        "feather_falling" => 2,
        "blast_protection" => 3,
        "projectile_protection" => 4,
        "thorns" => 5,
        "respiration" => 6,
        "depth_strider" => 7,
        "aqua_affinity" => 8,
        "sharpness" => 9,
        "smite" => 10,
        "bane_of_arthropods" => 11,
        "knockback" => 12,
        "fire_aspect" => 13,
        "looting" => 14,
        "efficiency" => 15,
        "silk_touch" => 16,
        "unbreaking" => 17,
        "fortune" => 18,
        "power" => 19,
        "punch" => 20,
        "flame" => 21,
        "infinity" => 22,
        "luck_of_the_sea" => 23,
        "lure" => 24,
        "frost_walker" => 25,
        "mending" => 26,
        "binding_curse" => 27,
        "vanishing_curse" => 28,
        "impaling" => 29,
        "riptide" => 30,
        "loyalty" => 31,
        "channeling" => 32,
        "multishot" => 33,
        "piercing" => 34,
        "quick_charge" => 35,
        "soul_speed" => 36,
        "swift_sneak" => 37,
        "wind_burst" => 38,
        "density" => 39,
        "breach" => 40,
        "lunge" => 41,
        _ => return None,
    })
}

pub(super) fn enchantment_nbt(stack: &ItemStack) -> Nbt {
    let enchantments = if stack.item.id == Item::ENCHANTED_BOOK.id {
        stack
            .get_data_component::<StoredEnchantmentsImpl>()
            .map(|data| data.enchantment.as_ref())
    } else {
        stack
            .get_data_component::<EnchantmentsImpl>()
            .map(|data| data.enchantment.as_ref())
    };
    let Some(enchantments) = enchantments.filter(|entries| !entries.is_empty()) else {
        return Nbt::default();
    };
    let entries: Vec<_> = enchantments
        .iter()
        .filter_map(|(enchantment, level)| {
            let id = enchantment_id(enchantment.registry_key)?;
            let level = i16::try_from(*level).ok().filter(|level| *level > 0)?;
            let mut entry = NbtCompound::new();
            entry.put_short("id", id);
            entry.put_short("lvl", level);
            Some(NbtTag::Compound(entry))
        })
        .collect();
    let mut data = NbtCompound::new();
    if !entries.is_empty() {
        data.put_list("ench", entries);
    }
    data.into()
}
