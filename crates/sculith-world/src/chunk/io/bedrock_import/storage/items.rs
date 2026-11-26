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
        DataComponentImpl, EnchantmentsImpl, MAX_ENCHANTMENT_LEVEL, PotionContentsImpl,
        StoredEnchantmentsImpl,
    },
    item::{BedrockItem, Item, JavaToBedrockItemMapping},
    item_stack::{ItemStack, bedrock},
};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};

use super::super::{Error, invalid, terrain::TerrainRegistry};

const VERSION_FILE: &str = ".sculith-bedrock-storage-version";
const CURRENT_VERSION: &[u8] = b"6\n";

#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum StorageVersion {
    Plain,
    PotionAndEnchantment,
    DrinkableDecay,
    TippedArrows,
    ThrownDecay,
    NamedContainers,
}

pub(in crate::chunk::io::bedrock_import) struct StorageItemRegistry {
    by_name: HashMap<&'static str, i16>,
    version: StorageVersion,
}

impl StorageItemRegistry {
    pub(in crate::chunk::io::bedrock_import) fn supports_named_containers(&self) -> bool {
        self.version >= StorageVersion::NamedContainers
    }

    fn supports_components(&self) -> bool {
        self.version >= StorageVersion::PotionAndEnchantment
    }

    fn potion_aux(&self, id: i16, contents: &PotionContentsImpl) -> Option<u32> {
        let decay_version =
            if id == BedrockItem::SPLASH_POTION.id || id == BedrockItem::LINGERING_POTION.id {
                StorageVersion::ThrownDecay
            } else {
                StorageVersion::DrinkableDecay
            };
        if (id == BedrockItem::ARROW.id && self.version < StorageVersion::TippedArrows)
            || (contents.potion_id.is_none() && self.version < decay_version)
        {
            return None;
        }
        bedrock::potion_aux_for_contents(id, contents)
    }
}

pub(in crate::chunk::io::bedrock_import) fn item_registry() -> StorageItemRegistry {
    StorageItemRegistry {
        by_name: BedrockItem::ALL_BEDROCK_ITEMS
            .iter()
            .map(|item| (item.registry_key, item.id))
            .collect(),
        version: StorageVersion::NamedContainers,
    }
}

pub(in crate::chunk::io::bedrock_import) fn write_item_version(world: &Path) -> io::Result<()> {
    fs::write(world.join(VERSION_FILE), CURRENT_VERSION)
}

pub(in crate::chunk::io::bedrock_import) fn read_import_registry(
    world: &Path,
) -> Result<StorageItemRegistry, Error> {
    let mut registry = item_registry();
    registry.version = match File::open(world.join(VERSION_FILE)) {
        Ok(file) => {
            let mut version = Vec::new();
            file.take(16).read_to_end(&mut version)?;
            match version.as_slice() {
                CURRENT_VERSION => StorageVersion::NamedContainers,
                b"5\n" => StorageVersion::ThrownDecay,
                b"4\n" => StorageVersion::TippedArrows,
                b"3\n" => StorageVersion::DrinkableDecay,
                b"2\n" => StorageVersion::PotionAndEnchantment,
                _ => return Err(invalid("unknown Bedrock storage import version")),
            }
        }
        // A legacy import may have replaced a previously skipped chest with an empty one.
        Err(error) if error.kind() == io::ErrorKind::NotFound => StorageVersion::Plain,
        Err(error) => return Err(error.into()),
    };
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
            "Damage" => !matches!(tag, NbtTag::Short(value) if *value >= 0 && (*value == 0 || items.supports_components())),
            "tag" => !matches!(tag, NbtTag::Compound(data) if data.is_empty() || items.supports_components()),
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
    let damage = u32::try_from(source.get_short("Damage").unwrap_or(0))
        .map_err(|_| "invalid item Damage value")?;
    let item =
        if items.version >= StorageVersion::TippedArrows && bedrock::is_tipped_arrow(id, damage) {
            &Item::TIPPED_ARROW
        } else {
            JavaToBedrockItemMapping::from_bedrock(id, 0)
                .map(|mapping| mapping.java_item)
                .filter(|item| *item != &Item::AIR)
                .ok_or_else(|| format!("item {name:?} has no plain mapping"))?
        };
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
    let potion = item
        .components
        .iter()
        .any(|(component, _)| *component == DataComponent::PotionContents);
    if potion {
        if !items.supports_components() {
            return Err(format!("item {name:?} requires potion data conversion"));
        }
        let contents = bedrock::potion_contents_from_bedrock(id, damage)
            .filter(|contents| items.potion_aux(id, contents) == Some(damage))
            .ok_or_else(|| format!("potion variant {damage} has no reversible mapping"))?;
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
            "components" if matches!(value, NbtTag::Compound(tag) if items.supports_components() || tag.is_empty()) => None,
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
        read_sculith_components(&mut stack, components, items)?;
    }
    Ok(stack)
}

fn read_sculith_components(
    stack: &mut ItemStack,
    components: &NbtCompound,
    items: &StorageItemRegistry,
) -> Result<(), String> {
    for (component, value) in &components.child_tags {
        let data = value
            .extract_compound()
            .ok_or("invalid item component data")?;
        match component.as_ref() {
            "minecraft:potion_contents" => {
                let id = JavaToBedrockItemMapping::from_java_item_id(stack.item.id)
                    .map(|mapping| mapping.bedrock_item.id)
                    .ok_or("unmapped potion item")?;
                let potion = PotionContentsImpl::read_data(value)
                    .filter(|potion| {
                        (bedrock::uses_potion_aux(id) || stack.item == &Item::TIPPED_ARROW)
                            && items.potion_aux(id, potion).is_some()
                    })
                    .ok_or("unmapped potion contents")?;
                let reversible = if potion.potion_id.is_some() {
                    data.child_tags.len() == 1 && data.get("potion").is_some()
                } else {
                    // The permissive shared decoder can normalize away unknown fields.
                    potion.write_data() == *value
                };
                if !reversible {
                    return Err("unsupported potion contents".into());
                }
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
    items: &StorageItemRegistry,
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
    let damage =
        if bedrock::uses_potion_aux(mapping.bedrock_item.id) || stack.item == &Item::TIPPED_ARROW {
            stack
                .get_data_component::<PotionContentsImpl>()
                .and_then(|potion| items.potion_aux(mapping.bedrock_item.id, potion))
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
