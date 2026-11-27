use pumpkin_data::{
    data_component_impl::{EnchantmentsImpl, PotionContentsImpl, StoredEnchantmentsImpl},
    item::Item,
    item_stack::{ItemStack, bedrock},
};
use pumpkin_nbt::{Nbt, NbtCompound};

pub use bedrock::potion_contents_from_bedrock;

pub(super) fn potion_aux(stack: &ItemStack, bedrock_id: i16) -> Option<u32> {
    if !bedrock::uses_potion_aux(bedrock_id) && stack.item != &Item::TIPPED_ARROW {
        return None;
    }
    bedrock::potion_aux_for_contents(
        bedrock_id,
        stack.get_data_component::<PotionContentsImpl>()?,
    )
}

pub(super) fn item_nbt(stack: &ItemStack) -> Nbt {
    let mut data = NbtCompound::new();
    let damage = stack.get_damage();
    if damage > 0 && stack.is_damageable() {
        // Bedrock uses a network NBT int for tool wear. Aux selects item variants.
        data.put_int("Damage", damage);
    }
    let enchantments = if stack.item.id == Item::ENCHANTED_BOOK.id {
        stack
            .get_data_component::<StoredEnchantmentsImpl>()
            .map(|data| data.enchantment.as_ref())
    } else {
        stack
            .get_data_component::<EnchantmentsImpl>()
            .map(|data| data.enchantment.as_ref())
    };
    if let Some(enchantments) = enchantments {
        let entries: Vec<_> = enchantments
            .iter()
            .filter_map(|(enchantment, level)| bedrock::enchantment_tag(enchantment, *level))
            .collect();
        if !entries.is_empty() {
            data.put_list("ench", entries);
        }
    }
    data.into()
}
