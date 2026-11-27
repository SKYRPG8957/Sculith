use crate::entity::player::advancement::trigger::AdvancementTrigger;
use crate::entity::player::statistics::StatisticCategory;
use crate::server::Server;
use crate::world::World;
use core::f32;
use pumpkin_data::damage::DamageType;
use pumpkin_data::data_component_impl::{BundleContentsImpl, ContainerImpl};
use pumpkin_data::entity::EntityType;
use pumpkin_data::game_event::GameEvent;
use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::tag::Taggable;
use pumpkin_nbt::compound::NbtCompound;
use sculith_protocol::bedrock::client::CAddItemActor;
use sculith_protocol::bedrock::network_item::ItemStackWrapper;
use sculith_protocol::bedrock::server::actor_event::{ActorEventID, SActorEvent};
use sculith_protocol::codec::item_stack_seralizer::ItemStackSerializer;
use sculith_protocol::codec::var_int::VarInt;
use sculith_protocol::codec::var_long::VarLong;
use sculith_protocol::codec::var_ulong::VarULong;
use sculith_protocol::java::client::play::{CSetEntityMetadata, Metadata};
use sculith_util::math::atomic_f32::AtomicF32;
use sculith_util::math::position::BlockPos;
use sculith_util::math::vector3::Vector3;
use std::sync::atomic::Ordering::{AcqRel, Relaxed};

use std::sync::{
    Arc, Mutex,
    atomic::{
        AtomicBool, AtomicU8, AtomicU32,
        Ordering::{self},
    },
};

use super::{
    Entity, EntityBase,
    drop_motion::{ActorClock, ActorTickOwner},
    living::{LivingEntity, RegionalActorDelivery},
    player::Player,
};
use crate::block::blocks::bubble_column::SURFACE_UPWARD_ACCELERATION;
use crate::plugin::api::events::entity::{
    item_despawn::ItemDespawnEvent, item_merge::ItemMergeEvent,
};
use crate::server::TickDeadline;
use crate::world::RegionId;
use crate::world::entity_islands::{self, AuditedReach};

pub(crate) mod storage;

#[derive(Clone, Copy)]
pub(crate) enum ItemTimerStep {
    World(Option<TickDeadline>),
    Region,
}

pub(crate) enum RegionalItemStep {
    Skipped,
    Advanced,
    Expired,
}

pub struct ItemEntity {
    entity: Entity,
    item_age: AtomicU32,
    clock: Mutex<ActorClock>,
    thrower: crossbeam::atomic::AtomicCell<Option<uuid::Uuid>>,
    // These cannot be atomic values because we mutate their state based on what they are; we run
    // into the ABA problem
    item_stack: Mutex<ItemStack>,
    pickup_delay: AtomicU8,
    health: AtomicF32,
    never_despawn: AtomicBool,
    never_pickup: AtomicBool,
    merge_reserved: AtomicBool,
    saved_records: Mutex<storage::SavedItemHistory>,
}

struct ItemMergeReservation<'a> {
    low: &'a AtomicBool,
    high: &'a AtomicBool,
}

impl<'a> ItemMergeReservation<'a> {
    fn acquire(low: &'a ItemEntity, high: &'a ItemEntity) -> Option<Self> {
        if std::ptr::eq(low, high)
            || low
                .merge_reserved
                .compare_exchange(false, true, AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }

        if high
            .merge_reserved
            .compare_exchange(false, true, AcqRel, Ordering::Acquire)
            .is_err()
        {
            low.merge_reserved.store(false, Ordering::Release);
            return None;
        }

        Some(Self {
            low: &low.merge_reserved,
            high: &high.merge_reserved,
        })
    }
}

impl Drop for ItemMergeReservation<'_> {
    fn drop(&mut self) {
        self.high.store(false, Ordering::Release);
        self.low.store(false, Ordering::Release);
    }
}

/// Vanilla `ItemEntity.LIFETIME`.
const LIFETIME: u32 = 6000; // 5 minutes in ticks
/// Vanilla `ItemEntity.merge(to, from, 64)`: cap on top of `getMaxStackSize`.
const MERGE_MAX_COUNT: u8 = 64;

/// Horizontal search distance of `mergeWithNeighbours`.
const MERGE_DISTANCE: f64 = 0.5;

impl ItemEntity {
    pub const DEFAULT_PICKUP_DELAY: u8 = 10;
    pub(super) const AIR_DRAG: f64 = 0.98;

    pub fn new(entity: Entity, item_stack: ItemStack) -> Self {
        entity.velocity.store(Vector3::new(
            rand::random::<f64>().mul_add(0.2, -0.1),
            0.2,
            rand::random::<f64>().mul_add(0.2, -0.1),
        ));
        entity.yaw.store(rand::random::<f32>() * 360.0);
        Self::update_fire_immune(&entity, &item_stack);

        Self {
            entity,
            item_stack: Mutex::new(item_stack),
            item_age: AtomicU32::new(0),
            clock: Mutex::default(),
            thrower: crossbeam::atomic::AtomicCell::new(None),
            pickup_delay: AtomicU8::new(Self::DEFAULT_PICKUP_DELAY),
            health: AtomicF32::new(5.0),
            never_despawn: AtomicBool::new(false),
            never_pickup: AtomicBool::new(false),
            merge_reserved: AtomicBool::new(false),
            saved_records: Mutex::default(),
        }
    }

    pub fn new_with_velocity(
        entity: Entity,
        item_stack: ItemStack,
        velocity: Vector3<f64>,
        pickup_delay: u8,
    ) -> Self {
        entity.velocity.store(velocity);
        entity.yaw.store(rand::random::<f32>() * 360.0);
        Self::update_fire_immune(&entity, &item_stack);

        Self {
            entity,
            item_stack: Mutex::new(item_stack),
            item_age: AtomicU32::new(0),
            clock: Mutex::default(),
            thrower: crossbeam::atomic::AtomicCell::new(None),
            pickup_delay: AtomicU8::new(pickup_delay), // Vanilla pickup delay is 10 ticks
            health: AtomicF32::new(5.0),
            never_despawn: AtomicBool::new(false),
            never_pickup: AtomicBool::new(false),
            merge_reserved: AtomicBool::new(false),
            saved_records: Mutex::default(),
        }
    }

    /// Creates an `ItemEntity` for restoring from NBT without random velocity.
    /// The velocity and position will be set by `Entity::read_nbt_non_mut`.
    pub fn new_empty(entity: Entity) -> Self {
        Self {
            entity,
            item_stack: Mutex::new(ItemStack::new(1, &pumpkin_data::item::Item::AIR)),
            item_age: AtomicU32::new(0),
            clock: Mutex::default(),
            thrower: crossbeam::atomic::AtomicCell::new(None),
            pickup_delay: AtomicU8::new(0),
            health: AtomicF32::new(5.0),
            never_despawn: AtomicBool::new(false),
            never_pickup: AtomicBool::new(false),
            merge_reserved: AtomicBool::new(false),
            saved_records: Mutex::default(),
        }
    }

    pub const fn get_item_stack(&self) -> &Mutex<ItemStack> {
        &self.item_stack
    }

    pub fn set_thrower(&self, uuid: uuid::Uuid) {
        self.thrower.store(Some(uuid));
    }

    pub fn get_thrower(&self) -> Option<uuid::Uuid> {
        self.thrower.load()
    }

    pub fn get_pickup_delay(&self) -> u8 {
        self.pickup_delay.load(Ordering::Relaxed)
    }

    pub fn set_pickup_delay(&self, pickup_delay: u8) {
        self.pickup_delay.store(pickup_delay, Ordering::Relaxed);
    }

    pub const fn get_entity(&self) -> &Entity {
        &self.entity
    }

    /// Vanilla `ItemEntity.fireImmune`, cached since the item only changes on load.
    fn update_fire_immune(entity: &Entity, stack: &ItemStack) {
        entity.fire_immune.store(
            !stack.can_be_hurt_by(DamageType::IN_FIRE),
            Ordering::Relaxed,
        );
    }

    /// Vanilla `ItemEntity.areMergable`.
    #[must_use]
    pub fn are_mergeable(stack: &ItemStack, other: &ItemStack) -> bool {
        u16::from(stack.item_count) + u16::from(other.item_count)
            <= u16::from(other.get_max_stack_size())
            && stack.are_items_and_components_equal(other)
    }

    /// Vanilla `ItemEntity.isMergable`.
    fn is_mergeable(&self) -> bool {
        if self.entity.removed.load(Ordering::SeqCst)
            || self.never_pickup.load(Ordering::Relaxed)
            || self.never_despawn.load(Ordering::Relaxed)
            || self.item_age.load(Ordering::Relaxed) >= LIFETIME
        {
            return false;
        }
        let Ok(item_stack) = self.item_stack.try_lock() else {
            return false;
        };

        item_stack.item_count < item_stack.get_max_stack_size()
    }

    /// Vanilla `ItemEntity.mergeWithNeighbours`.
    fn merge_with_neighbours(&self) {
        if !self.is_mergeable() {
            return;
        }

        let bounding_box =
            self.entity
                .bounding_box
                .load()
                .expand(MERGE_DISTANCE, 0.0, MERGE_DISTANCE);
        let world = self.entity.world.load();
        for other in world.get_entities_at_box(&bounding_box) {
            let Some(item) = other.get_item_entity() else {
                continue;
            };
            if item.entity.entity_id == self.entity.entity_id || !item.is_mergeable() {
                continue;
            }

            self.try_merge_with(item);
            if self.entity.removed.load(Ordering::SeqCst) {
                break;
            }
        }
    }

    #[expect(clippy::too_many_lines)]
    fn try_merge_with(&self, other: &Self) {
        let _motion = other.entity.hold_actor_motion();
        if !self
            .entity
            .bounding_box
            .load()
            .expand(MERGE_DISTANCE, 0.0, MERGE_DISTANCE)
            .intersects(&other.entity.bounding_box.load())
        {
            return;
        }
        // Always lock in entity_id order to prevent deadlock when two
        // items try to merge with each other concurrently.
        let (low, high) = if self.entity.entity_id < other.entity.entity_id {
            (self, other)
        } else {
            (other, self)
        };
        let Some(_reservation) = ItemMergeReservation::acquire(low, high) else {
            return;
        };

        let (expected_low, expected_high, target_is_self) = {
            let low_stack = low
                .item_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let high_stack = high
                .item_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (self_stack, other_stack) = if self.entity.entity_id < other.entity.entity_id {
                (&*low_stack, &*high_stack)
            } else {
                (&*high_stack, &*low_stack)
            };

            if !Self::are_mergeable(self_stack, other_stack) {
                return;
            }

            (
                low_stack.clone(),
                high_stack.clone(),
                other_stack.item_count < self_stack.item_count,
            )
        };
        let (target, source) = if target_is_self {
            (self, other)
        } else {
            (other, self)
        };
        if self.entity.removed.load(Ordering::SeqCst) || other.entity.removed.load(Ordering::SeqCst)
        {
            return;
        }

        let mut event = crate::plugin::api::events::entity::item_merge::ItemMergeEvent {
            entity_id: target.entity.entity_id,
            target_id: source.entity.entity_id,
            cancelled: false,
        };
        let server = self.entity.world.load().server.upgrade();
        if let Some(server) = server {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled
            || self.entity.removed.load(Ordering::SeqCst)
            || other.entity.removed.load(Ordering::SeqCst)
        {
            return;
        }

        let low_stack = low
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let high_stack = high
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if low_stack.uid != expected_low.uid
            || !low_stack.are_equal(&expected_low)
            || high_stack.uid != expected_high.uid
            || !high_stack.are_equal(&expected_high)
            || self.entity.removed.load(Ordering::SeqCst)
            || other.entity.removed.load(Ordering::SeqCst)
        {
            return;
        }

        let (self_stack, other_stack) = if self.entity.entity_id < other.entity.entity_id {
            (low_stack, high_stack)
        } else {
            (high_stack, low_stack)
        };
        if !Self::are_mergeable(&self_stack, &other_stack)
            || (other_stack.item_count < self_stack.item_count) != target_is_self
        {
            return;
        }
        let (mut stack1, mut stack2) = if target_is_self {
            (self_stack, other_stack)
        } else {
            (other_stack, self_stack)
        };

        // Vanilla `ItemEntity.merge(to, from, 64)`.
        let max_size = stack1.get_max_stack_size().min(MERGE_MAX_COUNT);
        let moved = stack2
            .item_count
            .min(max_size.saturating_sub(stack1.item_count));
        stack1.increment(moved);
        stack2.decrement(moved);
        let source_empty = stack2.is_empty();
        if source_empty {
            target.inherit_saved_item_records(source, stack1.item_count - moved, moved);
        } else if moved != 0 {
            target.mark_partial_item_transfer();
            source.mark_partial_item_transfer();
        }
        drop(stack1);
        drop(stack2);

        // `is_mergeable` already excluded never-despawn and never-pickup items.
        {
            let _low_clock = low
                .clock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _high_clock = high
                .clock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            target.pickup_delay.fetch_max(
                source.pickup_delay.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            target
                .item_age
                .fetch_min(source.item_age.load(Ordering::Relaxed), Ordering::Relaxed)
        };

        target.on_count_changed();
        if source_empty {
            source.entity.remove();
        } else {
            source.on_count_changed();
        }
    }

    /// Vanilla `Item.onDestroyed`: containers (`BlockItem`, e.g. shulker boxes and bundles
    /// spill their contents where the item died, whatever the damage type was).
    fn on_destroyed(&self, world: &Arc<World>) {
        let contents = {
            let mut stack = self
                .item_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut contents = Vec::new();
            if let Some(container) = stack.get_data_component::<ContainerImpl>()
                && !container.items.is_empty()
            {
                contents.extend(container.items.iter().map(|(_, item)| item.clone()));
                stack.set_data_component(ContainerImpl { items: Vec::new() });
            }
            if let Some(bundle) = stack.get_data_component::<BundleContentsImpl>()
                && !bundle.items.is_empty()
            {
                contents.extend(bundle.items.iter().cloned());
                stack.set_data_component(BundleContentsImpl::EMPTY);
            }
            contents
        };

        // no pickup delay.
        let pos = self.entity.pos.load();
        for item in contents.into_iter().filter(|item| !item.is_empty()) {
            let spilled = Self::new(Entity::new(world.clone(), pos, &EntityType::ITEM), item);
            spilled.set_pickup_delay(0);
            world.spawn_entity(Arc::new(spilled));
        }
    }

    /// Resends the stack after its count changed. Bedrock item actors ignore the `ITEM`
    /// metadata, so they get the count as `UpdateStackSize`
    fn on_count_changed(&self) {
        self.init_data_tracker();
        self.send_current_bedrock_stack_size();
    }

    pub(crate) fn send_current_bedrock_stack_size(&self) {
        let count = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .item_count;
        self.entity.world.load().send_to_tracking_players_bedrock(
            &self.entity,
            &Self::bedrock_stack_size_update(self.entity.entity_id, count),
        );
    }

    pub(crate) fn bedrock_stack_size_update(entity_id: i32, count: u8) -> SActorEvent {
        SActorEvent {
            target_runtime_id: VarULong(entity_id as u64),
            event_id: ActorEventID::UpdateStackSize,
            data: VarInt(i32::from(count)),
            fire_at_position: None,
        }
    }

    /// Updates the remainder metadata under the same publication lock as the packet batch.
    pub(crate) fn try_publish_pickup_remainder(
        &self,
        stack: ItemStack,
        publish: impl FnOnce(Option<Box<[u8]>>) -> Option<RegionalActorDelivery>,
    ) -> Option<RegionalActorDelivery> {
        let mut sent = None;
        self.entity.synched_data.try_publish_value(
            pumpkin_data::tracked_data::item::ITEM,
            ItemStackSerializer::from(stack),
            |metadata| {
                let delivery = publish(metadata)?;
                let java_queued = delivery.java_queued;
                sent = Some(delivery);
                Some(java_queued)
            },
        );
        sent
    }

    fn collect_for_player(&self, player: &Arc<Player>) -> Option<(&'static Item, u8, bool)> {
        let world = self.entity.world.load_full();
        let eligible = || {
            self.can_be_picked_up()
                && Arc::ptr_eq(&world, &self.entity.world.load())
                && player.can_simulate_in(&world)
                && player.has_stable_world_membership()
                && !player.living_entity.is_dead_or_dying()
                && !player.is_spectator()
                && crate::world::pickup_area(player).intersects(&self.entity.bounding_box.load())
        };
        if !eligible() {
            return None;
        }
        let pickup = {
            let stack = self
                .item_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            player.inventory.plan_pickup(&stack)?
        };
        let item = pickup.item();
        let count = pickup.count();
        if !player
            .living_entity
            .authorize_pickup(|| item.registry_key.to_string(), count)
        {
            return None;
        }

        let server = world.server.upgrade()?;
        let _checkpoint = server.try_entity_save_guard()?;
        let _snapshot = world.try_entity_snapshot()?;
        let _position = player
            .client
            .bedrock()
            .filter(|client| client.is_current_protocol())
            .map(|client| client.regional_work().try_lock())
            .transpose()
            .ok()?;
        let _inventory = player.hold_bedrock_inventory_edit();
        if !eligible() {
            return None;
        }
        let mut stack = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stack_count = stack.item_count;
        let remainder_count = stack_count.checked_sub(count)?;
        let remainder = if remainder_count == 0 {
            None
        } else {
            self.try_checkpoint_remainder(&world, &stack, remainder_count)
                .ok()
                .flatten()
        };
        let mut checkpoint = None;
        if !player.inventory.commit_pickup(pickup, &mut stack, || {
            if remainder_count == 0 || remainder.is_some() {
                let Ok(permit) = server.prepare_item_pickup(player, self, stack_count, remainder)
                else {
                    return false;
                };
                checkpoint = permit;
            }
            true
        }) {
            return None;
        }
        let empty = stack.is_empty();
        if let Some(checkpoint) = checkpoint {
            checkpoint.record(self);
        } else if !empty {
            self.mark_partial_item_transfer();
        }
        Some((item, count, empty))
    }

    fn can_be_picked_up(&self) -> bool {
        !self.entity.is_removed()
            && !self.never_pickup.load(Relaxed)
            && self.pickup_delay.load(Relaxed) == 0
    }

    fn write_item_nbt(&self, nbt: &mut NbtCompound, item: &ItemStack) {
        let mut item_compound = NbtCompound::new();
        item.write_item_stack(&mut item_compound);
        nbt.put_compound("Item", item_compound);

        let _clock = self
            .clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        nbt.put_short("Age", self.item_age.load(Ordering::Relaxed) as i16);
        nbt.put_short(
            "PickupDelay",
            self.pickup_delay.load(Ordering::Relaxed) as i16,
        );
        nbt.put_short("Health", self.health.load(Relaxed) as i16);
        if let Some(thrower) = self.thrower.load() {
            nbt.put_uuid("Thrower", thrower);
        }
    }

    pub(crate) fn can_be_picked_up_on_region(&self) -> bool {
        self.can_be_picked_up() && !self.has_expired() && self.thrower.load().is_none()
    }

    fn decrement_pickup_delay(&self) {
        self.pickup_delay
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |val| {
                Some(val.saturating_sub(1))
            })
            .ok();
    }

    pub(crate) fn timers_unobserved(server: &Server) -> bool {
        !server.plugin_manager.has_handlers::<ItemDespawnEvent>()
            && !server.plugin_manager.has_handlers::<ItemMergeEvent>()
    }

    /// The item starts taking timer steps on the deadline after its creation.
    pub(crate) fn start_regional_timer(&self, owner: RegionId, deadline: TickDeadline) {
        self.clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .claim(ActorTickOwner::Region(owner), deadline);
    }

    pub(crate) fn advance_regional_timers(
        &self,
        owner: RegionId,
        deadline: TickDeadline,
        still_owned: impl FnOnce() -> bool,
    ) -> RegionalItemStep {
        let Ok(stack) = self.item_stack.try_lock() else {
            return RegionalItemStep::Skipped;
        };
        let Ok(mut clock) = self.clock.try_lock() else {
            return RegionalItemStep::Skipped;
        };
        if stack.is_empty()
            || !still_owned()
            || !clock.claim(ActorTickOwner::Region(owner), deadline)
        {
            return RegionalItemStep::Skipped;
        }

        self.decrement_pickup_delay();
        if !self.never_despawn.load(Relaxed) {
            let age = self.item_age.load(Relaxed);
            if age < LIFETIME {
                self.item_age.store(age + 1, Relaxed);
            }
            if age >= LIFETIME - 1 {
                return RegionalItemStep::Expired;
            }
        }
        RegionalItemStep::Advanced
    }

    pub(crate) fn has_expired(&self) -> bool {
        !self.never_despawn.load(Relaxed) && self.item_age.load(Relaxed) >= LIFETIME
    }

    pub(crate) fn tick_from_world(
        &self,
        caller: &dyn EntityBase,
        server: &Server,
        time: ItemTimerStep,
        deadline: Option<TickDeadline>,
    ) {
        let (_permit, advance_motion) = self.entity.begin_world_motion(deadline);
        if self.entity.is_removed() {
            return;
        }
        if advance_motion {
            let entity = &self.entity;
            entity.tick_count.fetch_add(1, Relaxed);
            if entity.has_vehicle() {
                entity.velocity.store(Vector3::default());
            }
            self.tick_with_time(caller, server, time);
        } else if !self.entity.is_removed() && !self.remove_empty_stack() {
            self.process_age_and_merge(self.tick_timers(time));
        }
    }

    fn remove_empty_stack(&self) -> bool {
        let empty = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        if empty {
            self.entity.remove();
        }
        empty
    }

    fn tick_with_time(&self, caller: &dyn EntityBase, server: &Server, time: ItemTimerStep) {
        let entity = &self.entity;
        if self.remove_empty_stack() {
            return;
        }

        // Vanilla `super.tick()`: `xo/yo/zo`, fluids, fire, portals and the void.
        entity.tick(caller, server);
        if entity.removed.load(Ordering::SeqCst) {
            return;
        }

        let advance_time = self.tick_timers(time);

        let original_velo = entity.velocity.load();
        entity
            .velocity
            .store(self.apply_fluid_drag_or_gravity(original_velo));
        self.update_no_physics_and_push_out();

        let move_velo = entity.velocity.load(); // In case push_out_of_blocks modifies it
        if self.should_tick_move(move_velo, entity.tick_count.load(Relaxed)) {
            self.move_and_apply_friction(caller, move_velo);
        }

        if self.process_age_and_merge(advance_time) {
            self.mark_needs_sync(caller, original_velo);
        }
    }

    fn tick_timers(&self, time: ItemTimerStep) -> bool {
        let advance_time = match time {
            ItemTimerStep::Region => false,
            ItemTimerStep::World(deadline) => {
                let mut clock = self
                    .clock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(deadline) = deadline {
                    clock.claim(ActorTickOwner::World, deadline)
                } else {
                    *clock = ActorClock::default();
                    true
                }
            }
        };
        if advance_time {
            self.decrement_pickup_delay();
        }
        advance_time
    }

    pub(super) fn apply_fluid_drag_or_gravity(&self, mut velo: Vector3<f64>) -> Vector3<f64> {
        let entity = &self.entity;

        if entity.touching_water.load(Ordering::SeqCst) && entity.water_height.load() > 0.1 {
            velo.x *= 0.99;
            velo.z *= 0.99;
            if velo.y < 0.06 {
                velo.y += 5.0e-4;
            }
        } else if entity.touching_lava.load(Ordering::SeqCst) && entity.lava_height.load() > 0.1 {
            velo.x *= 0.95;
            velo.z *= 0.95;
            if velo.y < 0.06 {
                velo.y += 5.0e-4;
            }
        } else {
            velo.y -= <Self as EntityBase>::get_gravity(self);
        }

        velo
    }

    fn update_no_physics_and_push_out(&self) {
        let entity = &self.entity;
        let pos = entity.pos.load();
        let bounding_box = entity.bounding_box.load();

        let no_physics = !entity
            .world
            .load()
            .is_space_empty(bounding_box.expand(-1.0e-7, -1.0e-7, -1.0e-7));

        entity.no_physics.store(no_physics, Ordering::Relaxed);

        if no_physics {
            entity.push_out_of_blocks(Vector3::new(
                pos.x,
                f64::midpoint(bounding_box.min.y, bounding_box.max.y),
                pos.z,
            ));
        }
    }

    /// a resting item only moves every 4th `tickCount`, offset by its id.
    pub(super) fn should_tick_move(&self, move_velo: Vector3<f64>, tick_count: i32) -> bool {
        let entity = &self.entity;

        !entity.on_ground.load(Ordering::SeqCst)
            || move_velo.horizontal_length_squared() > 1.0e-5
            || tick_count.wrapping_add(entity.entity_id) % 4 == 0
    }

    fn move_and_apply_friction(&self, caller: &dyn EntityBase, move_velo: Vector3<f64>) {
        let entity = &self.entity;

        entity.move_entity(caller, move_velo);
        entity.tick_block_collisions(caller);

        let on_ground = entity.on_ground.load(Ordering::SeqCst);
        let velocity = entity.velocity.load();
        let slipperiness =
            on_ground.then(|| entity.get_block_with_y_offset(0.999_999).1.slipperiness);
        entity
            .velocity
            .store(Self::velocity_after_motion(velocity, slipperiness));
    }

    pub(super) fn velocity_after_motion(
        velocity: Vector3<f64>,
        slipperiness: Option<f32>,
    ) -> Vector3<f64> {
        let air_drag = Self::AIR_DRAG;
        let friction = air_drag * slipperiness.map_or(1.0, f64::from);
        let mut velocity = velocity.multiply(friction, air_drag, friction);

        // a landing item bounces back up with half its speed
        if slipperiness.is_some() && velocity.y < 0.0 {
            velocity.y *= -0.5;
        }
        velocity
    }

    fn process_age_and_merge(&self, advance_time: bool) -> bool {
        if self.never_despawn.load(Ordering::Relaxed) {
            return true;
        }

        let entity = &self.entity;
        let age = if advance_time {
            self.item_age.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.item_age.load(Relaxed)
        };

        if age >= LIFETIME {
            let entity_id = entity.entity_id;
            let world = entity.world.load_full();
            let mut despawn_event = ItemDespawnEvent::new(entity_id);
            if let Some(server) = world.server.upgrade() {
                server
                    .plugin_manager
                    .fire_blocking(&server, &mut despawn_event);
            }
            if !despawn_event.cancelled
                && let Some(e) = world.get_entity_by_id(entity_id)
            {
                e.get_entity().remove();
            }
            return false;
        }

        // merge rate on `tickCount`: 2 while the item changes block cell, else 40.
        let moved =
            BlockPos::floored_v(entity.last_pos.load()) != BlockPos::floored_v(entity.pos.load());
        let rate = if moved { 2 } else { 40 };
        if entity.tick_count.load(Ordering::Relaxed) % rate == 0 {
            self.merge_with_neighbours();
        }

        true
    }

    /// Vanilla `ItemEntity.tick` `needsSync`: fluid contact or velocity change.
    fn mark_needs_sync(&self, caller: &dyn EntityBase, original_velo: Vector3<f64>) {
        let entity = &self.entity;

        // Vanilla runs `updateFluidInteraction` again here, but the fluid damage only
        // comes from the block effects, so it must not be applied a second time.
        let in_fluid = !entity.update_fluid_interaction(caller).is_empty();

        self.mark_velocity_change(original_velo, in_fluid);
    }

    pub(super) fn mark_velocity_change(&self, original: Vector3<f64>, in_fluid: bool) {
        let entity = &self.entity;
        if in_fluid || entity.velocity.load().sub(&original).length_squared() > 0.01 {
            entity.velocity_dirty.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn try_spawn_stack(&self) -> Option<ItemStack> {
        self.item_stack.try_lock().ok().map(|stack| stack.clone())
    }

    pub(crate) fn bedrock_item_spawn_packet(&self, stack: &ItemStack) -> CAddItemActor {
        let runtime_id = self.entity.entity_id as u64;
        CAddItemActor {
            target_actor_id: VarLong(runtime_id as i64),
            target_runtime_id: VarULong(runtime_id),
            item: ItemStackWrapper::from(stack),
            position: self.bedrock_pos().to_f32_lossy(),
            velocity: self.entity.velocity.load().to_f32_lossy(),
            entity_data: self.entity.bedrock_metadata(),
            is_from_fishing: false,
        }
    }

    pub(crate) fn java_item_spawn_metadata(&self, stack: ItemStack) -> Option<CSetEntityMetadata> {
        let metadata = Metadata::new(
            pumpkin_data::tracked_data::item::ITEM,
            ItemStackSerializer::from(stack),
        );
        let mut data = Vec::new();
        metadata
            .write(&mut data, &pumpkin_data::packet::CURRENT_MC_VERSION)
            .ok()?;
        data.push(255);
        Some(CSetEntityMetadata::new(
            self.entity.entity_id.into(),
            data.into(),
        ))
    }
}

impl EntityBase for ItemEntity {
    fn dampens_vibrations(&self) -> bool {
        self.item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .item
            .has_tag(&pumpkin_data::tag::Item::MINECRAFT_DAMPENS_VIBRATIONS)
    }

    /// Vanilla `ItemEntity.getOwner`: the thrower.
    fn get_owner_entity(&self) -> Option<Arc<dyn EntityBase>> {
        let uuid = self.thrower.load()?;
        let world = self.entity.world.load();
        world.get_entity_by_uuid(uuid).or_else(|| {
            world
                .get_player_by_uuid(uuid)
                .map(|player| player as Arc<dyn EntityBase>)
        })
    }

    fn audited_reach(&self) -> Option<AuditedReach> {
        let entity = &self.entity;
        let world = entity.world.load();
        let pushes =
            super::MAX_PUSH_OUT_SPEED + super::max_fluid_push_speed(world.dimension.fast_lava);
        let velocity = entity.velocity.load();
        let horizontal = velocity.x.hypot(velocity.z) + pushes;
        let vertical = velocity.y.abs() + pushes + self.get_gravity() + SURFACE_UPWARD_ACCELERATION;
        let mut links = entity_islands::links_of(self);
        // Entering water splashes, wherever the item ends up.
        let moved = entity
            .bounding_box
            .load()
            .expand(horizontal, vertical, horizontal);
        super::mob::game_event_listener_links(&world, false, &moved, &mut links);
        let mut reach = AuditedReach {
            bounding_box: entity.bounding_box.load(),
            motion: (horizontal, vertical),
            entities: MERGE_DISTANCE,
            players: 0.0,
            block_reads: 1.0,
            route: None,
            changes_blocks: false,
            global: false,
            links,
        };
        reach.scan_contact_blocks(&world);
        Some(reach)
    }

    fn tick(&self, caller: &dyn EntityBase, server: &Server) {
        let _motion = self.entity.hold_actor_motion();
        if !self.entity.is_removed() {
            self.tick_with_time(caller, server, ItemTimerStep::World(None));
        }
    }

    fn init_data_tracker(&self) {
        self.entity.set_synced_data(
            pumpkin_data::tracked_data::item::ITEM,
            ItemStackSerializer::from(
                self.item_stack
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
        );
    }

    fn damage_with_context(
        &self,
        _caller: &dyn EntityBase,
        amount: f32,
        damage_type: DamageType,
        _position: Option<Vector3<f64>>,
        _source: Option<&dyn EntityBase>,
        cause: Option<&dyn EntityBase>,
    ) -> bool {
        // Vanilla `ItemEntity.hurtServer`.
        let entity = &self.entity;
        let _motion = entity.hold_actor_motion();
        if entity.is_removed() || entity.is_invulnerable_to(&damage_type, cause) {
            return false;
        }
        let world = entity.world.load_full();
        if !world.level_info.load().game_rules.mob_griefing
            && cause.is_some_and(|cause| cause.get_mob().is_some())
        {
            return false;
        }
        // E.g. netherite ignores fire, the nether star explosions.
        let can_be_hurt = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .can_be_hurt_by(damage_type);
        if !can_be_hurt {
            return false;
        }

        let mut event = crate::plugin::api::events::entity::entity_damage::EntityDamageEvent::new(
            entity.entity_id,
            damage_type,
            amount,
        );
        if let Some(server) = world.server.upgrade() {
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        if event.cancelled {
            return false;
        }

        // Vanilla `markHurt`: resend the motion.
        entity.velocity_dirty.store(true, Ordering::SeqCst);

        // Vanilla keeps item health as an int: `(int)(health - damage)`.
        let destroyed = loop {
            let current = self.health.load(Relaxed);
            let new = (current - event.damage).trunc();
            if self
                .health
                .compare_exchange(current, new, AcqRel, Relaxed)
                .is_ok()
            {
                break current > 0.0 && new <= 0.0;
            }
        };

        world.emit_game_event(GameEvent::EntityDamage.name(), entity.pos.load());
        if destroyed {
            self.on_destroyed(&world);
            entity.remove();
        }
        true
    }

    fn receives_player_touch(&self) -> bool {
        true
    }

    fn on_player_collision(&self, player: &Arc<Player>) {
        let _motion = self.entity.hold_actor_motion();
        let Some((item, amount, empty)) = self.collect_for_player(player) else {
            return;
        };
        player.inventory_changed.store(true, Relaxed);
        if let Some(thrower) = self
            .thrower
            .load()
            .and_then(|uuid| self.entity.world.load().get_entity_by_uuid(uuid))
        {
            player.trigger_advancement(AdvancementTrigger::ThrownItemPickedUpByPlayer {
                thrower_type: format!(
                    "minecraft:{}",
                    thrower.get_entity().entity_type.resource_name
                ),
            });
        }
        player.increment_stat(
            StatisticCategory::PickedUp,
            i32::from(item.id),
            i32::from(amount),
        );
        player
            .living_entity
            .send_pickup(&self.entity, amount.into(), empty);
        player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send_content_updates();

        if empty {
            self.entity.remove();
        } else {
            self.on_count_changed();
        }
    }

    fn get_entity(&self) -> &Entity {
        &self.entity
    }

    fn get_living_entity(&self) -> Option<&LivingEntity> {
        None
    }

    fn get_item_entity(&self) -> Option<&ItemEntity> {
        Some(self)
    }

    fn get_gravity(&self) -> f64 {
        0.04
    }

    fn bedrock_y_offset(&self) -> f64 {
        0.125
    }

    fn write_custom_nbt(&self, nbt: &mut NbtCompound) {
        let item = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.write_item_nbt(nbt, &item);
    }

    fn read_custom_nbt(&self, nbt: &NbtCompound) {
        self.thrower.store(nbt.get_uuid("Thrower"));
        // Restore the item stack from the "Item" compound
        if let Some(item_compound) = nbt.get_compound("Item")
            && let Some(stack) = ItemStack::read_item_stack(item_compound)
        {
            // `new_empty` had no item yet to derive this from.
            Self::update_fire_immune(&self.entity, &stack);
            *self
                .item_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = stack;
        }

        let _clock = self
            .clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Vanilla stores Age as a short
        self.item_age
            .store(nbt.get_short("Age").unwrap_or(0) as u32, Ordering::Relaxed);

        // Vanilla stores PickupDelay as a short
        if let Some(delay) = nbt.get_short("PickupDelay") {
            self.pickup_delay.store(delay as u8, Ordering::Relaxed);
        }

        // Vanilla stores Health as a short
        if let Some(health) = nbt.get_short("Health") {
            self.health.store(health as f32, Relaxed);
        }
    }

    fn cast_any(&self) -> &dyn std::any::Any {
        self
    }

    fn send_bedrock_spawn_packet(&self, client: &crate::net::bedrock::BedrockClient) {
        let data = {
            let stack = self
                .item_stack
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            client
                .serialize_packet(&self.bedrock_item_spawn_packet(&stack))
                .ok()
        };
        if let Some(data) = data {
            client.try_enqueue_packet(data);
        }
    }

    fn send_java_spawn_packet(&self, client: &crate::net::java::JavaClient) {
        let spawn_packet = self.entity.create_spawn_packet();
        if let Ok(data) = client.serialize_packet(&spawn_packet) {
            client.try_enqueue_packet(data);
        }

        let stack = self
            .item_stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(packet) = self.java_item_spawn_metadata(stack)
            && let Ok(data) = client.serialize_packet(&packet)
        {
            client.try_enqueue_packet(data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ActorClock, ActorTickOwner, ItemEntity, TickDeadline};
    use pumpkin_data::data_component_impl::{CustomDataImpl, CustomNameImpl};
    use pumpkin_data::item::Item;
    use pumpkin_data::item_stack::ItemStack;
    use pumpkin_data::{Enchantment, damage::DamageType};
    use pumpkin_nbt::compound::NbtCompound;
    use sculith_util::text::TextComponent;
    use std::time::{Duration, Instant};

    #[test]
    fn item_clock_rejects_overlapping_deadlines_on_handoff() {
        let start = Instant::now();
        let period = Duration::from_millis(50);
        let step = |ms| TickDeadline {
            at: start + Duration::from_millis(ms),
            period,
        };
        let mut clock = ActorClock::default();
        let old = ActorTickOwner::Region(2);
        let new = ActorTickOwner::Region(4);

        assert!(clock.claim(ActorTickOwner::World, step(0)));
        assert!(!clock.claim(old, step(20)));
        assert!(clock.claim(old, step(70)));
        assert!(!clock.claim(old, step(70)));
        assert!(!clock.claim(new, step(100)));
        assert!(clock.claim(new, step(150)));
        assert!(!clock.claim(old, step(120)));
        assert!(!clock.claim(ActorTickOwner::World, step(180)));
        assert!(clock.claim(ActorTickOwner::World, step(200)));
    }

    #[test]
    fn different_counts_merge_up_to_max_stack() {
        let stone = |count| ItemStack::new(count, &Item::STONE);
        assert!(ItemEntity::are_mergeable(&stone(1), &stone(5)));
        assert!(ItemEntity::are_mergeable(&stone(32), &stone(32)));
        assert!(!ItemEntity::are_mergeable(&stone(33), &stone(32)));
    }

    #[test]
    fn unstackable_items_never_merge() {
        let sword = ItemStack::new(1, &Item::DIAMOND_SWORD);
        assert!(!ItemEntity::are_mergeable(&sword, &sword.clone()));
    }

    #[test]
    fn different_components_never_merge() {
        let plain = ItemStack::new(1, &Item::STONE);

        let mut named = plain.clone();
        named.set_data_component(CustomNameImpl {
            name: TextComponent::text("a"),
        });
        let mut renamed = plain.clone();
        renamed.set_data_component(CustomNameImpl {
            name: TextComponent::text("b"),
        });
        assert!(!ItemEntity::are_mergeable(&plain, &named));
        assert!(!ItemEntity::are_mergeable(&named, &renamed));
        assert!(ItemEntity::are_mergeable(&named, &named.clone()));

        let mut enchanted = plain.clone();
        enchanted.add_enchantment(&Enchantment::UNBREAKING, 1);
        let mut stronger = plain.clone();
        stronger.add_enchantment(&Enchantment::UNBREAKING, 2);
        assert!(!ItemEntity::are_mergeable(&plain, &enchanted));
        assert!(!ItemEntity::are_mergeable(&enchanted, &stronger));

        let mut data = NbtCompound::new();
        data.put_string("id", "a".to_string());
        let mut tagged = plain.clone();
        tagged.set_data_component(CustomDataImpl::new(data));
        assert!(!ItemEntity::are_mergeable(&plain, &tagged));
    }

    #[test]
    fn damage_resistant_items() {
        let sword = ItemStack::new(1, &Item::NETHERITE_SWORD);
        assert!(!sword.can_be_hurt_by(DamageType::LAVA));
        assert!(sword.can_be_hurt_by(DamageType::CACTUS));

        let star = ItemStack::new(1, &Item::NETHER_STAR);
        assert!(!star.can_be_hurt_by(DamageType::EXPLOSION));
        assert!(star.can_be_hurt_by(DamageType::LAVA));
    }
}
