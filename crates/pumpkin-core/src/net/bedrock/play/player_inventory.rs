use std::sync::{Arc, Mutex, atomic::Ordering::Relaxed};

use pumpkin_data::{
    data_component_impl::BlocksAttacksImpl, item_stack::ItemStack, statistic::StatisticCategory,
};
use pumpkin_inventory::{
    container_click::SLOT_INDEX_OUTSIDE,
    crafting::{
        crafting_inventory::CraftingInventory,
        crafting_screen_handler::{CraftingTableScreenHandler, match_crafting_recipe},
    },
    double::DoubleInventory,
    generic_container_screen_handler::GenericContainerScreenHandler,
    inventory::Inventory,
    player::{player_inventory::PlayerInventory, player_screen_handler::PlayerScreenHandler},
    screen_handler::{ClickType, InventoryPlayer, ScreenHandler},
    slot::Slot,
};
use pumpkin_protocol::{
    RawPacket,
    bedrock::{
        client::{inventory_slot::CInventorySlot, item_stack_response::CItemStackResponse},
        network_item::{
            ContainerName, FullContainerName, NetworkItemDescriptor, NetworkItemStackDescriptor,
            potion_contents_from_bedrock,
        },
        server::{
            cursor_item_drag::{CURSOR_ITEM_DRAG_START, CURSOR_ITEM_DRAG_STOP, SCursorItemDrag},
            inventory_transaction::{InventoryAction, InventoryActionSource, WINDOW_ID_INVENTORY},
            item_stack_request::{
                ItemStackRequest, ItemStackRequestAction as Action,
                ItemStackRequestSlotInfo as SlotInfo, SItemStackRequest,
            },
        },
    },
    codec::{var_int::VarInt, var_uint::VarUInt},
    packet::Packet,
    serial::PacketRead,
};
use pumpkin_util::GameMode;

use crate::{
    entity::player::Player,
    net::bedrock::{BedrockClient, CursorDrag},
    plugin::{
        inventory::{
            craft_item::CraftItemEvent, inventory_drag::InventoryDragEvent,
            inventory_interact::InventoryInteractEvent, prepare_item_craft::PrepareItemCraftEvent,
        },
        player::inventory_interact::InventoryClickEvent,
        player::player_drop_item::PlayerDropItemEvent,
        server::packet::BedrockConnectionPacketSentEvent,
    },
    server::{Server, StorageTransferError},
    world::World,
};

use super::{
    CURSOR_SLOT, inventory_content_packet,
    item_stack_request::{
        SlotUpdate, build_stack_response, map_bedrock_container_slot, record_update,
    },
};

const REGIONAL_INVENTORY_BYTES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Slot(usize),
    Cursor,
    Created,
}

pub(super) struct StorageContext {
    screen: Arc<Mutex<dyn ScreenHandler>>,
    world: Arc<World>,
}

impl StorageContext {
    pub(super) fn capture(player: &Player) -> Option<Self> {
        let screen = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let storage = screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_any()
            .is::<GenericContainerScreenHandler>();
        storage.then(|| Self {
            screen,
            world: player.world(),
        })
    }
}

#[derive(Clone, Copy)]
struct CraftGrid {
    width: u8,
    first_slot: u8,
}

impl CraftGrid {
    fn current(handler: &dyn ScreenHandler) -> Option<Self> {
        if handler.as_any().is::<PlayerScreenHandler>() {
            Some(Self {
                width: 2,
                first_slot: 28,
            })
        } else if handler.as_any().is::<CraftingTableScreenHandler>() {
            Some(Self {
                width: 3,
                first_slot: 32,
            })
        } else {
            None
        }
    }

    fn len(self) -> usize {
        usize::from(self.width) * usize::from(self.width)
    }
}

struct ManualCraft {
    grid: CraftGrid,
    output: ItemStack,
    remaining_inputs: [u8; 9],
}

impl ManualCraft {
    fn prepare(
        world: &World,
        grid: CraftGrid,
        slots: &[ItemStack],
        repetitions: u8,
    ) -> Result<Option<Self>, ()> {
        let inputs = slots.get(1..=grid.len()).ok_or(())?;
        let inventory = CraftingInventory {
            width: grid.width,
            height: grid.width,
            items: std::sync::RwLock::new(inputs.to_vec()),
        };
        let server = world.server.upgrade().ok_or(())?;
        let recipe =
            match_crafting_recipe(&inventory, Some(server.recipe_manager.as_ref())).ok_or(())?;
        let mut output = recipe.to_item_stack();
        if output.is_empty() || repetitions == 0 {
            return Err(());
        }
        let mut remaining_inputs = [0; 9];
        for (index, stack) in inputs.iter().enumerate().filter(|(_, s)| !s.is_empty()) {
            // Recipes with remainder placement still use the existing handler.
            if recipe.remaining_item_for_slot(index, stack).is_some() {
                return Ok(None);
            }
            if stack.item_count < repetitions {
                return Err(());
            }
            remaining_inputs[index] = repetitions;
        }
        output.item_count = output.item_count.checked_mul(repetitions).ok_or(())?;
        Ok(Some(Self {
            grid,
            output,
            remaining_inputs,
        }))
    }

    fn input(&self, handler: &dyn ScreenHandler, info: &SlotInfo) -> Option<usize> {
        if info.container_name.dynamic_id.is_some()
            || info.container_name.container_name != ContainerName::CraftingInput
        {
            return None;
        }
        let index =
            map_bedrock_container_slot(handler, ContainerName::CraftingInput, info.slot_id)?
                .checked_sub(1)?;
        (index < self.grid.len()).then_some(index)
    }
}

struct InventoryPlan {
    world: Arc<World>,
    mode: GameMode,
    creative: bool,
    craft: Option<ManualCraft>,
    cursor_placement: bool,
    clicked_slot: Option<usize>,
    click: Option<InventoryClickEvent>,
    screen: Arc<Mutex<dyn ScreenHandler>>,
    slots: Vec<Arc<dyn Slot>>,
    before: Vec<ItemStack>,
    after: Vec<ItemStack>,
    cursor: Arc<Mutex<ItemStack>>,
    cursor_before: ItemStack,
    cursor_after: ItemStack,
    created: ItemStack,
    updates: Vec<SlotUpdate>,
    drops: Vec<ItemStack>,
    destroyed: Vec<Target>,
}

fn same_stack(a: &ItemStack, b: &ItemStack) -> bool {
    a.uid == b.uid && a.are_equal(b)
}

fn merge_stacks(source: &ItemStack, mut destination: ItemStack, count: u8) -> Option<ItemStack> {
    if destination.is_empty() {
        Some(source.copy_with_count(count))
    } else if destination.are_items_and_components_equal(source) {
        destination.item_count = destination.item_count.checked_add(count)?;
        Some(destination)
    } else {
        None
    }
}

fn stack_fits(slot: Option<&dyn Slot>, stack: &ItemStack) -> bool {
    stack.is_empty()
        || (stack.item_count <= stack.get_max_stack_size()
            && slot.is_none_or(|slot| {
                slot.can_insert(stack)
                    && stack.item_count <= slot.get_max_item_count_for_stack(stack)
            }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PersonalSlot {
    Storage(usize),
    Cursor,
}

impl PersonalSlot {
    fn from_request(slot: &SlotInfo) -> Option<Self> {
        if slot.container_name.dynamic_id.is_some() {
            return None;
        }
        let index = usize::from(slot.slot_id);
        match slot.container_name.container_name {
            ContainerName::Inventory | ContainerName::CombinedHotBarAndInventory
                if (PlayerInventory::HOTBAR_SIZE..PlayerInventory::MAIN_SIZE).contains(&index) =>
            {
                Some(Self::Storage(index))
            }
            ContainerName::Cursor if index == 0 => Some(Self::Cursor),
            _ => None,
        }
    }

    const fn stack<'a>(self, main: &'a [ItemStack], cursor: &'a ItemStack) -> &'a ItemStack {
        match self {
            Self::Storage(index) => &main[index],
            Self::Cursor => cursor,
        }
    }
}

const fn matches_stack_id(info: &SlotInfo, stack: &ItemStack) -> bool {
    let actual_id = if stack.is_empty() { 0 } else { stack.uid.get() };
    info.stack_id == actual_id
}

struct RegionalInventoryPlan {
    changes: [(PersonalSlot, ItemStack); 2],
    updates: Vec<SlotUpdate>,
}

impl RegionalInventoryPlan {
    fn prepare(
        request: &ItemStackRequest,
        handler: &dyn ScreenHandler,
        main: &[ItemStack],
        cursor: &ItemStack,
    ) -> Option<Self> {
        let (from, to, count) = match request.actions.as_slice() {
            [
                Action::Take {
                    source,
                    destination,
                    count,
                }
                | Action::Place {
                    source,
                    destination,
                    count,
                },
            ] => (source, destination, Some(*count)),
            [Action::Swap { slot1, slot2 }] => (slot1, slot2, None),
            _ => return None,
        };
        let from_target = PersonalSlot::from_request(from)?;
        let to_target = PersonalSlot::from_request(to)?;
        if from_target == to_target {
            return None;
        }
        let first = from_target.stack(main, cursor);
        let second = to_target.stack(main, cursor);
        if !matches_stack_id(from, first) || !matches_stack_id(to, second) {
            return None;
        }
        let (first, second) = if let Some(count) = count {
            if count == 0 || first.is_empty() || count > first.item_count {
                return None;
            }
            let mut remainder = first.clone();
            let merged = merge_stacks(first, second.clone(), count)?;
            remainder.decrement(count);
            (remainder, merged)
        } else {
            (second.clone(), first.clone())
        };
        let changes = [(from_target, first), (to_target, second)].map(|(slot, stack)| {
            (
                slot,
                if stack.is_empty() {
                    ItemStack::EMPTY.clone()
                } else {
                    stack
                },
            )
        });
        let mut updates = Vec::with_capacity(changes.len());
        for ((target, stack), info) in changes.iter().zip([from, to]) {
            let slot = match target {
                PersonalSlot::Storage(index) => {
                    Some(handler.get_behaviour().slots.get(*index)?.as_ref())
                }
                PersonalSlot::Cursor => None,
            };
            if !stack_fits(slot, stack) {
                return None;
            }
            record_update(
                &mut updates,
                info.container_name.clone(),
                info.slot_id,
                stack,
            );
        }
        Some(Self { changes, updates })
    }
}

pub(super) fn matches_item_type_and_count(desc: &NetworkItemDescriptor, stack: &ItemStack) -> bool {
    if stack.is_empty() {
        return desc.id.0 == 0 || desc.stack_size == 0;
    }
    // Client NBT and runtime state never replace data on the server's stack.
    let expected = NetworkItemDescriptor::from(stack);
    desc.id == expected.id
        && desc.aux_value == expected.aux_value
        && desc.stack_size == expected.stack_size
}

const fn player_container(info: &SlotInfo) -> bool {
    matches!(
        info.container_name.container_name,
        ContainerName::Inventory
            | ContainerName::CombinedHotBarAndInventory
            | ContainerName::HotBar
            | ContainerName::Armor
            | ContainerName::Offhand
            | ContainerName::Cursor
    )
}

pub(super) fn is_player_inventory_request(request: &ItemStackRequest) -> bool {
    request.actions.iter().all(|action| match action {
        Action::Take {
            source,
            destination,
            ..
        }
        | Action::Place {
            source,
            destination,
            ..
        } => player_container(source) && player_container(destination),
        Action::Swap { slot1, slot2 } => player_container(slot1) && player_container(slot2),
        Action::Drop { source, .. }
        | Action::Destroy { source, .. }
        | Action::Consume { source, .. } => player_container(source),
        _ => false,
    })
}

fn player_slot(player: &Player, slot: &dyn Slot) -> Option<usize> {
    let inventory: Arc<dyn Inventory> = player.inventory.clone();
    if !Arc::ptr_eq(&inventory, &slot.get_inventory()) {
        return None;
    }
    match slot.get_index() {
        index @ 0..9 => Some(36 + index),
        index @ 9..36 => Some(index),
        index @ 36..40 => Some(44 - index),
        40 => Some(45),
        _ => None,
    }
}

fn creative_stack(network_id: u32) -> Option<ItemStack> {
    let index = network_id.checked_sub(1)? as usize;
    let entry = pumpkin_data::bedrock_creative::CREATIVE_ENTRIES.get(index)?;
    let mapping = pumpkin_data::item::JavaToBedrockItemMapping::from_bedrock(
        entry.item_id,
        entry.item_aux_value,
    )?;
    let limit = ItemStack::static_new_java(1, mapping.java_item).get_max_stack_size();
    let mut stack = ItemStack::new(limit, mapping.java_item);
    if let Some(contents) = potion_contents_from_bedrock(entry.item_id, entry.item_aux_value) {
        stack.set_data_component(contents);
    }
    Some(stack)
}

impl InventoryPlan {
    fn prepare(
        player: &Arc<Player>,
        request: &ItemStackRequest,
        storage: Option<&StorageContext>,
        manual_craft: bool,
    ) -> Result<Option<Self>, ()> {
        let mode = player.gamemode.load();
        let creative = request
            .actions
            .iter()
            .any(|action| matches!(action, Action::CraftCreative { .. }));
        let screen = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if storage.is_some_and(|context| {
            !Arc::ptr_eq(&screen, &context.screen) || !Arc::ptr_eq(&player.world(), &context.world)
        }) {
            return Err(());
        }
        let handler = screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let grid = if manual_craft {
            let Some(grid) = CraftGrid::current(&*handler) else {
                return Ok(None);
            };
            Some(grid)
        } else {
            None
        };
        if mode == GameMode::Spectator || (creative && mode != GameMode::Creative) {
            return Err(());
        }
        if !handler.can_use(player.as_ref()) {
            return Err(());
        }
        let slots = handler.get_behaviour().slots.clone();
        let before: Vec<_> = slots.iter().map(|slot| slot.get_cloned_stack()).collect();
        let cursor = handler.get_behaviour().cursor_stack.clone();
        let cursor_before = cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let world = player.world();
        let (craft, actions) = if let Some(grid) = grid {
            let [Action::CraftRecipe { repetitions, .. }, actions @ ..] =
                request.actions.as_slice()
            else {
                return Err(());
            };
            if creative {
                return Err(());
            }
            let Some(craft) = ManualCraft::prepare(&world, grid, &before, *repetitions)? else {
                return Ok(None);
            };
            (Some(craft), actions)
        } else {
            (None, request.actions.as_slice())
        };
        let mut plan = Self {
            world,
            mode,
            creative,
            clicked_slot: craft.as_ref().map(|_| 0),
            created: craft
                .as_ref()
                .map_or_else(|| ItemStack::EMPTY.clone(), |c| c.output.clone()),
            craft,
            cursor_placement: false,
            click: None,
            screen: screen.clone(),
            slots,
            after: before.clone(),
            before,
            cursor,
            cursor_after: cursor_before.clone(),
            cursor_before,
            updates: Vec::new(),
            drops: Vec::new(),
            destroyed: Vec::new(),
        };
        plan.apply(player, &*handler, actions)?;
        if let Some(craft) = &plan.craft
            && (!plan.created.is_empty() || craft.remaining_inputs.iter().any(|&count| count != 0))
        {
            return Err(());
        }
        if !creative && !request.actions.is_empty() {
            plan.click = Some(plan.click_event(player, &*handler)?);
        }
        Ok(Some(plan))
    }

    fn click_event(
        &self,
        player: &Arc<Player>,
        handler: &dyn ScreenHandler,
    ) -> Result<InventoryClickEvent, ()> {
        let (slot, clicked) = match self.clicked_slot {
            Some(index) => (
                i16::try_from(index).map_err(|_| ())?,
                Some(self.before[index].clone()),
            ),
            None => (SLOT_INDEX_OUTSIDE, None),
        };
        Ok(InventoryClickEvent::new(
            player,
            handler.window_type(),
            ClickType::Unknown,
            slot,
            slot,
            clicked,
            Some(self.cursor_before.clone()),
            -1,
        ))
    }

    fn apply(
        &mut self,
        player: &Player,
        handler: &dyn ScreenHandler,
        actions: &[Action],
    ) -> Result<(), ()> {
        let mut created = false;
        for action in actions {
            match action {
                Action::CraftCreative {
                    creative_item_id, ..
                } => {
                    if created {
                        return Err(());
                    }
                    self.created = creative_stack(creative_item_id.0).ok_or(())?;
                    created = true;
                }
                Action::Take {
                    count,
                    source,
                    destination,
                }
                | Action::Place {
                    count,
                    source,
                    destination,
                } => {
                    self.transfer(player, handler, *count, source, destination)?;
                }
                Action::Swap { slot1, slot2 } => {
                    let a = self.target(player, handler, slot1)?;
                    let b = self.target(player, handler, slot2)?;
                    if a == Target::Created || b == Target::Created {
                        return Err(());
                    }
                    let first = self.get(a);
                    let second = self.get(b);
                    self.can_take(player, a)?;
                    self.can_take(player, b)?;
                    self.can_put(a, &second)?;
                    self.can_put(b, &first)?;
                    self.put(a, second, slot1);
                    self.put(b, first, slot2);
                }
                Action::Consume { count, source } if self.craft.is_some() => {
                    let craft = self.craft.as_mut().ok_or(())?;
                    let index = craft.input(handler, source).ok_or(())?;
                    if *count == 0 || *count > craft.remaining_inputs[index] {
                        return Err(());
                    }
                    craft.remaining_inputs[index] -= *count;
                    let target = Target::Slot(index + 1);
                    let mut stack = self.get(target);
                    stack.decrement(*count);
                    self.put(target, stack, source);
                }
                Action::Destroy { count, source }
                | Action::Consume { count, source }
                | Action::Drop { count, source, .. } => {
                    if (matches!(action, Action::Destroy { .. }) && self.mode != GameMode::Creative)
                        || (matches!(action, Action::Consume { .. }) && !self.creative)
                    {
                        return Err(());
                    }
                    let from = self.target(player, handler, source)?;
                    let mut stack = self.get(from);
                    if *count == 0 || *count > stack.item_count {
                        return Err(());
                    }
                    self.can_take(player, from)?;
                    if matches!(action, Action::Drop { .. }) {
                        self.drops.push(stack.copy_with_count(*count));
                    }
                    stack.decrement(*count);
                    self.put(from, stack, source);
                    if matches!(action, Action::Destroy { .. }) && !self.destroyed.contains(&from) {
                        self.destroyed.push(from);
                    }
                }
                Action::CraftResultsDeprecated { .. } if self.creative || self.craft.is_some() => {}
                Action::Create { .. } if self.creative => {}
                _ => return Err(()),
            }
        }
        Ok(())
    }

    fn transfer(
        &mut self,
        player: &Player,
        handler: &dyn ScreenHandler,
        count: u8,
        source: &SlotInfo,
        destination: &SlotInfo,
    ) -> Result<(), ()> {
        let from = self.target(player, handler, source)?;
        let to = self.target(player, handler, destination)?;
        if to == Target::Created {
            return Err(());
        }
        let mut source_stack = self.get(from);
        if count == 0 || count > source_stack.item_count {
            return Err(());
        }
        if from == to {
            if let Target::Slot(index) = from {
                self.clicked_slot.get_or_insert(index);
            }
            return Ok(());
        }
        let dest = merge_stacks(&source_stack, self.get(to), count).ok_or(())?;
        self.can_put(to, &dest)?;
        self.can_take(player, from)?;
        self.cursor_placement |= from == Target::Cursor && matches!(to, Target::Slot(_));
        source_stack.decrement(count);
        self.put(from, source_stack, source);
        self.put(to, dest, destination);
        Ok(())
    }

    fn target(
        &self,
        player: &Player,
        handler: &dyn ScreenHandler,
        info: &SlotInfo,
    ) -> Result<Target, ()> {
        if info.container_name.dynamic_id.is_some() {
            return Err(());
        }
        let name = info.container_name.container_name;
        match name {
            ContainerName::CreatedOutput if self.creative || self.craft.is_some() => {
                return Ok(Target::Created);
            }
            ContainerName::Cursor if info.slot_id == 0 => return Ok(Target::Cursor),
            ContainerName::HotBar if info.slot_id >= 9 => return Err(()),
            ContainerName::Inventory
            | ContainerName::CombinedHotBarAndInventory
            | ContainerName::HotBar
            | ContainerName::Armor
            | ContainerName::Offhand => {}
            ContainerName::LevelEntity
                if !self.creative && handler.as_any().is::<GenericContainerScreenHandler>() => {}
            _ => return Err(()),
        }
        let index = map_bedrock_container_slot(handler, name, info.slot_id).ok_or(())?;
        let slot = self.slots.get(index).ok_or(())?;
        if name != ContainerName::LevelEntity {
            player_slot(player, slot.as_ref()).ok_or(())?;
        }
        Ok(Target::Slot(index))
    }

    fn get(&self, target: Target) -> ItemStack {
        match target {
            Target::Slot(index) => self.after[index].clone(),
            Target::Cursor => self.cursor_after.clone(),
            Target::Created => self.created.clone(),
        }
    }

    fn can_take(&self, player: &Player, target: Target) -> Result<(), ()> {
        if let Target::Slot(index) = target
            && !self.slots[index].can_take_items(player)
        {
            return Err(());
        }
        Ok(())
    }

    fn can_put(&self, target: Target, stack: &ItemStack) -> Result<(), ()> {
        let slot = match target {
            Target::Slot(index) => Some(self.slots[index].as_ref()),
            _ => None,
        };
        stack_fits(slot, stack).then_some(()).ok_or(())
    }

    fn put(&mut self, target: Target, stack: ItemStack, info: &SlotInfo) {
        if let Target::Slot(index) = target {
            self.clicked_slot.get_or_insert(index);
        }
        let stack = if stack.is_empty() {
            ItemStack::EMPTY.clone()
        } else {
            stack
        };
        match target {
            Target::Slot(index) => self.after[index] = stack.clone(),
            Target::Cursor => self.cursor_after = stack.clone(),
            Target::Created => self.created = stack.clone(),
        }
        record_update(
            &mut self.updates,
            info.container_name.clone(),
            info.slot_id,
            &stack,
        );
    }

    fn authorize(&self, player: &Arc<Player>, dragging: bool) -> bool {
        let Some(server) = self.world.server.upgrade() else {
            return false;
        };
        if let Some(event) = &self.click {
            let mut event = event.clone();
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return false;
            }
        }
        if self.creative {
            for (index, stack) in self.after.iter().enumerate() {
                if !same_stack(stack, &self.before[index]) {
                    let Some(slot) = player_slot(player, self.slots[index].as_ref()) else {
                        return false;
                    };
                    if !player.allow_creative_change(slot as i16, stack) {
                        return false;
                    }
                }
            }
            if !same_stack(&self.cursor_before, &self.cursor_after)
                && !player.allow_creative_change(-1, &self.cursor_after)
            {
                return false;
            }
        } else {
            let mut event = InventoryInteractEvent::new(player.clone());
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return false;
            }
            if dragging {
                let mut event = InventoryDragEvent::new(player.clone());
                server.plugin_manager.fire_blocking(&server, &mut event);
                if event.cancelled {
                    return false;
                }
            }
            if let Some(craft) = &self.craft {
                let identifier = craft.output.item.registry_key.to_string();
                let mut event = CraftItemEvent::new(player.clone(), identifier.clone());
                let mut prepare = PrepareItemCraftEvent::new(player.clone(), identifier);
                server.plugin_manager.fire_blocking(&server, &mut event);
                server.plugin_manager.fire_blocking(&server, &mut prepare);
                if event.cancelled || prepare.cancelled {
                    return false;
                }
            }
            // Destroy has no CraftCreative action, but still sets a creative slot.
            for target in &self.destroyed {
                let slot = match *target {
                    Target::Slot(index) => {
                        let Some(slot) = player_slot(player, self.slots[index].as_ref()) else {
                            return false;
                        };
                        slot as i16
                    }
                    Target::Cursor => -1,
                    Target::Created => return false,
                };
                if !player.allow_creative_change(slot, &self.get(*target)) {
                    return false;
                }
            }
        }
        for stack in &self.drops {
            if self.creative && !player.allow_creative_change(-1, stack) {
                return false;
            }
            let mut event = PlayerDropItemEvent::new(
                player.clone(),
                stack.item.registry_key.to_string(),
                stack.item_count,
            );
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return false;
            }
        }
        true
    }

    fn commit(&self, player: &Arc<Player>) -> bool {
        let current = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Arc::ptr_eq(&current, &self.screen)
            || !Arc::ptr_eq(&player.world(), &self.world)
            || player.gamemode.load() != self.mode
        {
            return false;
        }
        let mut handler = current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handler.get_behaviour().slots.len() != self.slots.len()
            || !handler.can_use(player.as_ref())
        {
            return false;
        }
        for (index, before) in self.before.iter().enumerate() {
            let recipe_input = self
                .craft
                .as_ref()
                .is_some_and(|c| (1..=c.grid.len()).contains(&index));
            if (recipe_input || !same_stack(before, &self.after[index]))
                && (!Arc::ptr_eq(&self.slots[index], &handler.get_behaviour().slots[index])
                    || !same_stack(before, &self.slots[index].get_cloned_stack()))
            {
                return false;
            }
        }
        let cursor_changed = !same_stack(&self.cursor_before, &self.cursor_after);
        if cursor_changed && !Arc::ptr_eq(&self.cursor, &handler.get_behaviour().cursor_stack) {
            return false;
        }
        let mut cursor = cursor_changed.then(|| {
            self.cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if let Some(cursor) = &cursor
            && !same_stack(cursor, &self.cursor_before)
        {
            return false;
        }
        let transfer = if self.drops.is_empty()
            && self.crosses_storage_boundaries(&*handler, cursor_changed)
            && let Some(server) = self.world.server.upgrade()
        {
            match server.prepare_storage_transfer(player, &*handler) {
                Ok(transfer) => transfer,
                Err(StorageTransferError::Busy) => return false,
                Err(StorageTransferError::StaleContainer) => {
                    drop(cursor);
                    drop(handler);
                    // Closing the screen reacquires the active handler lock.
                    drop(current);
                    player.close_handled_screen();
                    return false;
                }
            }
        } else {
            None
        };
        for (index, after) in self.after.iter().enumerate() {
            if !same_stack(&self.before[index], after) {
                self.slots[index].set_stack(after.clone());
                handler.set_received_stack(index, after.clone());
            }
        }
        if let Some(cursor) = &mut cursor {
            **cursor = self.cursor_after.clone();
        }
        if let Some(transfer) = transfer {
            transfer.record();
        }
        true
    }

    fn crosses_storage_boundaries(
        &self,
        handler: &dyn ScreenHandler,
        cursor_changed: bool,
    ) -> bool {
        let Some(storage) = handler
            .as_any()
            .downcast_ref::<GenericContainerScreenHandler>()
        else {
            return false;
        };
        let count = storage.inventory.size();
        if count > self.before.len() {
            return false;
        }
        let changed = |range: std::ops::Range<usize>| {
            self.before[range.clone()]
                .iter()
                .zip(&self.after[range])
                .any(|(before, after)| !same_stack(before, after))
        };
        if changed(0..count) && (cursor_changed || changed(count..self.before.len())) {
            return true;
        }
        storage
            .inventory
            .as_any()
            .downcast_ref::<DoubleInventory>()
            .is_some_and(|inventory| {
                let split = inventory.halves()[0].size();
                changed(0..split) && changed(split..count)
            })
    }

    fn execute(self, player: &Arc<Player>, dragging: bool) -> Option<Vec<SlotUpdate>> {
        let accepted = self.authorize(player, dragging) && self.commit(player);
        for (index, before) in self.before.iter().enumerate() {
            let current = self.slots[index].get_cloned_stack();
            if !before.are_equal(&current)
                && let Some(slot) = player_slot(player, self.slots[index].as_ref())
            {
                player.sync_screen_slot_equipment(slot, &current);
            }
        }
        if !accepted {
            return None;
        }
        if let Some(craft) = &self.craft {
            player.increment_stat(
                StatisticCategory::Crafted,
                i32::from(craft.output.item.id),
                i32::from(craft.output.item_count),
            );
            player.on_recipe_crafted(&craft.output);
        }
        for stack in self.drops {
            player.drop_item(stack);
        }
        Some(self.updates)
    }
}

impl BedrockClient {
    pub(in crate::net::bedrock) const fn can_route_inventory(packet: &RawPacket) -> bool {
        packet.id == SItemStackRequest::PACKET_ID
            && packet.payload.len() <= REGIONAL_INVENTORY_BYTES
    }

    fn inventory_request_can_run_on_region(&self, player: &Player, server: &Server) -> bool {
        let plugins = &server.plugin_manager;
        player.has_client_loaded()
            && !player.living_entity.is_dead_or_dying()
            && !server.stop_token.is_cancelled()
            && !server.tick_rate_manager.is_frozen()
            && !plugins.has_handlers::<InventoryClickEvent>()
            && !plugins.has_handlers::<InventoryInteractEvent>()
            && !plugins.has_handlers::<BedrockConnectionPacketSentEvent>()
            && self.cursor_drag.try_lock().is_ok_and(|drag| drag.is_none())
            // A full inventory refresh can toggle shield blocking. Keep that
            // side effect on the world when it can change the player's state.
            && !player.living_entity.entity.is_sneaking()
            && player.living_entity.item_in_use.try_lock().is_ok_and(|active| {
                active.as_ref().is_none_or(|stack| stack.get_data_component::<BlocksAttacksImpl>().is_none())
            })
    }

    pub(in crate::net::bedrock) fn try_regional_inventory_request(
        &self,
        player: &Player,
        server: &Server,
        packet: &RawPacket,
        owns_player: impl FnOnce() -> bool,
    ) -> Option<()> {
        if !Self::can_route_inventory(packet) {
            return None;
        }
        let mut payload = packet.payload.as_ref();
        let packet = SItemStackRequest::read(&mut payload).ok()?;
        let [request] = packet.requests.as_slice() else {
            return None;
        };
        let mode = player.gamemode.load();
        if !payload.is_empty()
            || !request.filter_strings.is_empty()
            || mode == GameMode::Spectator
            || !self.inventory_request_can_run_on_region(player, server)
        {
            return None;
        }
        let current = player.current_screen_handler.try_lock().ok()?;
        if !std::ptr::addr_eq(
            Arc::as_ptr(&current),
            Arc::as_ptr(&player.player_screen_handler),
        ) {
            return None;
        }
        let mut handler = current.try_lock().ok()?;
        if !handler.as_any().is::<PlayerScreenHandler>() || !handler.can_use(player) {
            return None;
        }
        // Player checkpoints take these locks in the same order, so they see
        // the stacks before the move or after it, including the cursor.
        let cursor = handler.get_behaviour().cursor_stack.clone();
        let mut cursor = cursor.try_lock().ok()?;
        let mut main = player.inventory.main_inventory.try_write().ok()?;
        let equipment = player.inventory.entity_equipment.try_lock().ok()?;
        let RegionalInventoryPlan { changes, updates } =
            RegionalInventoryPlan::prepare(request, &*handler, &main[..], &cursor)?;
        let after = |target| {
            changes
                .iter()
                .find(|(changed, _)| *changed == target)
                .map_or_else(|| target.stack(&main[..], &cursor), |(_, stack)| stack)
        };
        let response = self
            .serialize_packet(&CItemStackResponse {
                responses: vec![build_stack_response(request.request_id, 0, updates)],
            })
            .ok()?;
        let content = self
            .serialize_packet(&inventory_content_packet(
                WINDOW_ID_INVENTORY as u32,
                ContainerName::Inventory,
                (0..PlayerInventory::MAIN_SIZE)
                    .map(|index| {
                        NetworkItemStackDescriptor::from(after(PersonalSlot::Storage(index)))
                    })
                    .collect(),
            ))
            .ok()?;
        let cursor_packet = self
            .serialize_inventory_slot(CURSOR_SLOT, after(PersonalSlot::Cursor))
            .ok()?;
        let equipment_packets = (PlayerInventory::MAIN_SIZE..=PlayerInventory::OFF_HAND_SLOT)
            .map(|index| {
                let stack = player
                    .inventory
                    .equipment_slots
                    .get(&index)
                    .map_or_else(|| ItemStack::EMPTY.clone(), |slot| equipment.get(slot));
                self.encode_player_inventory_slot(index, &stack)
            })
            .collect::<Option<Vec<_>>>()?;
        if player.gamemode.load() != mode || !player.has_stable_world_membership() || !owns_player()
        {
            return None;
        }
        for (target, stack) in changes {
            match target {
                PersonalSlot::Storage(index) => {
                    if !same_stack(&main[index], &stack) {
                        main[index] = stack.clone();
                        handler.set_received_stack(index, stack);
                        player.inventory_changed.store(true, Relaxed);
                    }
                }
                PersonalSlot::Cursor => *cursor = stack,
            }
        }
        for packet in [response, content, cursor_packet]
            .into_iter()
            .chain(equipment_packets)
        {
            if !self.try_enqueue_packet_data_unobserved(packet) {
                break;
            }
        }
        tracing::trace!(player = %player.gameprofile.name, request_id = request.request_id.0, "Completed Bedrock inventory request on its region");
        Some(())
    }

    pub(in crate::net::bedrock) fn handle_cursor_drag(
        &self,
        player: &Player,
        packet: &SCursorItemDrag,
    ) {
        match packet.state {
            CURSOR_ITEM_DRAG_START => {
                let screen = player
                    .current_screen_handler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let world = player.world();
                *self
                    .cursor_drag
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CursorDrag {
                    screen: Arc::downgrade(&screen),
                    world: Arc::downgrade(&world),
                });
            }
            CURSOR_ITEM_DRAG_STOP => self.clear_cursor_drag(),
            _ => {}
        }
    }

    pub(crate) fn clear_cursor_drag(&self) {
        *self
            .cursor_drag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub(super) fn handle_inventory_request(
        &self,
        player: &Arc<Player>,
        request: &ItemStackRequest,
        storage: Option<&StorageContext>,
    ) {
        let (result, updates) = InventoryPlan::prepare(player, request, storage, false)
            .ok()
            .flatten()
            .and_then(|plan| {
                let dragging = plan.cursor_placement
                    && !plan.creative
                    && self
                        .cursor_drag
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .is_some_and(|drag| {
                            std::ptr::addr_eq(drag.screen.as_ptr(), Arc::as_ptr(&plan.screen))
                                && std::ptr::addr_eq(drag.world.as_ptr(), Arc::as_ptr(&plan.world))
                        });
                plan.execute(player, dragging)
            })
            .map_or_else(|| (1, Vec::new()), |updates| (0, updates));
        self.try_enqueue_client_packet(&CItemStackResponse {
            responses: vec![build_stack_response(request.request_id, result, updates)],
        });
        self.resend_player_inventory(player);
    }

    pub(super) fn handle_manual_craft(
        &self,
        player: &Arc<Player>,
        request: &ItemStackRequest,
    ) -> bool {
        let result = match InventoryPlan::prepare(player, request, None, true) {
            Ok(Some(plan)) => plan.execute(player, false),
            Ok(None) => return false,
            Err(()) => None,
        };
        let (status, updates) = result.map_or_else(|| (1, Vec::new()), |updates| (0, updates));
        self.try_enqueue_client_packet(&CItemStackResponse {
            responses: vec![build_stack_response(request.request_id, status, updates)],
        });
        self.resend_player_inventory(player);
        let screen = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let handler = screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(grid) = CraftGrid::current(&*handler) {
            for (index, slot) in handler
                .get_behaviour()
                .slots
                .iter()
                .skip(1)
                .take(grid.len())
                .enumerate()
            {
                self.try_enqueue_client_packet(&CInventorySlot {
                    container_id: VarUInt(124),
                    slot: VarUInt(u32::from(grid.first_slot) + index as u32),
                    full_container_name: Some(FullContainerName {
                        container_name: ContainerName::CraftingInput,
                        dynamic_id: None,
                    }),
                    storage_item: None,
                    item: NetworkItemStackDescriptor::from(&slot.get_cloned_stack()),
                });
            }
        }
        true
    }

    pub(super) fn apply_legacy_inventory_drop(
        player: &Arc<Player>,
        actions: &[InventoryAction],
    ) -> Option<Vec<SlotUpdate>> {
        let [first, second] = actions else {
            return None;
        };
        let (source, dropped) = match (
            InventoryActionSource::from(first.source_type),
            InventoryActionSource::from(second.source_type),
        ) {
            (InventoryActionSource::Container, InventoryActionSource::World) => (first, second),
            (InventoryActionSource::World, InventoryActionSource::Container) => (second, first),
            _ => return None,
        };
        if source.window_id != Some(WINDOW_ID_INVENTORY)
            || source.inventory_slot >= 36
            || dropped.inventory_slot != 0
            || (dropped.old_item.id.0 != 0 && dropped.old_item.stack_size != 0)
        {
            return None;
        }
        let count = u8::try_from(dropped.new_item.stack_size).ok()?;
        let slot = source.inventory_slot as u8;
        let request = ItemStackRequest {
            request_id: VarInt(0),
            actions: vec![Action::Drop {
                count,
                source: SlotInfo {
                    container_name: FullContainerName {
                        container_name: ContainerName::Inventory,
                        dynamic_id: None,
                    },
                    slot_id: slot,
                    stack_id: 0,
                },
                randomly: false,
            }],
            filter_strings: Vec::new(),
            filter_cause: 0,
        };
        let mut plan = InventoryPlan::prepare(player, &request, None, false).ok()??;
        // Legacy hotbar drops do not carry an inventory click gesture.
        plan.click = None;
        let screen_slot = if slot < 9 {
            usize::from(slot) + 36
        } else {
            usize::from(slot)
        };
        let index = plan
            .slots
            .iter()
            .position(|slot| player_slot(player, slot.as_ref()) == Some(screen_slot))?;
        let actual = &plan.before[index];
        let remaining = actual.copy_with_count(actual.item_count.checked_sub(count)?);
        if !matches_item_type_and_count(&source.old_item, actual)
            || !matches_item_type_and_count(&source.new_item, &remaining)
            || !matches_item_type_and_count(&dropped.new_item, &actual.copy_with_count(count))
        {
            return None;
        }
        plan.execute(player, false)
    }

    pub(super) fn resend_player_inventory(&self, player: &Player) {
        let slots = player
            .inventory
            .main_inventory
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(NetworkItemStackDescriptor::from)
            .collect();
        let current = player
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (cursor, storage) = {
            let handler = current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let behaviour = handler.get_behaviour();
            let storage = handler
                .as_any()
                .is::<GenericContainerScreenHandler>()
                .then(|| {
                    let count = behaviour
                        .slots
                        .len()
                        .saturating_sub(PlayerInventory::MAIN_SIZE);
                    (
                        behaviour.sync_id,
                        behaviour.slots[..count]
                            .iter()
                            .map(|slot| NetworkItemStackDescriptor::from(&slot.get_cloned_stack()))
                            .collect(),
                    )
                });
            (behaviour.cursor_stack.clone(), storage)
        };
        let cursor = cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let equipment: Vec<_> = (36..=40)
            .filter_map(|index| {
                super::bedrock_player_inventory_slot(index)
                    .map(|target| (target, player.inventory.get_slot(index)))
            })
            .collect();
        self.try_enqueue_client_packet(&inventory_content_packet(
            WINDOW_ID_INVENTORY as u32,
            ContainerName::Inventory,
            slots,
        ));
        if let Some((window, slots)) = storage {
            self.try_enqueue_client_packet(&inventory_content_packet(
                u32::from(window),
                ContainerName::LevelEntity,
                slots,
            ));
        }
        super::send_bedrock_cursor_stack(self, &cursor);
        for (target, stack) in equipment {
            super::send_bedrock_inventory_slot(self, target, &stack);
        }
        player.update_bedrock_shield_sneaking();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::item::Item;
    use pumpkin_inventory::{build_equipment_slots, entity_equipment::EntityEquipment};

    #[test]
    fn regional_inventory_move_checks_both_slot_ids_before_writing() {
        let inventory = Arc::new(PlayerInventory::new(
            Arc::new(Mutex::new(EntityEquipment::new())),
            Arc::new(build_equipment_slots()),
        ));
        let handler = PlayerScreenHandler::new(&inventory, None, 0, None);
        let mut main = inventory.main_inventory.write().unwrap();
        main[10] = ItemStack::new(11, &Item::DIAMOND);
        let cursor = ItemStack::new(5, &Item::DIAMOND);
        let action = |count, source_id, cursor_id| Action::Take {
            count,
            source: SlotInfo {
                container_name: FullContainerName {
                    container_name: ContainerName::CombinedHotBarAndInventory,
                    dynamic_id: None,
                },
                slot_id: 10,
                stack_id: source_id,
            },
            destination: SlotInfo {
                container_name: FullContainerName {
                    container_name: ContainerName::Cursor,
                    dynamic_id: None,
                },
                slot_id: 0,
                stack_id: cursor_id,
            },
        };
        let source = main[10].uid.get();
        let target = cursor.uid.get();
        let plan = |action| {
            RegionalInventoryPlan::prepare(
                &ItemStackRequest {
                    request_id: VarInt(-7),
                    actions: vec![action],
                    filter_strings: Vec::new(),
                    filter_cause: 0,
                },
                &handler,
                &main[..],
                &cursor,
            )
        };
        let valid = plan(action(3, source, target)).unwrap();
        let [(_, left), (_, held)] = valid.changes;
        assert_eq!((left.item_count, held.item_count), (8, 8));
        assert_eq!((left.uid.get(), held.uid.get()), (source, target));
        assert!(plan(action(3, 0, target)).is_none());
        assert!(plan(action(3, source, -7)).is_none());
        assert!(plan(action(12, source, target)).is_none());
        assert_eq!((main[10].item_count, cursor.item_count), (11, 5));
    }
}
