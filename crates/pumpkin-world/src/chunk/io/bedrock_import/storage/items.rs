use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

use pumpkin_data::{
    Block, Enchantment,
    data_component::DataComponent,
    data_component_impl::{
        EnchantmentsImpl, MAX_ENCHANTMENT_LEVEL, PotionContentsImpl, StoredEnchantmentsImpl,
    },
    item::{BedrockItem, Item, JavaToBedrockItemMapping},
    item_stack::{ItemStack, bedrock},
};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};

use super::super::{Error, invalid, terrain::TerrainRegistry};

const VERSION_FILE: &str = ".sculith-bedrock-storage-version";
const CURRENT_VERSION: &[u8] = b"2\n";

pub(in crate::chunk::io::bedrock_import) struct StorageItemRegistry {
    by_name: HashMap<&'static str, i16>,
    components: bool,
}

pub(in crate::chunk::io::bedrock_import) fn item_registry() -> StorageItemRegistry {
    StorageItemRegistry {
        by_name: BedrockItem::ALL_BEDROCK_ITEMS
            .iter()
            .map(|item| (item.registry_key, item.id))
            .collect(),
        components: true,
    }
}

pub(in crate::chunk::io::bedrock_import) fn write_item_version(world: &Path) -> io::Result<()> {
    fs::write(world.join(VERSION_FILE), CURRENT_VERSION)
}

pub(in crate::chunk::io::bedrock_import) fn read_import_registry(
    world: &Path,
) -> Result<StorageItemRegistry, Error> {
    let mut registry = item_registry();
    match File::open(world.join(VERSION_FILE)) {
        Ok(file) => {
            let mut version = Vec::new();
            file.take(16).read_to_end(&mut version)?;
            if version != CURRENT_VERSION {
                return Err(invalid("unknown Bedrock storage import version"));
            }
        }
        // A legacy import may have replaced a previously skipped chest with an empty one.
        Err(error) if error.kind() == io::ErrorKind::NotFound => registry.components = false,
        Err(error) => return Err(error.into()),
    }
    Ok(registry)
}

pub(in crate::chunk::io::bedrock_import) fn convert_stack(
    source: &NbtCompound,
    terrain: &TerrainRegistry,
    items: &StorageItemRegistry,
) -> Result<ItemStack, String> {
    if let Some(field) = source
        .child_tags
        .iter()
        .filter(|(field, tag)| match field.as_ref() {
            "Slot" | "Name" | "Count" | "WasPickedUp" | "Block" => false,
            "Damage" => !matches!(tag, NbtTag::Short(value) if *value >= 0 && (*value == 0 || items.components)),
            "tag" => !matches!(tag, NbtTag::Compound(data) if data.is_empty() || items.components),
            _ => true,
        })
        .map(|(field, _)| field)
        .min()
    {
        return Err(format!("unsupported item field {field:?}"));
    }
    let name = source.get_string("Name").ok_or("missing item name")?;
    let id = *items
        .by_name
        .get(name)
        .ok_or_else(|| format!("item {name:?} has no plain mapping"))?;
    let item = JavaToBedrockItemMapping::from_bedrock(id, 0)
        .map(|mapping| mapping.java_item)
        .filter(|item| *item != &Item::AIR)
        .ok_or_else(|| format!("item {name:?} has no plain mapping"))?;
    if let Some(block) = source.get("Block") {
        let block = block.extract_compound().ok_or("invalid block item data")?;
        let state = terrain
            .block(block)
            .map_err(|error| format!("unsupported block item state: {error}"))?;
        let block = Block::from_state_id(state);
        if block.name != item.registry_key || state != block.default_state.id {
            return Err("only default states for block items are supported".into());
        }
    }
    let count = source
        .get_byte("Count")
        .and_then(|count| u8::try_from(count).ok())
        .filter(|&count| count > 0)
        .ok_or("invalid item count")?;
    let mut stack = ItemStack::new(count, item);
    if count > stack.get_max_stack_size() {
        return Err(format!("item count {count} exceeds the stack limit"));
    }
    let damage = source.get_short("Damage").unwrap_or(0);
    let potion = item
        .components
        .iter()
        .any(|(component, _)| *component == DataComponent::PotionContents);
    if potion {
        if !items.components {
            return Err(format!("item {name:?} requires potion data conversion"));
        }
        let aux = u32::try_from(damage).map_err(|_| "invalid potion variant")?;
        let contents = bedrock::potion_contents_from_bedrock(id, aux)
            .filter(|contents| contents.potion_id.and_then(bedrock::potion_aux) == Some(aux))
            .ok_or_else(|| format!("potion variant {aux} has no reversible mapping"))?;
        stack.set_data_component(contents);
    } else if damage != 0 {
        return Err("unsupported item Damage value".into());
    }
    if let Some(tag) = source.get_compound("tag").filter(|tag| !tag.is_empty()) {
        let entries = tag
            .get_list("ench")
            .filter(|_| tag.child_tags.len() == 1)
            .ok_or("unsupported Bedrock item tag")?;
        set_enchantments(&mut stack, read_bedrock_enchantments(entries)?)?;
    }
    Ok(stack)
}

type EnchantmentLevel = (&'static Enchantment, i32);

fn read_bedrock_enchantments(
    entries: &[NbtTag],
) -> Result<Cow<'static, [EnchantmentLevel]>, String> {
    let mut enchantments: Vec<EnchantmentLevel> = Vec::new();
    for entry in entries {
        let data = entry
            .extract_compound()
            .filter(|data| data.child_tags.len() == 2)
            .ok_or("invalid Bedrock enchantment entry")?;
        let enchantment = data
            .get_short("id")
            .and_then(bedrock::enchantment_by_id)
            .ok_or("unmapped Bedrock enchantment")?;
        let level = data
            .get_short("lvl")
            .map(i32::from)
            .filter(|level| (1..=MAX_ENCHANTMENT_LEVEL).contains(level))
            .ok_or("invalid Bedrock enchantment level")?;
        if enchantments
            .iter()
            .any(|(other, _)| other.id == enchantment.id)
        {
            return Err("duplicate Bedrock enchantment".into());
        }
        enchantments.push((enchantment, level));
    }
    Ok(Cow::Owned(enchantments))
}

fn set_enchantments(
    stack: &mut ItemStack,
    mut enchantment: Cow<'static, [EnchantmentLevel]>,
) -> Result<(), String> {
    if enchantment.is_empty() {
        return Ok(());
    }
    // Java NBT stores a map and Bedrock stores a list. Compare their meanings.
    enchantment
        .to_mut()
        .sort_unstable_by_key(|(enchantment, _)| enchantment.id);
    if enchantment
        .windows(2)
        .any(|pair| pair[0].0.id == pair[1].0.id)
    {
        return Err("duplicate item enchantment".into());
    }
    if stack.item == &Item::ENCHANTED_BOOK {
        stack.set_data_component(StoredEnchantmentsImpl { enchantment });
    } else {
        stack.set_data_component(EnchantmentsImpl { enchantment });
    }
    Ok(())
}

pub(in crate::chunk::io::bedrock_import) fn read_sculith_stack(
    entry: &NbtCompound,
    items: &StorageItemRegistry,
) -> Result<ItemStack, String> {
    if let Some(field) = entry
        .child_tags
        .iter()
        .filter_map(|(field, value)| match field.as_ref() {
            "id" | "count" | "Slot" => None,
            "components" if matches!(value, NbtTag::Compound(tag) if items.components || tag.is_empty()) => None,
            _ => Some(field),
        })
        .min()
    {
        return Err(format!("unsupported item field {field:?}"));
    }
    let item = entry
        .get_string("id")
        .and_then(|id| id.strip_prefix("minecraft:"))
        .and_then(Item::from_registry_key)
        .filter(|item| *item != &Item::AIR)
        .ok_or("invalid Sculith item identifier")?;
    let count = entry
        .get_int("count")
        .and_then(|count| i8::try_from(count).ok())
        .filter(|&count| count > 0)
        .ok_or("invalid Sculith item count")? as u8;
    let mut stack = ItemStack::new(count, item);
    if let Some(components) = entry.get_compound("components") {
        read_sculith_components(&mut stack, components)?;
    }
    Ok(stack)
}

fn read_sculith_components(stack: &mut ItemStack, components: &NbtCompound) -> Result<(), String> {
    for (component, value) in &components.child_tags {
        let data = value
            .extract_compound()
            .ok_or("invalid item component data")?;
        match component.as_ref() {
            "minecraft:potion_contents" => {
                let has_mapping = JavaToBedrockItemMapping::from_java_item_id(stack.item.id)
                    .is_some_and(|mapping| bedrock::uses_potion_aux(mapping.bedrock_item.id));
                if !has_mapping || data.child_tags.len() != 1 || data.get("potion").is_none() {
                    return Err("unsupported potion contents".into());
                }
                let potion = PotionContentsImpl::read_data(value)
                    .filter(|potion| potion.potion_id.and_then(bedrock::potion_aux).is_some())
                    .ok_or("unmapped potion contents")?;
                stack.set_data_component(potion);
            }
            "minecraft:enchantments" | "minecraft:stored_enchantments" => {
                let stored = component.as_ref() == "minecraft:stored_enchantments";
                if stored != (stack.item == &Item::ENCHANTED_BOOK) {
                    return Err(format!("unsupported item component {component:?}"));
                }
                let enchantment = read_sculith_enchantments(value, data)?;
                set_enchantments(stack, enchantment)?;
            }
            _ => return Err(format!("unsupported item component {component:?}")),
        }
    }
    Ok(())
}

fn read_sculith_enchantments(
    value: &NbtTag,
    data: &NbtCompound,
) -> Result<Cow<'static, [EnchantmentLevel]>, String> {
    if data.get("levels").is_some() && data.child_tags.len() != 1 {
        return Err("unsupported item enchantment fields".into());
    }
    EnchantmentsImpl::read_data(value)
        .map(|data| data.enchantment)
        .ok_or_else(|| "invalid item enchantments".into())
}

pub(in crate::chunk::io::bedrock_import) fn new_bedrock_stack(
    stack: &ItemStack,
) -> Result<NbtCompound, String> {
    let mapping = JavaToBedrockItemMapping::from_java_item_id(stack.item.id)
        .filter(|mapping| mapping.bedrock_data == 0)
        .ok_or_else(|| {
            format!(
                "item {:?} has no plain Bedrock mapping",
                stack.item.registry_key
            )
        })?;
    let mut entry = NbtCompound::new();
    entry.put_string("Name", mapping.bedrock_item.registry_key.to_owned());
    let damage = if bedrock::uses_potion_aux(mapping.bedrock_item.id) {
        stack
            .get_data_component::<PotionContentsImpl>()
            .and_then(|potion| potion.potion_id)
            .and_then(bedrock::potion_aux)
            .and_then(|aux| i16::try_from(aux).ok())
            .ok_or("missing mapped potion contents")?
    } else {
        0
    };
    entry.put_short("Damage", damage);
    let enchantments = if stack.item == &Item::ENCHANTED_BOOK {
        stack
            .get_data_component::<StoredEnchantmentsImpl>()
            .map(|data| data.enchantment.as_ref())
    } else {
        stack
            .get_data_component::<EnchantmentsImpl>()
            .map(|data| data.enchantment.as_ref())
    };
    if let Some(enchantments) = enchantments.filter(|entries| !entries.is_empty()) {
        let encoded = enchantments
            .iter()
            .map(|(enchantment, level)| bedrock::enchantment_tag(enchantment, *level))
            .collect::<Option<Vec<_>>>()
            .ok_or("unmapped Bedrock enchantment")?;
        let mut tag = NbtCompound::new();
        tag.put_list("ench", encoded);
        entry.put_compound("tag", tag);
    }
    Ok(entry)
}
