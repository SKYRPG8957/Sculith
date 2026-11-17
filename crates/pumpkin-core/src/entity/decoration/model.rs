//! One tracked display with native and Java presentations of a shared model.

use std::{
    borrow::Cow,
    sync::{Arc, atomic::Ordering},
};

use pumpkin_data::{
    damage::DamageType, data_component::DataComponent, data_component_impl::ItemModelImpl,
    entity::EntityType, item::Item, item_stack::ItemStack,
};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use pumpkin_protocol::{
    bedrock::client::{
        move_actor_absolute::CMoveActorAbsolute,
        set_actor_data::{MetadataValue, SyncedActorDataList, entity_data_flag, entity_data_key},
    },
    codec::var_ulong::VarULong,
};
use pumpkin_util::math::{position::BlockPos, vector3::Vector3};
use sculith_packs::ModelMapping;

use crate::{
    entity::{Entity, EntityBase, living::LivingEntity},
    server::Server,
    world::World,
};

use super::display::ItemDisplayEntity;

const MODEL_NAMESPACE: &str = "sculith";
const MODEL_KEY: &str = "model";
const MAX_MODEL_SCALE: f32 = 16.0;

pub struct ModelDisplayEntity {
    display: Arc<ItemDisplayEntity>,
    model: Option<ModelMapping>,
}

pub(crate) fn is_saved_model(nbt: &NbtCompound) -> bool {
    nbt.get_compound("item")
        .and_then(|item| item.get_compound("components"))
        .and_then(|components| components.get_compound(DataComponent::CustomData.to_name()))
        .and_then(|data| data.get_compound(MODEL_NAMESPACE))
        .is_some_and(|data| data.get(MODEL_KEY).is_some())
}

fn model_mapping(world: &World, name: &str) -> Option<ModelMapping> {
    world
        .server
        .upgrade()?
        .shared_resources
        .as_ref()?
        .model(name)
        .cloned()
}

fn validate_position(world: &World, position: Vector3<f64>) -> Result<(), String> {
    if ![position.x, position.y, position.z]
        .iter()
        .all(|value| value.is_finite())
    {
        return Err("Model position must be finite".into());
    }
    let block = BlockPos::containing_vec(position);
    if !World::is_valid(block) || !world.is_in_height_limit(block.0.y) {
        return Err("Model position is outside the world".into());
    }
    Ok(())
}

fn validate_scale(scale: f32) -> Result<(), String> {
    if !scale.is_finite() || scale <= 0.0 || scale > MAX_MODEL_SCALE {
        return Err("Model scale must be greater than zero and at most 16".into());
    }
    Ok(())
}

impl ModelDisplayEntity {
    pub fn spawn(
        world: &Arc<World>,
        name: &str,
        position: Vector3<f64>,
        scale: f32,
    ) -> Result<Arc<Self>, String> {
        validate_position(world, position)?;
        validate_scale(scale)?;
        let model =
            model_mapping(world, name).ok_or_else(|| format!("Unknown shared model: {name}"))?;
        let display = ItemDisplayEntity::new(Entity::new(
            world.clone(),
            position,
            &EntityType::ITEM_DISPLAY,
        ));
        let mut item = ItemStack::new(1, &Item::PAPER);
        item.set_data_component(ItemModelImpl {
            id: Cow::Owned(model.java.clone()),
        });
        item.set_custom_data(
            MODEL_NAMESPACE,
            MODEL_KEY,
            NbtTag::String(model.java.clone().into()),
        );
        display.set_item(item);
        let model = Arc::new(Self {
            display,
            model: Some(model),
        });
        model.configure_static_actor();
        model.apply_scale(scale);
        if !world.spawn_entity(model.clone()) {
            return Err("Model spawn was cancelled".into());
        }
        Ok(model)
    }

    pub(crate) fn from_saved(entity: Entity, nbt: &NbtCompound) -> Arc<Self> {
        let display = ItemDisplayEntity::new(entity);
        display.read_custom_nbt(nbt);
        let item = display.get_item();
        let model = item
            .get_custom_data(MODEL_NAMESPACE, MODEL_KEY)
            .as_ref()
            .and_then(NbtTag::extract_string)
            .filter(|name| {
                item.get_data_component::<ItemModelImpl>()
                    .is_some_and(|component| component.id == *name)
            })
            .and_then(|name| model_mapping(&display.display.entity.world.load(), name));
        let scale = display.display.get_scale();
        let valid_transform = scale.x == scale.y
            && scale.y == scale.z
            && validate_scale(scale.x).is_ok()
            && display.display.get_translation() == Vector3::new(0.0, scale.x * 0.5, 0.0)
            && display.display.get_left_rotation() == [0.0, 0.0, 0.0, 1.0]
            && display.display.get_right_rotation() == [0.0, 1.0, 0.0, 0.0]
            && display.get_item_display_mode() == 0;
        let model = Arc::new(Self {
            display,
            model: model.filter(|_| valid_transform),
        });
        if model.model.is_none() {
            tracing::warn!(entity=%model.get_entity().entity_uuid, "Shared model unavailable; retaining saved data");
        }
        if model.model.is_some() {
            model.configure_static_actor();
        }
        model
    }

    #[must_use]
    pub fn model_name(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.java.as_str())
    }

    #[must_use]
    pub fn scale(&self) -> f32 {
        self.display.display.get_scale().x
    }

    pub fn set_scale(&self, scale: f32) -> Result<(), String> {
        if self.get_entity().is_removed() {
            return Err("Model was removed".into());
        }
        validate_scale(scale)?;
        if self.model.is_none() {
            return Err("Shared model is unavailable".into());
        }
        self.apply_scale(scale);
        self.get_entity()
            .send_bedrock_actor_data(&self.native_metadata());
        Ok(())
    }

    fn apply_scale(&self, scale: f32) {
        self.display
            .display
            .set_scale(Vector3::new(scale, scale, scale));
        // Item models are centered by the renderer; native geometry uses feet Y=0.
        self.display
            .display
            .set_translation(Vector3::new(0.0, scale * 0.5, 0.0));
        // DisplayRenderer adds a half-turn for items; retain the authored model basis.
        self.display
            .display
            .set_right_rotation([0.0, 1.0, 0.0, 0.0]);
    }

    fn configure_static_actor(&self) {
        let entity = self.get_entity();
        entity.set_has_no_gravity(true);
        entity.bedrock_metadata();
        entity.bedrock_flags.fetch_and(
            !((1i64 << entity_data_flag::HAS_GRAVITY) | (1i64 << entity_data_flag::HAS_COLLISION)),
            Ordering::Relaxed,
        );
        entity
            .bedrock_flags
            .fetch_or(1i64 << entity_data_flag::NO_AI, Ordering::Relaxed);
    }

    fn native_metadata(&self) -> SyncedActorDataList {
        let mut metadata = self.get_entity().bedrock_metadata();
        metadata.set(entity_data_key::SCALE, MetadataValue::Float(self.scale()));
        metadata
    }

    pub fn set_position(&self, position: Vector3<f64>) -> Result<(), String> {
        if self.get_entity().is_removed() {
            return Err("Model was removed".into());
        }
        let world = self.get_entity().world.load_full();
        validate_position(&world, position)?;
        self.get_entity().teleport(position, None, None, &world);
        if let Some(tracked) = world
            .entity_tracker
            .get_tracked_entity(self.get_entity().entity_id)
        {
            let packet = CMoveActorAbsolute {
                actor_runtime_id: VarULong(self.get_entity().entity_id as u64),
                header: CMoveActorAbsolute::FLAG_TELEPORT,
                position: position.to_f32_lossy(),
                rotation_x: pumpkin_util::math::pack_degrees(self.get_entity().pitch.load()),
                rotation_y: pumpkin_util::math::pack_degrees(self.get_entity().yaw.load()),
                rotation_y_head: pumpkin_util::math::pack_degrees(
                    self.get_entity().head_yaw.load(),
                ),
            };
            tracked.send_to_tracking_players_bedrock(&packet, &world);
        }
        Ok(())
    }
}

impl EntityBase for ModelDisplayEntity {
    fn has_client_representation(&self) -> bool {
        self.model.is_some()
    }
    fn get_entity(&self) -> &Entity {
        self.display.get_entity()
    }
    fn get_living_entity(&self) -> Option<&LivingEntity> {
        None
    }
    fn cast_any(&self) -> &dyn std::any::Any {
        self
    }
    fn tick(&self, _caller: &dyn EntityBase, _server: &Server) {}
    fn init_data_tracker(&self) {
        self.display.init_data_tracker();
    }
    fn is_pushable(&self) -> bool {
        false
    }
    fn is_pushed_by_fluids(&self) -> bool {
        false
    }
    fn can_hit(&self) -> bool {
        false
    }
    fn is_immune_to_explosion(&self) -> bool {
        true
    }
    fn damage_with_context(
        &self,
        _caller: &dyn EntityBase,
        _amount: f32,
        _damage_type: DamageType,
        _position: Option<Vector3<f64>>,
        _source: Option<&dyn EntityBase>,
        _cause: Option<&dyn EntityBase>,
    ) -> bool {
        false
    }

    fn bedrock_identifier(&self) -> &str {
        self.model
            .as_ref()
            .map_or("minecraft:armor_stand", |model| model.bedrock.as_str())
    }
    fn entity_bedrock_spawn_metadata(&self) -> Option<SyncedActorDataList> {
        Some(self.native_metadata())
    }

    fn teleport(
        &self,
        position: Vector3<f64>,
        _yaw: Option<f32>,
        _pitch: Option<f32>,
        world: Arc<World>,
    ) {
        if !Arc::ptr_eq(&world, &self.get_entity().world.load_full()) {
            tracing::warn!("Shared model dimension transfer is unsupported");
            return;
        }
        if let Err(error) = self.set_position(position) {
            tracing::warn!(%error, "Model teleport rejected");
        }
    }

    fn write_custom_nbt(&self, nbt: &mut NbtCompound) {
        self.display.write_custom_nbt(nbt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stored_in_the_standard_item_component() {
        let mut item = ItemStack::new(1, &Item::PAPER);
        item.set_data_component(ItemModelImpl {
            id: Cow::Borrowed("sculith:statue"),
        });
        item.set_custom_data(
            MODEL_NAMESPACE,
            MODEL_KEY,
            NbtTag::String("sculith:statue".into()),
        );
        item.set_custom_data(
            "other",
            "opaque",
            NbtTag::ByteArray(vec![-128, 0, 127].into()),
        );
        let mut stack = NbtCompound::new();
        item.write_item_stack(&mut stack);
        let mut entity = NbtCompound::new();
        entity.put_compound("item", stack.clone());
        assert!(is_saved_model(&entity));
        assert_eq!(
            entity.child_tags.len(),
            1,
            "identity must not depend on non-vanilla entity tags"
        );
        let restored = ItemStack::read_item_stack(&stack).unwrap();
        assert_eq!(
            restored.get_data_component::<ItemModelImpl>().unwrap().id,
            "sculith:statue"
        );
        assert_eq!(
            restored.get_custom_data(MODEL_NAMESPACE, MODEL_KEY),
            Some(NbtTag::String("sculith:statue".into()))
        );
        assert_eq!(
            restored.get_custom_data("other", "opaque"),
            Some(NbtTag::ByteArray(vec![-128, 0, 127].into()))
        );
    }

    #[test]
    fn item_model_without_shared_identity_remains_a_java_display() {
        let mut item = ItemStack::new(1, &Item::PAPER);
        item.set_data_component(ItemModelImpl {
            id: Cow::Borrowed("sculith:statue"),
        });
        let mut stack = NbtCompound::new();
        item.write_item_stack(&mut stack);
        let mut entity = NbtCompound::new();
        entity.put_compound("item", stack);
        assert!(!is_saved_model(&entity));
    }

    #[test]
    fn scale_rejects_nonfinite_and_out_of_range_values() {
        for scale in [
            0.0,
            -1.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            MAX_MODEL_SCALE + 1.0,
        ] {
            assert!(validate_scale(scale).is_err());
        }
        for scale in [f32::EPSILON, 1.0, MAX_MODEL_SCALE] {
            assert!(validate_scale(scale).is_ok());
        }
    }
}
