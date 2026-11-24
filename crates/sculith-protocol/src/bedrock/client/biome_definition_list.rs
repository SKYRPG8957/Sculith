use std::io::{Error, Write};

use pumpkin_data::bedrock_biome::BIOME_DEFINITIONS;
use pumpkin_macros::packet;

use crate::{
    codec::var_uint::VarUInt,
    serial::{PacketRead, PacketWrite},
};

/// Sent by the server to synchronize all available biome definitions and properties with the client.
#[packet(pumpkin_data::bedrock_packet::BIOME_DEFINITION_LIST)]
pub struct CBiomeDefinitionList;

impl PacketWrite for CBiomeDefinitionList {
    fn write<W: Write>(&self, writer: &mut W) -> Result<(), Error> {
        writer.write_all(BIOME_DEFINITIONS)
    }
}

/// Biome definitions and the string table referenced by their indices.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::BIOME_DEFINITION_LIST)]
pub struct CBiomeDefinitionListPayload {
    pub biomes: Vec<BedrockBiomeDefinition>,
    pub strings: Vec<String>,
}

#[derive(PacketRead)]
pub struct BedrockBiomeDefinition {
    pub name_index: u16,
    pub biome_id: u16,
    pub temperature: f32,
    pub downfall: f32,
    pub foliage_snow: f32,
    pub depth: f32,
    pub scale: f32,
    pub water_color_argb: i32,
    pub rain: bool,
    pub tags: Option<Vec<u16>>,
    pub chunk_generation: Option<BiomeChunkGeneration>,
}

#[derive(PacketRead)]
pub struct BiomeChunkGeneration {
    pub climate: Option<BiomeClimate>,
    pub features: Option<Vec<BiomeGenerationFeature>>,
    pub mountain: Option<BiomeMountain>,
    pub material_adjustments: Option<Vec<BiomeMaterialAdjustment>>,
    pub overworld: Option<BiomeOverworldRules>,
    pub multi_noise: Option<BiomeMultiNoiseRules>,
    pub legacy_pre_hills: Option<Vec<BiomeConditionalTransformation>>,
    pub replacements: Option<Vec<BiomeReplacement>>,
    pub village_type: Option<u8>,
    pub surface: Option<BiomeSurfaceBuilder>,
    pub subsurface: Option<BiomeSurfaceBuilder>,
}

#[derive(PacketRead)]
pub struct BiomeClimate {
    pub temperature: f32,
    pub downfall: f32,
    pub snow_accumulation_min: f32,
    pub snow_accumulation_max: f32,
}

#[derive(PacketRead)]
pub struct BiomeGenerationFeature {
    pub scatter: BiomeFeatureScatter,
    pub feature_index: u16,
    pub identifier_index: u16,
    pub pass_index: u16,
    pub can_use_internal_feature: bool,
}

#[derive(PacketRead)]
pub struct BiomeFeatureScatter {
    pub coordinates: Vec<BiomeFeatureCoordinate>,
    pub evaluation_order: VarUInt,
    pub chance_percent_operation: VarUInt,
    pub chance_percent: u16,
    pub chance_numerator: i32,
    pub chance_denominator: i32,
    pub iterations_operation: VarUInt,
    pub iterations: u16,
}

#[derive(PacketRead)]
pub struct BiomeFeatureCoordinate {
    pub minimum_operation: VarUInt,
    pub minimum: u16,
    pub maximum_operation: VarUInt,
    pub maximum: u16,
    pub grid_offset: u32,
    pub grid_step_size: u32,
    pub distribution: VarUInt,
}

#[derive(PacketRead)]
pub struct BiomeMountain {
    pub steep_block: i32,
    pub north_slopes: bool,
    pub south_slopes: bool,
    pub west_slopes: bool,
    pub east_slopes: bool,
    pub top_slide_enabled: bool,
}

#[derive(PacketRead)]
pub struct BiomeMaterialAdjustment {
    pub noise_frequency_scale: f32,
    pub noise_lower_bound: f32,
    pub noise_upper_bound: f32,
    pub height_minimum_operation: VarUInt,
    pub height_minimum: u16,
    pub height_maximum_operation: VarUInt,
    pub height_maximum: u16,
    pub adjusted_materials: BiomeSurfaceMaterials,
}

#[derive(PacketRead)]
pub struct BiomeSurfaceMaterials {
    pub top_block: i32,
    pub mid_block: i32,
    pub sea_floor_block: i32,
    pub foundation_block: i32,
    pub sea_block: i32,
    pub sea_floor_depth: i32,
}

#[derive(PacketRead)]
pub struct BiomeOverworldRules {
    pub hills: Vec<BiomeWeightedTransformation>,
    pub mutate: Vec<BiomeWeightedTransformation>,
    pub river: Vec<BiomeWeightedTransformation>,
    pub shore: Vec<BiomeWeightedTransformation>,
    pub pre_hills_edge: Vec<BiomeConditionalTransformation>,
    pub post_shore_edge: Vec<BiomeConditionalTransformation>,
    pub climate: Vec<BiomeWeightedTemperature>,
}

#[derive(PacketRead)]
pub struct BiomeWeightedTransformation {
    pub biome_index: u16,
    pub weight: i32,
}

#[derive(PacketRead)]
pub struct BiomeConditionalTransformation {
    pub transforms_into: Vec<BiomeWeightedTransformation>,
    pub condition_index: u16,
    pub minimum_passing_neighbors: u32,
}

#[derive(PacketRead)]
pub struct BiomeWeightedTemperature {
    pub temperature: VarUInt,
    pub weight: i32,
}

#[derive(PacketRead)]
pub struct BiomeMultiNoiseRules {
    pub temperature: f32,
    pub humidity: f32,
    pub altitude: f32,
    pub weirdness: f32,
    pub weight: f32,
}

#[derive(PacketRead)]
pub struct BiomeReplacement {
    pub biome_index: i16,
    pub dimension: i16,
    pub target_biomes: Vec<i16>,
    pub amount: f32,
    pub noise_frequency_scale: f32,
    pub replacement_index: i32,
}

#[derive(PacketRead)]
pub struct BiomeSurfaceBuilder {
    pub surface_materials: Option<BiomeSurfaceMaterials>,
    pub has_default_overworld_surface: bool,
    pub has_swamp_surface: bool,
    pub has_frozen_ocean_surface: bool,
    pub has_the_end_surface: bool,
    pub mesa: Option<BiomeMesaSurface>,
    pub capped: Option<BiomeCappedSurface>,
    pub noise_gradient: Option<BiomeNoiseGradientSurface>,
}

#[derive(PacketRead)]
pub struct BiomeMesaSurface {
    pub clay_material: i32,
    pub hard_clay_material: i32,
    pub bryce_pillars: bool,
    pub has_forest: bool,
}

#[derive(PacketRead)]
pub struct BiomeCappedSurface {
    pub floor_blocks: Vec<i32>,
    pub ceiling_blocks: Vec<i32>,
    pub sea_block: Option<i32>,
    pub foundation_block: Option<i32>,
    pub beach_block: Option<i32>,
}

#[derive(PacketRead)]
pub struct BiomeNoiseGradientSurface {
    pub nonreplaceable_blocks: Vec<i32>,
    pub gradient_blocks: Vec<BiomeNoiseBlock>,
    pub noise_name: String,
    pub first_octave: i32,
    pub amplitudes: Vec<f32>,
}

#[derive(PacketRead)]
pub struct BiomeNoiseBlock {
    pub noise: String,
    pub threshold: f32,
    pub minimum: f32,
    pub maximum: f32,
    pub block: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_registry_contains_all_biomes() {
        let biome_count = VarUInt::read(&mut BIOME_DEFINITIONS.as_slice())
            .expect("generated biome count must decode");

        assert_eq!(
            biome_count.0 as usize,
            pumpkin_data::bedrock_biome::BIOME_COUNT
        );
        assert_eq!(biome_count.0, 89);
        assert!(BIOME_DEFINITIONS.len() > 100_000);

        let mut body = BIOME_DEFINITIONS.as_slice();
        let packet = CBiomeDefinitionListPayload::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(packet.biomes.len(), 89);
        assert!(!packet.strings.is_empty());
        assert!(
            packet
                .biomes
                .iter()
                .any(|biome| biome.chunk_generation.is_some())
        );
    }
}
