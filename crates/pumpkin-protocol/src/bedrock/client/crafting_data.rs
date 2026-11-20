use crate::{
    bedrock::network_item::NetworkItemDescriptor,
    codec::{var_int::VarInt, var_uint::VarUInt},
    serial::{PacketRead, PacketWrite},
};
use pumpkin_macros::packet;
use std::io::{Error, ErrorKind, Read, Write};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeItemDescriptor {
    Empty,
    Item {
        identifier: String,
        metadata_value: i32,
    },
    Tag(String),
    Molang(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDescriptorCount {
    pub descriptor: RecipeItemDescriptor,
    pub count: i32,
}

impl ItemDescriptorCount {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            descriptor: RecipeItemDescriptor::Empty,
            count: 0,
        }
    }

    #[must_use]
    pub const fn item(identifier: String, metadata_value: i32) -> Self {
        Self {
            descriptor: RecipeItemDescriptor::Item {
                identifier,
                metadata_value,
            },
            count: 1,
        }
    }

    #[must_use]
    pub const fn tag(identifier: String) -> Self {
        Self {
            descriptor: RecipeItemDescriptor::Tag(identifier),
            count: 1,
        }
    }
}

impl PacketWrite for ItemDescriptorCount {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        match &self.descriptor {
            RecipeItemDescriptor::Empty => {
                VarUInt(0).write(writer)?;
                VarInt(32767).write(writer)?;
            }
            RecipeItemDescriptor::Item {
                identifier,
                metadata_value,
            } => {
                VarUInt(1).write(writer)?;
                "name".write(writer)?;
                identifier.write(writer)?;
                VarInt(*metadata_value).write(writer)?;
            }
            RecipeItemDescriptor::Tag(identifier) => {
                VarUInt(1).write(writer)?;
                "item_tag".write(writer)?;
                identifier.write(writer)?;
                VarInt(32767).write(writer)?;
            }
            RecipeItemDescriptor::Molang(expression) => {
                VarUInt(1).write(writer)?;
                "tags".write(writer)?;
                expression.write(writer)?;
                VarInt(32767).write(writer)?;
            }
        }
        VarInt(self.count).write(writer)?;
        Ok(())
    }
}

impl PacketRead for ItemDescriptorCount {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let descriptor = match VarUInt::read(reader)?.0 {
            0 => {
                let _auxiliary = VarInt::read(reader)?;
                RecipeItemDescriptor::Empty
            }
            1 => {
                let kind = String::read(reader)?;
                let value = String::read(reader)?;
                let auxiliary = VarInt::read(reader)?.0;
                match kind.as_str() {
                    "name" => RecipeItemDescriptor::Item {
                        identifier: value,
                        metadata_value: auxiliary,
                    },
                    "item_tag" => RecipeItemDescriptor::Tag(value),
                    "tags" => RecipeItemDescriptor::Molang(value),
                    _ => {
                        return Err(Error::new(
                            ErrorKind::InvalidData,
                            "invalid Bedrock recipe ingredient",
                        ));
                    }
                }
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "unsupported Bedrock item descriptor",
                ));
            }
        };
        Ok(Self {
            descriptor,
            count: VarInt::read(reader)?.0,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RecipeUnlockRequirement {
    pub context: i32,
    pub ingredients: Option<Vec<ItemDescriptorCount>>,
}

impl RecipeUnlockRequirement {
    #[must_use]
    pub const fn always_unlocked() -> Self {
        Self {
            context: 1,
            ingredients: None,
        }
    }
}

impl PacketWrite for RecipeUnlockRequirement {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        VarInt(self.context).write(writer)?;
        self.ingredients.is_some().write(writer)?;
        if let Some(ingredients) = &self.ingredients {
            VarUInt(ingredients.len() as u32).write(writer)?;
            ingredients.write(writer)?;
        }
        Ok(())
    }
}

impl PacketRead for RecipeUnlockRequirement {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            context: VarInt::read(reader)?.0,
            ingredients: Option::read(reader)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BedrockShapelessRecipe {
    pub recipe_id: String,
    pub input: Vec<ItemDescriptorCount>,
    pub output: Vec<NetworkItemDescriptor>,
    pub uuid: Uuid,
    pub block: String,
    pub priority: VarInt,
    pub unlock_requirement: Option<RecipeUnlockRequirement>,
    pub recipe_network_id: VarUInt,
}

fn write_bedrock_uuid<W: Write>(uuid: Uuid, writer: &mut W) -> Result<(), Error> {
    let (msb, lsb) = uuid.as_u64_pair();
    writer.write_all(&msb.to_le_bytes())?;
    writer.write_all(&lsb.to_le_bytes())
}

impl PacketWrite for BedrockShapelessRecipe {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.recipe_id.write(writer)?;

        // input slice with VarUInt length prefix
        VarUInt(self.input.len() as u32).write(writer)?;
        for item in &self.input {
            item.write(writer)?;
        }

        // output slice with VarUInt length prefix
        VarUInt(self.output.len() as u32).write(writer)?;
        for item in &self.output {
            item.write_item_instance(writer)?;
        }

        // uuid
        write_bedrock_uuid(self.uuid, writer)?;

        // block
        self.block.write(writer)?;

        // priority
        self.priority.write(writer)?;

        self.unlock_requirement.write(writer)?;

        // recipe_network_id
        self.recipe_network_id.write(writer)?;

        Ok(())
    }
}

fn read_recipe_outputs<R: Read>(reader: &mut R) -> Result<Vec<NetworkItemDescriptor>, Error> {
    let count = VarUInt::read(reader)?.0 as usize;
    if count > 65_536 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "too many Bedrock recipe outputs",
        ));
    }
    let mut output = Vec::with_capacity(count.min(16));
    for _ in 0..count {
        output.push(NetworkItemDescriptor::read_item_instance(reader)?);
    }
    Ok(output)
}

impl PacketRead for BedrockShapelessRecipe {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            recipe_id: String::read(reader)?,
            input: Vec::read(reader)?,
            output: read_recipe_outputs(reader)?,
            uuid: Uuid::read(reader)?,
            block: String::read(reader)?,
            priority: VarInt::read(reader)?,
            unlock_requirement: Option::read(reader)?,
            recipe_network_id: VarUInt::read(reader)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BedrockShapedRecipe {
    pub recipe_id: String,
    pub width: VarInt,
    pub height: VarInt,
    pub input: Vec<ItemDescriptorCount>,
    pub output: Vec<NetworkItemDescriptor>,
    pub uuid: Uuid,
    pub block: String,
    pub priority: VarInt,
    pub assume_symmetry: bool,
    pub unlock_requirement: Option<RecipeUnlockRequirement>,
    pub recipe_network_id: VarUInt,
}

impl PacketWrite for BedrockShapedRecipe {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.recipe_id.write(writer)?;
        self.width.write(writer)?;
        self.height.write(writer)?;

        VarUInt(self.input.len() as u32).write(writer)?;
        for item in &self.input {
            item.write(writer)?;
        }

        // output slice with VarUInt length prefix
        VarUInt(self.output.len() as u32).write(writer)?;
        for item in &self.output {
            item.write_item_instance(writer)?;
        }

        // uuid
        write_bedrock_uuid(self.uuid, writer)?;

        // block
        self.block.write(writer)?;

        // priority
        self.priority.write(writer)?;

        // assume_symmetry
        self.assume_symmetry.write(writer)?;

        self.unlock_requirement.write(writer)?;

        // recipe_network_id
        self.recipe_network_id.write(writer)?;

        Ok(())
    }
}

impl PacketRead for BedrockShapedRecipe {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let recipe_id = String::read(reader)?;
        let width = VarInt::read(reader)?;
        let height = VarInt::read(reader)?;
        let input = Vec::read(reader)?;
        if width.0 < 0
            || height.0 < 0
            || i64::from(width.0) * i64::from(height.0) != input.len() as i64
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Bedrock recipe input count does not match its dimensions",
            ));
        }
        Ok(Self {
            recipe_id,
            width,
            height,
            input,
            output: read_recipe_outputs(reader)?,
            uuid: Uuid::read(reader)?,
            block: String::read(reader)?,
            priority: VarInt::read(reader)?,
            assume_symmetry: bool::read(reader)?,
            unlock_requirement: Option::read(reader)?,
            recipe_network_id: VarUInt::read(reader)?,
        })
    }
}

#[derive(Clone, Debug)]
pub enum BedrockRecipe {
    Shapeless(BedrockShapelessRecipe),
    Shaped(BedrockShapedRecipe),
}

impl PacketWrite for BedrockRecipe {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        match self {
            Self::Shapeless(recipe) => {
                VarInt(0).write(writer)?; // type 0: Shapeless
                recipe.write(writer)?;
            }
            Self::Shaped(recipe) => {
                VarInt(1).write(writer)?; // type 1: Shaped
                recipe.write(writer)?;
            }
        }
        Ok(())
    }
}

/// Sent by the server to send all crafting and smelting recipes available in the world.
#[packet(pumpkin_data::bedrock_packet::CRAFTING_DATA)]
pub struct CCraftingData {
    /// Additional shaped and shapeless recipes (or incremental recipes when `clean_recipes` is false).
    pub recipes: Vec<BedrockRecipe>,
    /// Whether previous recipe entries should be cleared and replaced with the full static + dynamic set.
    pub clean_recipes: bool,
}

impl PacketWrite for CCraftingData {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        let shaped = self
            .recipes
            .iter()
            .filter_map(|recipe| match recipe {
                BedrockRecipe::Shaped(recipe) => Some(recipe),
                BedrockRecipe::Shapeless(_) => None,
            })
            .collect::<Vec<_>>();
        let shapeless = self
            .recipes
            .iter()
            .filter_map(|recipe| match recipe {
                BedrockRecipe::Shapeless(recipe) => Some(recipe),
                BedrockRecipe::Shaped(_) => None,
            })
            .collect::<Vec<_>>();

        if self.clean_recipes {
            VarUInt(pumpkin_data::bedrock_recipe::SHAPED_COUNT + shaped.len() as u32)
                .write(writer)?;
            writer.write_all(pumpkin_data::bedrock_recipe::SHAPED_RECIPES)?;
            for recipe in shaped {
                recipe.write(writer)?;
            }

            VarUInt(pumpkin_data::bedrock_recipe::SHAPELESS_COUNT + shapeless.len() as u32)
                .write(writer)?;
            writer.write_all(pumpkin_data::bedrock_recipe::SHAPELESS_RECIPES)?;
            for recipe in shapeless {
                recipe.write(writer)?;
            }

            writer.write_all(pumpkin_data::bedrock_recipe::TAIL_SECTIONS)?;
        } else {
            VarUInt(shaped.len() as u32).write(writer)?;
            for recipe in shaped {
                recipe.write(writer)?;
            }

            VarUInt(shapeless.len() as u32).write(writer)?;
            for recipe in shapeless {
                recipe.write(writer)?;
            }

            // Multi, user, chemistry, smithing, potion, container and material arrays.
            for _ in 0..9 {
                VarUInt(0).write(writer)?;
            }
        }

        // clean_recipes
        self.clean_recipes.write(writer)?;

        Ok(())
    }
}

/// All recipe tables carried by a Bedrock crafting data packet.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::CRAFTING_DATA)]
pub struct CCraftingDataPayload {
    pub shaped: Vec<BedrockShapedRecipe>,
    pub shapeless: Vec<BedrockShapelessRecipe>,
    pub multi: Vec<BedrockMultiRecipe>,
    pub user_data_shapeless: Vec<BedrockShapelessRecipe>,
    pub shapeless_chemistry: Vec<BedrockShapelessRecipe>,
    pub shaped_chemistry: Vec<BedrockShapedRecipe>,
    pub smithing_transform: Vec<BedrockSmithingTransformRecipe>,
    pub smithing_trim: Vec<BedrockSmithingTrimRecipe>,
    pub potions: Vec<BedrockPotionRecipe>,
    pub potion_container_changes: Vec<BedrockPotionContainerChange>,
    pub material_reducers: Vec<BedrockMaterialReducer>,
    pub clean_recipes: bool,
}

#[derive(PacketRead)]
pub struct BedrockMultiRecipe {
    pub uuid: Uuid,
    pub recipe_network_id: VarUInt,
}

pub struct BedrockSmithingTransformRecipe {
    pub recipe_id: String,
    pub template: ItemDescriptorCount,
    pub base: ItemDescriptorCount,
    pub addition: ItemDescriptorCount,
    pub result: NetworkItemDescriptor,
    pub block: String,
    pub recipe_network_id: VarUInt,
}

impl PacketRead for BedrockSmithingTransformRecipe {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        Ok(Self {
            recipe_id: String::read(reader)?,
            template: ItemDescriptorCount::read(reader)?,
            base: ItemDescriptorCount::read(reader)?,
            addition: ItemDescriptorCount::read(reader)?,
            result: NetworkItemDescriptor::read_item_instance(reader)?,
            block: String::read(reader)?,
            recipe_network_id: VarUInt::read(reader)?,
        })
    }
}

#[derive(PacketRead)]
pub struct BedrockSmithingTrimRecipe {
    pub recipe_id: String,
    pub template: ItemDescriptorCount,
    pub base: ItemDescriptorCount,
    pub addition: ItemDescriptorCount,
    pub block: String,
    pub recipe_network_id: VarUInt,
}

#[derive(PacketRead)]
pub struct BedrockPotionRecipe {
    pub input_potion_id: VarInt,
    pub input_potion_metadata: VarInt,
    pub reagent_item_id: VarInt,
    pub reagent_item_metadata: VarInt,
    pub output_potion_id: VarInt,
    pub output_potion_metadata: VarInt,
}

#[derive(PacketRead)]
pub struct BedrockPotionContainerChange {
    pub input_item_id: VarInt,
    pub reagent_item_id: VarInt,
    pub output_item_id: VarInt,
}

pub struct BedrockMaterialReducer {
    pub input_item_id: i32,
    pub input_item_metadata: u16,
    pub outputs: Vec<BedrockMaterialReducerOutput>,
}

impl PacketRead for BedrockMaterialReducer {
    fn read<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let input = VarInt::read(reader)?.0;
        Ok(Self {
            input_item_id: input >> 16,
            input_item_metadata: input as u16,
            outputs: Vec::read(reader)?,
        })
    }
}

#[derive(PacketRead)]
pub struct BedrockMaterialReducerOutput {
    pub item_id: VarInt,
    pub count: VarInt,
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use uuid::Uuid;

    use crate::{
        bedrock::network_item::NetworkItemDescriptor,
        codec::{var_int::VarInt, var_uint::VarUInt},
        serial::{PacketRead, PacketWrite},
    };

    use super::{
        BedrockRecipe, BedrockShapedRecipe, BedrockShapelessRecipe, CCraftingData,
        CCraftingDataPayload, ItemDescriptorCount, RecipeUnlockRequirement,
    };

    fn read_str(cursor: &mut Cursor<&[u8]>) -> String {
        let len = VarUInt::read(cursor).unwrap().0 as usize;
        let mut buf = vec![0u8; len];
        cursor.read_exact(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn read_bedrock_uuid(cursor: &mut Cursor<&[u8]>) -> Uuid {
        let mut msb = [0u8; 8];
        let mut lsb = [0u8; 8];
        cursor.read_exact(&mut msb).unwrap();
        cursor.read_exact(&mut lsb).unwrap();
        Uuid::from_u64_pair(u64::from_le_bytes(msb), u64::from_le_bytes(lsb))
    }

    fn read_ingredient(cursor: &mut Cursor<&[u8]>) {
        let internal_type = VarUInt::read(cursor).unwrap().0;
        match internal_type {
            0 => {
                let aux = VarInt::read(cursor).unwrap().0;
                assert_eq!(aux, 32767);
            }
            1 => {
                let deferred_kind = read_str(cursor);
                let _name_or_tag = read_str(cursor);
                let _aux = VarInt::read(cursor).unwrap().0;
                assert!(deferred_kind == "name" || deferred_kind == "item_tag");
            }
            other => panic!("unexpected ingredient type {other}"),
        }
        let _stack_size = VarInt::read(cursor).unwrap().0;
    }

    fn read_item_instance(cursor: &mut Cursor<&[u8]>) {
        let id = VarInt::read(cursor).unwrap().0;
        if id == 0 {
            return;
        }
        let _count = u16::read(cursor).unwrap();
        let _aux = VarUInt::read(cursor).unwrap().0;
        let _block_runtime_id = VarInt::read(cursor).unwrap().0;
        let user_data_len = VarUInt::read(cursor).unwrap().0 as usize;
        let mut user_data = vec![0u8; user_data_len];
        cursor.read_exact(&mut user_data).unwrap();
        assert!(user_data.len() >= 10);
        let marker = i16::from_le_bytes([user_data[0], user_data[1]]);
        assert!(marker == 0 || (marker == -1 && user_data[2] == 1 && user_data[3] == 0x0a));
    }

    fn read_unlock_requirement(cursor: &mut Cursor<&[u8]>) {
        let has_req = bool::read(cursor).unwrap();
        assert!(has_req);
        let context = VarInt::read(cursor).unwrap().0;
        assert_eq!(context, 1);
        let has_ingredients = bool::read(cursor).unwrap();
        assert!(!has_ingredients);
    }

    fn read_shapeless(cursor: &mut Cursor<&[u8]>) -> u32 {
        let _id = read_str(cursor);
        let in_len = VarUInt::read(cursor).unwrap().0;
        for _ in 0..in_len {
            read_ingredient(cursor);
        }
        let out_len = VarUInt::read(cursor).unwrap().0;
        for _ in 0..out_len {
            read_item_instance(cursor);
        }
        let _uuid = read_bedrock_uuid(cursor);
        let _block = read_str(cursor);
        let _priority = VarInt::read(cursor).unwrap().0;
        read_unlock_requirement(cursor);
        VarUInt::read(cursor).unwrap().0
    }

    fn read_shaped(cursor: &mut Cursor<&[u8]>) -> u32 {
        let _id = read_str(cursor);
        let width = VarInt::read(cursor).unwrap().0;
        let height = VarInt::read(cursor).unwrap().0;
        let in_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(in_len as i32, width * height);
        for _ in 0..in_len {
            read_ingredient(cursor);
        }
        let out_len = VarUInt::read(cursor).unwrap().0;
        for _ in 0..out_len {
            read_item_instance(cursor);
        }
        let _uuid = read_bedrock_uuid(cursor);
        let _block = read_str(cursor);
        let _priority = VarInt::read(cursor).unwrap().0;
        let _assume_symmetry = bool::read(cursor).unwrap();
        read_unlock_requirement(cursor);
        VarUInt::read(cursor).unwrap().0
    }

    #[test]
    fn item_tag_descriptor_uses_bedrock_recipe_wire_format() {
        let mut encoded = Vec::new();
        ItemDescriptorCount::tag("minecraft:planks".to_string())
            .write(&mut encoded)
            .unwrap();

        let mut expected = vec![1, 8];
        expected.extend_from_slice(b"item_tag");
        expected.push(16);
        expected.extend_from_slice(b"minecraft:planks");
        expected.extend_from_slice(&[0xfe, 0xff, 0x03, 0x02]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn round_trips_an_optional_recipe_ingredient_list() {
        let requirement = RecipeUnlockRequirement {
            context: 0,
            ingredients: Some(vec![ItemDescriptorCount::tag("minecraft:planks".into())]),
        };
        let mut body = Vec::new();
        requirement.write(&mut body).unwrap();
        let mut body = body.as_slice();
        let decoded = RecipeUnlockRequirement::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(decoded.context, 0);
        assert_eq!(decoded.ingredients, requirement.ingredients);
    }

    fn sample_dynamic_recipes(dynamic_net_id: u32) -> Vec<BedrockRecipe> {
        let output_item = |stack_size| NetworkItemDescriptor {
            id: VarInt(323),
            stack_size,
            aux_value: VarUInt(0),
            block_runtime_id: VarInt(0),
            nbt_data: pumpkin_nbt::Nbt::default(),
            place_on_blocks: Vec::new(),
            destroy_blocks: Vec::new(),
            shield_blocking_tick: 0,
        };
        vec![
            BedrockRecipe::Shaped(BedrockShapedRecipe {
                recipe_id: "pumpkin:test_shaped".to_string(),
                width: VarInt(1),
                height: VarInt(1),
                input: vec![ItemDescriptorCount::tag("minecraft:planks".to_string())],
                output: vec![output_item(2)],
                uuid: Uuid::from_u128(1),
                block: "crafting_table".to_string(),
                priority: VarInt(1),
                assume_symmetry: true,
                unlock_requirement: Some(RecipeUnlockRequirement::always_unlocked()),
                recipe_network_id: VarUInt(dynamic_net_id),
            }),
            BedrockRecipe::Shapeless(BedrockShapelessRecipe {
                recipe_id: "pumpkin:test_shapeless".to_string(),
                input: vec![ItemDescriptorCount::item("minecraft:stick".to_string(), 0)],
                output: vec![output_item(1)],
                uuid: Uuid::from_u128(2),
                block: "crafting_table".to_string(),
                priority: VarInt(1),
                unlock_requirement: Some(RecipeUnlockRequirement::always_unlocked()),
                recipe_network_id: VarUInt(dynamic_net_id + 1),
            }),
        ]
    }

    fn assert_tail_sections(cursor: &mut Cursor<&[u8]>, expected_next_net_id: u32) {
        let multi_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(multi_len, pumpkin_data::bedrock_recipe::MULTI_COUNT);
        let mut multi_uuids = Vec::new();
        for _ in 0..multi_len {
            multi_uuids.push(read_bedrock_uuid(cursor));
            let _net_id = VarUInt::read(cursor).unwrap().0;
        }
        assert!(
            multi_uuids.contains(&Uuid::parse_str("442d85ed-8272-4543-a6f1-418f90ded05d").unwrap())
        );

        let user_data_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(
            user_data_len,
            pumpkin_data::bedrock_recipe::USER_DATA_SHAPELESS_COUNT
        );
        for _ in 0..user_data_len {
            let _ = read_shapeless(cursor);
        }

        assert_eq!(VarUInt::read(cursor).unwrap().0, 0); // shapeless_chemistry
        assert_eq!(VarUInt::read(cursor).unwrap().0, 0); // shaped_chemistry

        let smithing_transform_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(
            smithing_transform_len,
            pumpkin_data::bedrock_recipe::SMITHING_TRANSFORM_COUNT
        );
        for _ in 0..smithing_transform_len {
            let _id = read_str(cursor);
            read_ingredient(cursor);
            read_ingredient(cursor);
            read_ingredient(cursor);
            read_item_instance(cursor);
            assert_eq!(read_str(cursor), "smithing_table");
            let _net_id = VarUInt::read(cursor).unwrap().0;
        }

        let smithing_trim_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(
            smithing_trim_len,
            pumpkin_data::bedrock_recipe::SMITHING_TRIM_COUNT
        );
        let mut max_static_net_id = 0;
        for _ in 0..smithing_trim_len {
            let _id = read_str(cursor);
            read_ingredient(cursor);
            read_ingredient(cursor);
            read_ingredient(cursor);
            assert_eq!(read_str(cursor), "smithing_table");
            max_static_net_id = VarUInt::read(cursor).unwrap().0;
        }
        assert_eq!(max_static_net_id + 1, expected_next_net_id);

        let potion_mix_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(
            potion_mix_len,
            pumpkin_data::bedrock_recipe::POTION_MIX_COUNT
        );
        for _ in 0..(potion_mix_len * 6) {
            let _ = VarInt::read(cursor).unwrap().0;
        }

        let container_mix_len = VarUInt::read(cursor).unwrap().0;
        assert_eq!(
            container_mix_len,
            pumpkin_data::bedrock_recipe::CONTAINER_MIX_COUNT
        );
        for _ in 0..(container_mix_len * 3) {
            let _ = VarInt::read(cursor).unwrap().0;
        }

        assert_eq!(VarUInt::read(cursor).unwrap().0, 0); // material_reducers
    }

    #[test]
    fn full_bedrock_crafting_data_wire_payload_decodes_cleanly() {
        let dynamic_net_id = pumpkin_data::bedrock_recipe::NEXT_RECIPE_NETWORK_ID;
        let packet = CCraftingData {
            recipes: sample_dynamic_recipes(dynamic_net_id),
            clean_recipes: true,
        };
        let mut encoded = Vec::new();
        packet.write(&mut encoded).unwrap();

        let mut cursor = Cursor::new(encoded.as_slice());

        let shaped_len = VarUInt::read(&mut cursor).unwrap().0;
        assert_eq!(shaped_len, pumpkin_data::bedrock_recipe::SHAPED_COUNT + 1);
        let mut last_shaped_id = 0;
        for _ in 0..shaped_len {
            last_shaped_id = read_shaped(&mut cursor);
        }
        assert_eq!(last_shaped_id, dynamic_net_id);

        let shapeless_len = VarUInt::read(&mut cursor).unwrap().0;
        assert_eq!(
            shapeless_len,
            pumpkin_data::bedrock_recipe::SHAPELESS_COUNT + 1
        );
        let mut last_shapeless_id = 0;
        for _ in 0..shapeless_len {
            last_shapeless_id = read_shapeless(&mut cursor);
        }
        assert_eq!(last_shapeless_id, dynamic_net_id + 1);

        assert_tail_sections(&mut cursor, dynamic_net_id);
        assert!(bool::read(&mut cursor).unwrap());
        assert_eq!(cursor.position() as usize, encoded.len());

        let mut body = encoded.as_slice();
        let decoded = CCraftingDataPayload::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert!(decoded.clean_recipes);
        assert_eq!(
            decoded.shaped.last().unwrap().recipe_network_id.0,
            dynamic_net_id
        );
        assert_eq!(
            decoded.shapeless.last().unwrap().recipe_network_id.0,
            dynamic_net_id + 1
        );
    }
}
