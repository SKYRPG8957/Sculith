use std::io::{Error, ErrorKind, Read, Write};
use std::num::NonZero;

use pumpkin_data::item::{BedrockItem, JavaToBedrockItemMapping};
use pumpkin_data::item_stack::ItemStack;
use pumpkin_nbt::{
    Nbt,
    deserializer::{NbtReadHelperBedrock, NbtReadHelperBedrockDisk},
};

use crate::{
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};

mod components;

pub use components::potion_contents_from_bedrock;

// Empty NBT, followed by two empty block filter lists.
const EMPTY_USER_DATA_LEN: usize = size_of::<i16>() + 2 * size_of::<i32>();

/// Describes a network item stack transmitted over the wire in Bedrock packets.
///
/// Contains the item runtime ID, count, aux/metadata value, block runtime state,
/// and variable user data including NBT, can-place, and can-destroy block filters.
#[derive(Default, Clone, Debug)]
pub struct NetworkItemDescriptor {
    /// Runtime item identifier.
    pub id: VarInt,
    /// Stack item count.
    pub stack_size: u16,
    /// Auxiliary metadata or damage value.
    pub aux_value: VarUInt,
    /// Block runtime ID associated with the item (for block items).
    pub block_runtime_id: VarInt,

    // remainder is expansion of `User Data Buffer` (ItemInstanceUserData)
    /// NBT component data (display name, enchantments, etc.).
    pub nbt_data: Nbt,
    /// Block identifiers this item can be placed against in Adventure mode.
    pub place_on_blocks: Vec<String>,
    /// Block identifiers this tool can destroy in Adventure mode.
    pub destroy_blocks: Vec<String>,

    /// Shield blocking tick counter used for shield cooldown sync.
    pub shield_blocking_tick: i64,
}

impl PacketWrite for NetworkItemDescriptor {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        (self.id.0 as i16).write(writer)?;
        self.write_stack_data(writer, None)
    }
}

impl PacketRead for NetworkItemDescriptor {
    fn read<R: Read>(buf: &mut R) -> Result<Self, Error> {
        let id = VarInt(i32::from(i16::read(buf)?));
        let stack_size = u16::read(buf)?;
        let aux_value = VarUInt::read(buf)?;

        let has_net_id = bool::read(buf)?;
        if has_net_id {
            let _net_id = VarInt::read(buf)?;
        }

        let block_runtime_id = VarInt(VarUInt::read(buf)?.0 as i32);

        let user_data_len = VarUInt::read(buf)?.0 as usize;
        if user_data_len > 1_048_576 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "user_data_len exceeds 1MB limit",
            ));
        }
        let mut user_data = vec![0u8; user_data_len];
        buf.read_exact(&mut user_data)?;

        let (nbt_data, place_on_blocks, destroy_blocks, shield_blocking_tick) =
            read_user_data(&user_data, id.0 == i32::from(BedrockItem::SHIELD.id))?;

        Ok(Self {
            id,
            stack_size,
            aux_value,
            block_runtime_id,
            nbt_data,
            place_on_blocks,
            destroy_blocks,
            shield_blocking_tick,
        })
    }
}

impl NetworkItemDescriptor {
    fn write_stack_data<W: Write>(
        &self,
        writer: &mut W,
        net_id: Option<VarInt>,
    ) -> Result<(), Error> {
        self.stack_size.write(writer)?;
        self.aux_value.write(writer)?;
        net_id.is_some().write(writer)?;
        if let Some(net_id) = net_id {
            net_id.write(writer)?;
        }
        VarUInt(self.block_runtime_id.0 as u32).write(writer)?;
        self.write_user_data(writer)?;
        Ok(())
    }

    /// Writes the 1.26.40 `NetworkItemInstanceDescriptor` used by creative and
    /// crafting packets. Regular item fields use the stack descriptor instead.
    pub fn write_item_instance<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.id.write(writer)?;
        self.stack_size.write(writer)?;
        self.aux_value.write(writer)?;
        self.block_runtime_id.write(writer)?;
        self.write_user_data(writer)
    }

    /// Reads the item descriptor used in creative inventory and crafting recipes.
    pub fn read_item_instance<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let id = VarInt::read(reader)?;
        let stack_size = u16::read(reader)?;
        let aux_value = VarUInt::read(reader)?;
        let block_runtime_id = VarInt::read(reader)?;
        let len = VarUInt::read(reader)?.0 as usize;
        if len > 1_048_576 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "item user data exceeds 1MB",
            ));
        }
        let mut user_data = vec![0; len];
        reader.read_exact(&mut user_data)?;
        let (nbt_data, place_on_blocks, destroy_blocks, shield_blocking_tick) =
            read_user_data(&user_data, id.0 == i32::from(BedrockItem::SHIELD.id))?;
        Ok(Self {
            id,
            stack_size,
            aux_value,
            block_runtime_id,
            nbt_data,
            place_on_blocks,
            destroy_blocks,
            shield_blocking_tick,
        })
    }

    fn write_user_data<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        if self.id.0 == 0 {
            return VarUInt(0).write(writer);
        }

        let mut buf = Vec::with_capacity(EMPTY_USER_DATA_LEN);
        self.write_user_data_buffer(&mut buf)?;
        VarUInt(buf.len() as u32).write(writer)?;
        writer.write_all(&buf)
    }

    fn write_user_data_buffer<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        if self.nbt_data.is_empty() {
            (0i16).write(writer)?;
        } else {
            (-1i16).write(writer)?;
            (1i8).write(writer)?;
            self.nbt_data
                .clone()
                .write_to_writer_bedrock_disk(&mut *writer)
                .map_err(Error::other)?;
        }
        write_user_data_strings(writer, &self.place_on_blocks)?;
        write_user_data_strings(writer, &self.destroy_blocks)?;
        if self.id.0 == i32::from(BedrockItem::SHIELD.id) {
            self.shield_blocking_tick.write(writer)?;
        }
        Ok(())
    }
}

impl From<&ItemStack> for NetworkItemDescriptor {
    fn from(stack: &ItemStack) -> Self {
        if stack.is_empty() {
            Self::default()
        } else {
            JavaToBedrockItemMapping::from_java_item_id(stack.get_item().id).map_or(
                Self::default(),
                |mapping| Self {
                    id: VarInt::from(mapping.bedrock_item.id),
                    stack_size: stack.item_count as u16,
                    aux_value: VarUInt(
                        components::potion_aux(stack, mapping.bedrock_item.id)
                            .unwrap_or(mapping.bedrock_data),
                    ),
                    block_runtime_id: VarInt::from(mapping.bedrock_block_state),
                    nbt_data: components::enchantment_nbt(stack),
                    ..Self::default()
                },
            )
        }
    }
}

/// Wrapped network item stack including its network ID for slot tracking.
#[derive(Default, Clone, Debug)]
pub struct ItemStackWrapper {
    /// Item runtime ID.
    pub id: i16,
    /// Stack count.
    pub stack_size: u16,
    /// Auxiliary metadata or damage value.
    pub aux_value: VarUInt,
    /// Runtime block state identifier.
    pub block_runtime_id: VarInt,
    /// Item NBT data.
    pub nbt_data: Nbt,
    /// Allowed block placement filter.
    pub place_on_blocks: Vec<String>,
    /// Allowed block destruction filter.
    pub destroy_blocks: Vec<String>,
    /// Shield blocking tick counter.
    pub shield_blocking_tick: i64,
    /// Server-assigned unique network stack ID, if present.
    pub net_id: Option<NonZero<i32>>,
}

impl PacketWrite for ItemStackWrapper {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.id.write(writer)?;
        self.stack_size.write(writer)?;
        self.aux_value.write(writer)?;
        self.net_id.is_some().write(writer)?;
        if let Some(id) = self.net_id {
            VarInt(id.get()).write(writer)?;
        }
        VarUInt(self.block_runtime_id.0 as u32).write(writer)?;

        let descriptor = NetworkItemDescriptor {
            id: VarInt(i32::from(self.id)),
            stack_size: self.stack_size,
            aux_value: self.aux_value,
            block_runtime_id: self.block_runtime_id,
            nbt_data: self.nbt_data.clone(),
            place_on_blocks: self.place_on_blocks.clone(),
            destroy_blocks: self.destroy_blocks.clone(),
            shield_blocking_tick: self.shield_blocking_tick,
        };
        descriptor.write_user_data(writer)
    }
}

impl PacketRead for ItemStackWrapper {
    fn read<R: Read>(buf: &mut R) -> Result<Self, Error> {
        let id = i16::read(buf)?;
        let stack_size = u16::read(buf)?;
        let aux_value = VarUInt::read(buf)?;

        let has_net_id = bool::read(buf)?;
        let net_id = if has_net_id {
            let stack_id = VarInt::read(buf)?;
            NonZero::new(stack_id.0)
        } else {
            None
        };

        let block_runtime_id = VarInt(VarUInt::read(buf)?.0 as i32);

        let user_data_len = VarUInt::read(buf)?.0 as usize;
        if user_data_len > 1_048_576 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "user_data_len exceeds 1MB limit",
            ));
        }
        let mut user_data = vec![0u8; user_data_len];
        buf.read_exact(&mut user_data)?;

        let (nbt_data, place_on_blocks, destroy_blocks, shield_blocking_tick) =
            read_user_data(&user_data, id == BedrockItem::SHIELD.id)?;

        Ok(Self {
            id,
            stack_size,
            aux_value,
            block_runtime_id,
            nbt_data,
            place_on_blocks,
            destroy_blocks,
            shield_blocking_tick,
            net_id,
        })
    }
}

impl From<&ItemStack> for ItemStackWrapper {
    fn from(stack: &ItemStack) -> Self {
        let item = NetworkItemDescriptor::from(stack);
        Self {
            id: item.id.0 as i16,
            stack_size: item.stack_size,
            aux_value: item.aux_value,
            block_runtime_id: item.block_runtime_id,
            nbt_data: item.nbt_data,
            place_on_blocks: item.place_on_blocks,
            destroy_blocks: item.destroy_blocks,
            shield_blocking_tick: item.shield_blocking_tick,
            net_id: (item.id.0 != 0).then_some(stack.uid),
        }
    }
}

/// Compact network item stack descriptor with unparsed raw extra data bytes.
#[derive(Default, Clone, Debug)]
pub struct NetworkItemStackDescriptor {
    /// Item runtime ID.
    pub id: i16,
    /// Stack count.
    pub stack_size: u16,
    /// Auxiliary metadata or damage value.
    pub aux_value: VarUInt,
    /// Runtime block state identifier.
    pub block_runtime_id: VarUInt,
    /// Serialized user data buffer (NBT, filters).
    pub extra_data: Vec<u8>,
    /// Unique network stack ID.
    pub net_id: Option<NonZero<i32>>,
}

impl PacketWrite for NetworkItemStackDescriptor {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.id.write(writer)?;

        self.stack_size.write(writer)?;
        self.aux_value.write(writer)?;

        self.net_id.is_some().write(writer)?;
        if let Some(id) = self.net_id {
            VarInt(id.get()).write(writer)?;
        }

        self.block_runtime_id.write(writer)?;

        VarUInt(self.extra_data.len() as u32).write(writer)?;
        writer.write_all(&self.extra_data)?;

        Ok(())
    }
}

impl PacketRead for NetworkItemStackDescriptor {
    fn read<R: Read>(buf: &mut R) -> Result<Self, Error> {
        let id = i16::read(buf)?;

        let stack_size = u16::read(buf)?;
        let aux_value = VarUInt::read(buf)?;

        let has_net_id = bool::read(buf)?;
        let net_id = if has_net_id {
            let stack_id = VarInt::read(buf)?;
            NonZero::new(stack_id.0)
        } else {
            None
        };

        let block_runtime_id = VarUInt::read(buf)?;

        let extra_data_len = VarUInt::read(buf)?.0 as usize;
        if extra_data_len > 1_048_576 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "extra_data_len exceeds 1MB limit",
            ));
        }
        let mut extra_data = Vec::new();
        if buf
            .take(extra_data_len as u64)
            .read_to_end(&mut extra_data)?
            != extra_data_len
        {
            return Err(ErrorKind::UnexpectedEof.into());
        }

        Ok(Self {
            id,
            stack_size,
            aux_value,
            block_runtime_id,
            extra_data,
            net_id,
        })
    }
}

impl NetworkItemStackDescriptor {
    fn from_descriptor(item: &NetworkItemDescriptor, net_id: NonZero<i32>) -> Self {
        if item.id.0 == 0 {
            return Self::default();
        }
        let mut extra_data = Vec::with_capacity(EMPTY_USER_DATA_LEN);
        if item.write_user_data_buffer(&mut extra_data).is_err() {
            // Keep the inventory slot and discard the partial user data.
            extra_data.clear();
            extra_data.resize(EMPTY_USER_DATA_LEN, 0);
            if item.id.0 == i32::from(BedrockItem::SHIELD.id) {
                extra_data.extend_from_slice(&item.shield_blocking_tick.to_le_bytes());
            }
        }
        Self {
            id: item.id.0 as i16,
            stack_size: item.stack_size,
            aux_value: item.aux_value,
            block_runtime_id: VarUInt(item.block_runtime_id.0 as u32),
            extra_data,
            net_id: Some(net_id),
        }
    }
}

impl From<&ItemStack> for NetworkItemStackDescriptor {
    fn from(stack: &ItemStack) -> Self {
        Self::from_descriptor(&NetworkItemDescriptor::from(stack), stack.uid)
    }
}

/// Container identifier paired with an optional dynamic session ID for complex inventories.
#[derive(PacketWrite, PacketRead, Clone, Debug, PartialEq, Eq)]
pub struct FullContainerName {
    /// Specific container slot category.
    pub container_name: ContainerName,
    /// Dynamic ID assigned to temporary containers (e.g. trading or crafting).
    pub dynamic_id: Option<u32>,
}

/// Identifies the container or inventory screen segment referenced by an item stack request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ContainerName {
    AnvilInput,
    AnvilMaterial,
    AnvilResultPreview,
    SmithingTableInput,
    SmithingTableMaterial,
    SmithingTableResultPreview,
    Armor,
    LevelEntity,
    BeaconPayment,
    BrewingStandInput,
    BrewingStandResult,
    BrewingStandFuel,
    CombinedHotBarAndInventory,
    CraftingInput,
    CraftingOutputPreview,
    RecipeConstruction,
    RecipeNature,
    RecipeItems,
    RecipeSearch,
    RecipeSearchBar,
    RecipeEquipment,
    RecipeBook,
    EnchantingInput,
    EnchantingMaterial,
    FurnaceFuel,
    FurnaceIngredient,
    FurnaceResult,
    HorseEquip,
    HotBar,
    Inventory,
    ShulkerBox,
    TradeIngredient1,
    TradeIngredient2,
    TradeResultPreview,
    Offhand,
    CompoundCreatorInput,
    CompoundCreatorOutputPreview,
    ElementConstructorOutputPreview,
    MaterialReducerInput,
    MaterialReducerOutput,
    LabTableInput,
    LoomInput,
    LoomDye,
    LoomMaterial,
    LoomResultPreview,
    BlastFurnaceIngredient,
    SmokerIngredient,
    Trade2Ingredient1,
    Trade2Ingredient2,
    Trade2ResultPreview,
    GrindstoneInput,
    GrindstoneAdditional,
    GrindstoneResultPreview,
    StonecutterInput,
    StonecutterResultPreview,
    CartographyInput,
    CartographyAdditional,
    CartographyResultPreview,
    Barrel,
    Cursor,
    CreatedOutput,
    SmithingTableTemplate,
    CrafterLevelEntity,
    Dynamic,
    RecipeFood,
    RecipeBlocks,
    RecipeFurnaceItems,
    ContainerReservedA,
    ContainerReservedB,
    ContainerReservedC,
    ContainerReservedD,
}

impl TryFrom<u8> for ContainerName {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::AnvilInput),
            1 => Ok(Self::AnvilMaterial),
            2 => Ok(Self::AnvilResultPreview),
            3 => Ok(Self::SmithingTableInput),
            4 => Ok(Self::SmithingTableMaterial),
            5 => Ok(Self::SmithingTableResultPreview),
            6 => Ok(Self::Armor),
            7 => Ok(Self::LevelEntity),
            8 => Ok(Self::BeaconPayment),
            9 => Ok(Self::BrewingStandInput),
            10 => Ok(Self::BrewingStandResult),
            11 => Ok(Self::BrewingStandFuel),
            12 => Ok(Self::CombinedHotBarAndInventory),
            13 => Ok(Self::CraftingInput),
            14 => Ok(Self::CraftingOutputPreview),
            15 => Ok(Self::RecipeConstruction),
            16 => Ok(Self::RecipeNature),
            17 => Ok(Self::RecipeItems),
            18 => Ok(Self::RecipeSearch),
            19 => Ok(Self::RecipeSearchBar),
            20 => Ok(Self::RecipeEquipment),
            21 => Ok(Self::RecipeBook),
            22 => Ok(Self::EnchantingInput),
            23 => Ok(Self::EnchantingMaterial),
            24 => Ok(Self::FurnaceFuel),
            25 => Ok(Self::FurnaceIngredient),
            26 => Ok(Self::FurnaceResult),
            27 => Ok(Self::HorseEquip),
            28 => Ok(Self::HotBar),
            29 => Ok(Self::Inventory),
            30 => Ok(Self::ShulkerBox),
            31 => Ok(Self::TradeIngredient1),
            32 => Ok(Self::TradeIngredient2),
            33 => Ok(Self::TradeResultPreview),
            34 => Ok(Self::Offhand),
            35 => Ok(Self::CompoundCreatorInput),
            36 => Ok(Self::CompoundCreatorOutputPreview),
            37 => Ok(Self::ElementConstructorOutputPreview),
            38 => Ok(Self::MaterialReducerInput),
            39 => Ok(Self::MaterialReducerOutput),
            40 => Ok(Self::LabTableInput),
            41 => Ok(Self::LoomInput),
            42 => Ok(Self::LoomDye),
            43 => Ok(Self::LoomMaterial),
            44 => Ok(Self::LoomResultPreview),
            45 => Ok(Self::BlastFurnaceIngredient),
            46 => Ok(Self::SmokerIngredient),
            47 => Ok(Self::Trade2Ingredient1),
            48 => Ok(Self::Trade2Ingredient2),
            49 => Ok(Self::Trade2ResultPreview),
            50 => Ok(Self::GrindstoneInput),
            51 => Ok(Self::GrindstoneAdditional),
            52 => Ok(Self::GrindstoneResultPreview),
            53 => Ok(Self::StonecutterInput),
            54 => Ok(Self::StonecutterResultPreview),
            55 => Ok(Self::CartographyInput),
            56 => Ok(Self::CartographyAdditional),
            57 => Ok(Self::CartographyResultPreview),
            58 => Ok(Self::Barrel),
            59 => Ok(Self::Cursor),
            60 => Ok(Self::CreatedOutput),
            61 => Ok(Self::SmithingTableTemplate),
            62 => Ok(Self::CrafterLevelEntity),
            63 => Ok(Self::Dynamic),
            64 => Ok(Self::RecipeFood),
            65 => Ok(Self::RecipeBlocks),
            66 => Ok(Self::RecipeFurnaceItems),
            67 => Ok(Self::ContainerReservedA),
            68 => Ok(Self::ContainerReservedB),
            69 => Ok(Self::ContainerReservedC),
            70 => Ok(Self::ContainerReservedD),
            _ => Err(Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid ContainerName ID: {value}"),
            )),
        }
    }
}

impl PacketWrite for ContainerName {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        (*self as u8).write(writer)?;
        Ok(())
    }
}

impl PacketRead for ContainerName {
    fn read<R: Read>(buf: &mut R) -> Result<Self, Error> {
        let value = u8::read(buf)?;
        Self::try_from(value)
    }
}

/// Compact representation of an item stack in network transactions with raw extra data bytes.
#[derive(Debug, Clone)]
pub struct NetworkItemStack {
    /// Runtime item ID.
    pub id: VarInt,
    /// Stack count.
    pub count: u16,
    /// Auxiliary metadata or damage value.
    pub aux_value: VarUInt,
    /// Runtime block state identifier.
    pub block_runtime_id: VarInt,
    /// Raw unparsed extra user data buffer.
    pub extra_data: Vec<u8>,
}

impl PacketRead for NetworkItemStack {
    fn read<R: Read>(buf: &mut R) -> Result<Self, Error> {
        let id = VarInt::read(buf)?;
        let count = u16::read(buf)?;
        let aux_value = VarUInt::read(buf)?;
        let block_runtime_id = VarInt::read(buf)?;

        let extra_data_len = VarUInt::read(buf)?.0 as usize;
        if extra_data_len > 1_048_576 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "extra_data_len exceeds 1MB limit",
            ));
        }
        let mut extra_data = vec![0u8; extra_data_len];
        buf.read_exact(&mut extra_data)?;

        Ok(Self {
            id,
            count,
            aux_value,
            block_runtime_id,
            extra_data,
        })
    }
}

fn write_user_data_strings<W: Write>(writer: &mut W, values: &[String]) -> Result<(), Error> {
    (values.len() as i32).write(writer)?;
    for value in values {
        let bytes = value.as_bytes();
        let len = i16::try_from(bytes.len())
            .map_err(|_| Error::new(std::io::ErrorKind::InvalidInput, "item string too long"))?;
        len.write(writer)?;
        writer.write_all(bytes)?;
    }
    Ok(())
}

fn read_user_data_strings<R: Read>(reader: &mut R, legacy: bool) -> Result<Vec<String>, Error> {
    let len = i32::read(reader)?;
    if !(0..=1024).contains(&len) {
        return Err(Error::new(
            std::io::ErrorKind::InvalidData,
            "item string array length out of bounds",
        ));
    }
    let mut values = Vec::with_capacity((len as usize).min(32));
    for _ in 0..len {
        let str_len = usize::from(if legacy {
            u16::read_be(reader)?
        } else {
            u16::read(reader)?
        });
        if str_len > 32767 {
            return Err(Error::new(
                std::io::ErrorKind::InvalidData,
                "item string too long",
            ));
        }
        let mut bytes = vec![0; str_len];
        reader.read_exact(&mut bytes)?;
        values.push(
            String::from_utf8(bytes)
                .map_err(|error| Error::new(std::io::ErrorKind::InvalidData, error))?,
        );
    }
    Ok(values)
}

type ItemUserData = (Nbt, Vec<String>, Vec<String>, i64);

fn read_user_data(user_data: &[u8], is_shield: bool) -> Result<ItemUserData, Error> {
    if user_data.is_empty() {
        return Ok((Nbt::default(), Vec::new(), Vec::new(), 0));
    }

    let mut cursor = std::io::Cursor::new(user_data);
    let nbt_length = i16::read(&mut cursor)?;
    if nbt_length < -1 || (nbt_length == -1 && u8::read(&mut cursor)? != 1) {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "unsupported Bedrock item NBT version",
        ));
    }
    let data_start = cursor.position();
    read_item_attributes(&mut cursor, nbt_length != 0, is_shield, false).or_else(|error| {
        // Older Sculith writers used network NBT and big-endian filter string lengths.
        cursor.set_position(data_start);
        read_item_attributes(&mut cursor, nbt_length != 0, is_shield, true).or(Err(error))
    })
}

fn read_item_attributes(
    cursor: &mut std::io::Cursor<&[u8]>,
    has_nbt: bool,
    is_shield: bool,
    legacy: bool,
) -> Result<ItemUserData, Error> {
    let nbt_data = if has_nbt {
        let result = if legacy {
            Nbt::read(&mut NbtReadHelperBedrock::new(&mut *cursor))
        } else {
            Nbt::read(&mut NbtReadHelperBedrockDisk::new(&mut *cursor))
        };
        result.map_err(|error| Error::new(ErrorKind::InvalidData, error))?
    } else {
        Nbt::default()
    };
    let place_on_blocks = read_user_data_strings(cursor, legacy)?;
    let destroy_blocks = read_user_data_strings(cursor, legacy)?;
    let shield_blocking_tick = if is_shield { i64::read(cursor)? } else { 0 };
    if cursor.position() != cursor.get_ref().len() as u64 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "trailing Bedrock item user data",
        ));
    }
    Ok((
        nbt_data,
        place_on_blocks,
        destroy_blocks,
        shield_blocking_tick,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::{enchantment::Enchantment, item::Item, potion::Potion};

    #[test]
    fn preserves_potion_variants_in_item_descriptors() {
        for (item, bedrock, aux, potion) in [
            (&Item::POTION, &BedrockItem::POTION, 21, &Potion::HEALING),
            (
                &Item::SPLASH_POTION,
                &BedrockItem::SPLASH_POTION,
                16,
                &Potion::STRONG_SWIFTNESS,
            ),
            (
                &Item::LINGERING_POTION,
                &BedrockItem::LINGERING_POTION,
                46,
                &Potion::INFESTED,
            ),
        ] {
            let mut stack = ItemStack::new(1, item);
            let contents = potion_contents_from_bedrock(bedrock.id, aux).unwrap();
            assert_eq!(contents.potion_id, Some(i32::from(potion.id)));
            stack.set_data_component(contents);
            let item = NetworkItemDescriptor::from(&stack);
            assert_eq!(item.id.0, i32::from(bedrock.id));
            for aux_value in [
                item.aux_value,
                ItemStackWrapper::from(&stack).aux_value,
                NetworkItemStackDescriptor::from(&stack).aux_value,
            ] {
                assert_eq!(aux_value, VarUInt(aux));
            }
            let mut bytes = Vec::new();
            item.write_item_instance(&mut bytes).unwrap();
            let decoded = NetworkItemDescriptor::read_item_instance(&mut bytes.as_slice()).unwrap();
            assert_eq!(decoded.aux_value, VarUInt(aux));
        }
        assert!(potion_contents_from_bedrock(BedrockItem::POTION.id, 100).is_none());
    }

    #[test]
    fn preserves_native_tipped_arrows_in_item_descriptors() {
        let arrow = BedrockItem::ARROW.id;
        for aux in 6..=47 {
            let contents = potion_contents_from_bedrock(arrow, aux).unwrap();
            let mut stack = ItemStack::new(1, &Item::TIPPED_ARROW);
            stack.set_data_component(contents);
            let item = NetworkItemDescriptor::from(&stack);
            assert_eq!(item.id.0, i32::from(arrow));
            for encoded_aux in [
                item.aux_value,
                ItemStackWrapper::from(&stack).aux_value,
                NetworkItemStackDescriptor::from(&stack).aux_value,
            ] {
                assert_eq!(encoded_aux, VarUInt(aux));
            }
        }
        let ordinary = ItemStack::new(1, &Item::ARROW);
        assert_eq!(NetworkItemDescriptor::from(&ordinary).aux_value, VarUInt(0));
        for invalid in [0, 5, 48] {
            assert!(potion_contents_from_bedrock(arrow, invalid).is_none());
        }
        assert_eq!(
            potion_contents_from_bedrock(arrow, 37),
            potion_contents_from_bedrock(BedrockItem::POTION.id, 36)
        );
    }

    #[test]
    fn drinkable_decay_preserves_its_effect_and_bedrock_aux() {
        let potion_id = BedrockItem::POTION.id;
        let mut contents = potion_contents_from_bedrock(potion_id, 36).unwrap();
        assert_eq!(contents.potion_id, None);
        let [effect] = contents.custom_effects.as_slice() else {
            panic!("expected the Decay effect");
        };
        assert_eq!(effect.effect_id, "minecraft:wither");
        assert_eq!((effect.amplifier, effect.duration), (1, 800));
        assert!(!effect.ambient && effect.show_particles && effect.show_icon);

        let mut stack = ItemStack::new(1, &Item::POTION);
        stack.set_data_component(contents.clone());
        let descriptor = NetworkItemDescriptor::from(&stack);
        for aux in [
            descriptor.aux_value,
            ItemStackWrapper::from(&stack).aux_value,
            NetworkItemStackDescriptor::from(&stack).aux_value,
        ] {
            assert_eq!(aux, VarUInt(36));
        }
        let mut bytes = Vec::new();
        descriptor.write_item_instance(&mut bytes).unwrap();
        let decoded = NetworkItemDescriptor::read_item_instance(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.aux_value, VarUInt(36));

        contents.custom_effects[0].duration += 1;
        stack.set_data_component(contents);
        assert_ne!(NetworkItemDescriptor::from(&stack).aux_value, VarUInt(36));
        assert!(potion_contents_from_bedrock(BedrockItem::SPLASH_POTION.id, 36).is_none());
        assert!(potion_contents_from_bedrock(BedrockItem::LINGERING_POTION.id, 36).is_none());
    }

    #[test]
    fn writes_bedrock_enchantments_to_equipment_and_inventory() {
        let mut stack = ItemStack::new(1, &Item::DIAMOND_CHESTPLATE);
        stack.add_enchantment(&Enchantment::PROTECTION, 4);
        stack.add_enchantment(&Enchantment::UNBREAKING, 3);
        let mut bytes = Vec::new();
        NetworkItemDescriptor::from(&stack)
            .write(&mut bytes)
            .unwrap();
        let equipment = NetworkItemDescriptor::read(&mut bytes.as_slice()).unwrap();
        bytes.clear();
        ItemStackWrapper::from(&stack).write(&mut bytes).unwrap();
        let wrapped = ItemStackWrapper::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(wrapped.net_id, Some(stack.uid));
        bytes.clear();
        NetworkItemStackDescriptor::from(&stack)
            .write(&mut bytes)
            .unwrap();
        let inventory = NetworkItemDescriptor::read(&mut bytes.as_slice()).unwrap();
        for nbt in [&equipment.nbt_data, &wrapped.nbt_data, &inventory.nbt_data] {
            let entries: Vec<_> = nbt
                .get_list("ench")
                .unwrap()
                .iter()
                .map(|tag| {
                    let tag = tag.extract_compound().unwrap();
                    (tag.get_short("id").unwrap(), tag.get_short("lvl").unwrap())
                })
                .collect();
            assert_eq!(entries, [(0, 4), (17, 3)]);
        }
    }

    #[test]
    fn stack_descriptor_keeps_item_with_unencodable_user_data() {
        let stack = ItemStack::new(1, &Item::SHIELD);
        let mut item = NetworkItemDescriptor::from(&stack);
        item.shield_blocking_tick = 314;
        let mut tags = pumpkin_nbt::NbtCompound::new();
        tags.put_string("invalid", "a".repeat(usize::from(u16::MAX) + 1));
        item.nbt_data = tags.into();

        let descriptor = NetworkItemStackDescriptor::from_descriptor(&item, stack.uid);
        let mut bytes = Vec::new();
        descriptor.write(&mut bytes).unwrap();
        let decoded = NetworkItemDescriptor::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded.id, VarInt(i32::from(BedrockItem::SHIELD.id)));
        assert_eq!(decoded.stack_size, 1);
        assert!(decoded.nbt_data.is_empty());
        assert_eq!(decoded.shield_blocking_tick, 314);
        assert_eq!(
            NetworkItemStackDescriptor::read(&mut bytes.as_slice())
                .unwrap()
                .net_id,
            Some(stack.uid),
        );
    }

    #[test]
    fn reads_item_metadata_and_filter_strings_in_both_encodings() {
        let mut tags = pumpkin_nbt::NbtCompound::new();
        tags.put_int("rank", 20);
        let nbt = Nbt::from(tags);
        let item = NetworkItemDescriptor {
            id: VarInt(7),
            stack_size: 2,
            nbt_data: nbt.clone(),
            place_on_blocks: vec!["stone".into()],
            ..Default::default()
        };
        let mut body = Vec::new();
        item.write_item_instance(&mut body).unwrap();
        let mut body = body.as_slice();
        let decoded = NetworkItemDescriptor::read_item_instance(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.nbt_data.get_int("rank"), Some(20));
        assert_eq!(decoded.place_on_blocks, ["stone"]);

        let mut legacy = vec![0xff, 0xff, 1];
        nbt.write_to_writer_bedrock(&mut legacy).unwrap();
        legacy.extend_from_slice(&[1, 0, 0, 0, 0, 5]);
        legacy.extend_from_slice(b"stone");
        legacy.extend_from_slice(&[0, 0, 0, 0]);
        let (tags, place_on, destroy, blocking) = read_user_data(&legacy, false).unwrap();
        assert_eq!(tags.get_int("rank"), Some(20));
        assert_eq!(place_on, ["stone"]);
        assert!(destroy.is_empty());
        assert_eq!(blocking, 0);
    }

    #[test]
    fn parses_reserved_container_names() {
        assert_eq!(
            ContainerName::try_from(67).unwrap(),
            ContainerName::ContainerReservedA
        );
        assert_eq!(
            ContainerName::try_from(68).unwrap(),
            ContainerName::ContainerReservedB
        );
        assert_eq!(
            ContainerName::try_from(69).unwrap(),
            ContainerName::ContainerReservedC
        );
        assert_eq!(
            ContainerName::try_from(70).unwrap(),
            ContainerName::ContainerReservedD
        );
    }
}
