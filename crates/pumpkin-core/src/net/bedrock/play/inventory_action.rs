#[allow(clippy::wildcard_imports)]
use super::*;
use crate::block::BlockPlacement;
use crate::{entity::player::ItemUseSnapshot, item::registry::should_try_block_placement};
use pumpkin_data::item::Item;
use pumpkin_protocol::bedrock::server::inventory_transaction::HandSlot;

const SIMULATION_TICK_TRIGGER: u8 = 2;

impl BedrockClient {
    fn correct_rejected_food_use(&self, player: &Player) {
        // Holding use can repeat rejected transactions. Limit prediction corrections
        // to every 20 ticks, without indefinitely trusting a previously sent snapshot.
        const CORRECTION_INTERVAL_TICKS: i32 = 20;
        let tick = player.tick_counter.load(Ordering::Relaxed);
        let recently_corrected = self.last_food_rejection_tick.load().is_some_and(|last| {
            tick >= last && tick.saturating_sub(last) < CORRECTION_INTERVAL_TICKS
        });
        let has_active_use = player
            .living_entity
            .item_in_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if recently_corrected && !has_active_use {
            return;
        }
        if player.has_client_loaded() {
            self.last_food_rejection_tick.store(Some(tick));
        }
        player.living_entity.clear_active_hand();
        player.send_health();
    }

    #[allow(clippy::too_many_lines, clippy::collapsible_if, clippy::unreachable)]
    pub(super) fn handle_item_transaction(&self, player: &Arc<Player>, data: TransactionData) {
        match data {
            TransactionData::Normal(_) | TransactionData::Mismatch(_) => {}
            TransactionData::UseItem(data) => {
                let world = player.world();
                let Some(server) = world.server.upgrade() else {
                    return;
                };

                if data.action_type.0 == 0 {
                    let Some(face) = BlockDirection::from_index(data.block_face) else {
                        return;
                    };
                    let placement = BlockPlacement {
                        clicked_pos: data.block_position,
                        clicked_face: face,
                        cursor_pos: data.click_position,
                        hand: Hand::Right,
                    };
                    let snapshot = ItemUseSnapshot::new(player, Hand::Right);
                    let before_use = &snapshot.stack;
                    if before_use.item.id == Item::FISHING_ROD.id
                        && data.trigger_type == SIMULATION_TICK_TRIGGER
                    {
                        return;
                    }
                    let Some(block) = snapshot.block_interaction(player, &server, &placement)
                    else {
                        return;
                    };
                    let world = snapshot.world();
                    if player.gamemode.load() == GameMode::Spectator {
                        if let Some(factory) = server.block_registry.get_screen_handler_factory(
                            block,
                            player,
                            &placement.clicked_pos,
                            &server,
                            world,
                        ) {
                            player
                                .open_handled_screen(factory.as_ref(), Some(placement.clicked_pos));
                        }
                        return;
                    }

                    let mut held_item = before_use.clone();
                    if (!player.get_entity().is_sneaking() || held_item.is_empty())
                        && server
                            .block_registry
                            .use_clicked_block(
                                player,
                                &server,
                                world,
                                block,
                                &placement,
                                &mut held_item,
                            )
                            .consumes_action()
                    {
                        if !held_item.are_equal(before_use) {
                            if before_use.is_damageable() && held_item.is_empty() {
                                player.increment_stat(
                                    pumpkin_data::statistic::StatisticCategory::Broken,
                                    i32::from(before_use.item.id),
                                    1,
                                );
                                player.world().send_entity_status(
                                    player.get_entity(),
                                    crate::entity::equipment_break_status(
                                        &EquipmentSlot::MAIN_HAND,
                                    ),
                                    ActorEventID::None,
                                );
                            }
                            let selected = player.inventory().get_selected_slot() as usize;
                            player.inventory().set_held_item(held_item.clone());
                            player.sync_hand_slot(selected, held_item);
                        }
                        return;
                    }

                    // Cast or reel if the targeted block does not handle the interaction.
                    if held_item.item.id == Item::FISHING_ROD.id {
                        server.item_registry.on_use(before_use, player, Hand::Right);
                        return;
                    }

                    let mut stack = held_item;
                    if !stack.is_empty() {
                        let item_id = stack.item.id;
                        let before = stack.clone();
                        player.increment_stat(
                            pumpkin_data::statistic::StatisticCategory::Used,
                            item_id as i32,
                            1,
                        );
                        let item_result = server.item_registry.use_on_block(
                            &mut stack,
                            player,
                            data.block_position,
                            face,
                            data.click_position,
                            block,
                            &server,
                        );

                        if should_try_block_placement(&item_result) {
                            let item_id = stack.item.id;
                            if let Some(placed_block) = pumpkin_data::Block::from_item_id(item_id) {
                                if let Ok(Some(_)) = server.block_registry.place_block(
                                    player,
                                    placed_block,
                                    &server,
                                    &placement,
                                ) && player.gamemode.load() != GameMode::Creative
                                {
                                    stack.decrement(1);
                                }
                            }
                        }
                        if before.is_damageable() && stack.is_empty() {
                            player.increment_stat(
                                pumpkin_data::statistic::StatisticCategory::Broken,
                                item_id as i32,
                                1,
                            );
                            player.world().send_entity_status(
                                player.get_entity(),
                                crate::entity::equipment_break_status(&EquipmentSlot::MAIN_HAND),
                                ActorEventID::None,
                            );
                        }
                        player.inventory().set_held_item(stack);
                    }
                } else if data.action_type.0 == 1 {
                    // Click air / Use item
                    let hand = match data.hand {
                        HandSlot::Mainhand => Hand::Right,
                        HandSlot::Offhand => Hand::Left,
                    };
                    let snapshot = ItemUseSnapshot::new(player, hand);
                    if snapshot.stack.item.id == Item::FISHING_ROD.id
                        && data.trigger_type == SIMULATION_TICK_TRIGGER
                    {
                        return;
                    }
                    let mut held = snapshot.stack.clone();

                    let mut event = PlayerInteractEvent::new(
                        player,
                        InteractAction::RightClickAir,
                        &pumpkin_data::Block::AIR,
                        None,
                    );
                    server.plugin_manager.fire_blocking(&server, &mut event);
                    if event.cancelled || !snapshot.unchanged(player) {
                        snapshot.correct(player);
                        return;
                    }

                    if !held.is_empty() {
                        player.increment_stat(
                            pumpkin_data::statistic::StatisticCategory::Used,
                            held.item.id as i32,
                            1,
                        );
                    }

                    let stack_for_use = held.clone();

                    {
                        let mut cooldown_active = false;
                        if let Some(cooldown) = held.get_use_cooldown() {
                            let group = cooldown
                                .cooldown_group
                                .clone()
                                .unwrap_or_else(|| held.item.registry_key.to_string());
                            if player.is_on_cooldown(&group) {
                                cooldown_active = true;
                            }
                        }

                        if !cooldown_active {
                            // Bedrock can repeat click-air while using an item. Do not
                            // restart its server-side use timer on these timed inputs.
                            let already_using =
                                player.living_entity.item_use_time.load(Ordering::Relaxed) > 0
                                    && player
                                        .living_entity
                                        .item_in_use
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .as_ref()
                                        .is_some_and(|item| {
                                            item.are_items_and_components_equal(&held)
                                        });
                            if !already_using
                                && held.get_data_component::<ConsumableImpl>().is_some()
                            {
                                if held
                                    .get_data_component::<FoodImpl>()
                                    .is_none_or(|food| player.can_eat(food.can_always_eat))
                                {
                                    player.living_entity.set_active_hand(
                                        hand,
                                        held.clone(),
                                        held.get_max_use_time(),
                                    );
                                } else {
                                    // Correct predicted eating when the server's food
                                    // level is already full, even if it has not changed.
                                    self.correct_rejected_food_use(player);
                                }
                            }
                            if let Some(equippable) = held.get_data_component::<EquippableImpl>() {
                                let should_change = {
                                    let inventory = player.inventory();
                                    let equipment_guard = inventory
                                        .entity_equipment
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    let current_equipped = equipment_guard.get(equippable.slot);
                                    !current_equipped.are_items_and_components_equal(&held)
                                };
                                if should_change {
                                    player.enqueue_equipment_change(equippable.slot, &held);

                                    let inventory = player.inventory();
                                    let mut equipment_guard = inventory
                                        .entity_equipment
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    let equip_item = equipment_guard
                                        .equipment
                                        .entry(equippable.slot.clone())
                                        .or_insert_with(|| ItemStack::EMPTY.clone());
                                    if equip_item.is_empty() {
                                        *equip_item = held.clone();
                                        held.decrement_unless_creative(player.gamemode.load(), 1);
                                    } else {
                                        let old_held = held.clone();
                                        held = equip_item.clone();
                                        *equip_item = old_held;
                                    }
                                    drop(equipment_guard);
                                    player.inventory().set_stack_in_hand(hand, held.clone());
                                }
                            }
                        }
                    }

                    server.item_registry.on_use(&stack_for_use, player, hand);
                }
            }
            TransactionData::UseItemOnEntity(data) => {
                let action = match data.action_type.0 {
                    // Bedrock does not distinguish an entity hit position here. ItemInteract is
                    // therefore exposed as the general Interact action rather than InteractAt.
                    0 | 2 => ActionType::Interact,
                    1 => ActionType::Attack,
                    action => {
                        tracing::warn!("invalid UseItemOnEntity action type {action}");
                        return;
                    }
                };
                let Ok(target_runtime_id) = i32::try_from(data.target_entity_runtime_id.0) else {
                    tracing::warn!(
                        "invalid UseItemOnEntity target runtime ID {}",
                        data.target_entity_runtime_id.0
                    );
                    return;
                };

                let world = player.world();
                let Some(target) = world.get_entity_by_id(target_runtime_id) else {
                    return;
                };
                if !player.can_interact_with_entity(target.as_ref(), 1.0) {
                    return;
                }
                let Some(server) = world.server.upgrade() else {
                    return;
                };

                let mut event = PlayerInteractEntityEvent::new(
                    player,
                    target.clone(),
                    action,
                    None,
                    player.get_entity().is_sneaking(),
                );
                server.plugin_manager.fire_blocking(&server, &mut event);
                if event.cancelled {
                    return;
                }

                match event.action {
                    ActionType::Interact | ActionType::InteractAt => {
                        let mut stack = player.inventory().held_item();
                        let item_id = stack.item.id;
                        let before = stack.clone();
                        let native_interaction = event.target.interact_with_hand(
                            player,
                            &mut stack,
                            pumpkin_util::Hand::Right,
                        );
                        if !native_interaction
                            .unwrap_or_else(|| event.target.interact(player, &mut stack))
                        {
                            server
                                .item_registry
                                .use_on_entity(&mut stack, player, event.target);
                        }
                        if native_interaction.is_none() && !stack.are_equal(&before) {
                            player.increment_stat(
                                pumpkin_data::statistic::StatisticCategory::Used,
                                item_id as i32,
                                1,
                            );
                            if before.is_damageable() && stack.is_empty() {
                                player.increment_stat(
                                    pumpkin_data::statistic::StatisticCategory::Broken,
                                    item_id as i32,
                                    1,
                                );
                                player.world().send_entity_status(
                                    player.get_entity(),
                                    crate::entity::equipment_break_status(
                                        &EquipmentSlot::MAIN_HAND,
                                    ),
                                    ActorEventID::None,
                                );
                            }
                        }
                        player.inventory().set_held_item(stack);
                    }
                    ActionType::Attack => player.attack(&event.target),
                }
            }
            TransactionData::ReleaseItem(_data) => {
                let item_in_use = player
                    .living_entity
                    .item_in_use
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(stack) = item_in_use {
                    let Some(server) = player.world().server.upgrade() else {
                        return;
                    };
                    server.item_registry.on_stopped_using(&stack, player);
                }
                player.living_entity.clear_active_hand();
            }
        }
    }
}
