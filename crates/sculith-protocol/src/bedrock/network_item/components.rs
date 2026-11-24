use pumpkin_data::{
    data_component_impl::{EnchantmentsImpl, PotionContentsImpl, StoredEnchantmentsImpl},
    item::Item,
    item_stack::{ItemStack, bedrock},
};
use pumpkin_nbt::{Nbt, NbtCompound};

pub use bedrock::potion_contents_from_bedrock;

pub(super) fn potion_aux(stack: &ItemStack, bedrock_id: i16) -> Option<u32> {
    if !bedrock::uses_potion_aux(bedrock_id) {
        return None;
    }
    let id = stack
        .get_data_component::<PotionContentsImpl>()?
        .potion_id?;
    bedrock::potion_aux(id)
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
        .filter_map(|(enchantment, level)| bedrock::enchantment_tag(enchantment, *level))
        .collect();
    let mut data = NbtCompound::new();
    if !entries.is_empty() {
        data.put_list("ench", entries);
    }
    data.into()
}
