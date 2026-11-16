use super::*;
use sculith_game::Edition;

async fn seed_inventory(world: &WorldHandle, id: u64) {
    for (index, item) in ["minecraft:stone", "minecraft:dirt", "minecraft:grass_block"]
        .iter()
        .enumerate()
    {
        world
            .plugin_inventory(
                "fixture".into(),
                id,
                index as u32 + 1,
                sculith_core::inventory::InventoryOperation::Give {
                    item: (*item).into(),
                    count: 64,
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn plugin_inventory_results_are_private_and_notifications_read_current_owner_state() {
    use sculith_core::inventory::{InventoryError, InventoryOperation, InventoryResult};
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let actor = world
        .reserve(profile("api-inventory"), Permissions { can_build: true })
        .await
        .unwrap();
    let viewer = world
        .reserve(profile("api-viewer"), Permissions::default())
        .await
        .unwrap();
    let id = actor.player.id;
    world.activate(id).await.unwrap();
    world.activate(viewer.player.id).await.unwrap();
    seed_inventory(&world, id).await;
    world.configure_observations(crate::observations::INVENTORY);
    let mut normal = world.subscribe();
    let mut hooks = world.subscribe_game();
    let mut peers = world.observe_players(viewer.player.id).await.unwrap();
    world
        .plugin_inventory(
            "owner".into(),
            id,
            42,
            InventoryOperation::Give {
                item: "minecraft:dirt".into(),
                count: 5,
            },
        )
        .await
        .unwrap();
    let notification = normal.recv().await.unwrap();
    assert!(
        matches!(notification.as_ref(),WorldEvent::InventoryUpdated {player_id} if *player_id==id)
    );
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::InventoryChanged {player_id,..} if *player_id==id)
    );
    let response = hooks.recv().await.unwrap();
    assert!(
        matches!(response.as_ref(),WorldEvent::PluginInventoryResult {plugin,player_id,request_id:42,result:InventoryResult::Snapshot {inventory}} if plugin=="owner" && *player_id==id && inventory.slots[3].as_ref().unwrap().count==5)
    );
    assert!(normal.try_recv().is_err() && peers.updates.try_recv().is_err());
    // Consume a later client mutation before handling the queued server push.
    world
        .creative_slot(id, 3, Some(BlockKind::Bedrock), 7)
        .await
        .unwrap();
    assert!(matches!(
        hooks.recv().await.unwrap().as_ref(),
        WorldEvent::InventoryChanged { .. }
    ));
    let WorldEvent::InventoryUpdated { player_id } = notification.as_ref() else {
        unreachable!()
    };
    assert_eq!(
        world.player_snapshot(*player_id).await.unwrap().inventory[3]
            .unwrap()
            .kind,
        BlockKind::Bedrock
    );
    world
        .plugin_inventory("owner".into(), id, 43, InventoryOperation::Get)
        .await
        .unwrap();
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::PluginInventoryResult {request_id:43,result:InventoryResult::Snapshot {inventory},..} if inventory.slots[3].as_ref().unwrap().count==7)
    );
    assert!(
        normal.try_recv().is_err()
            && peers.updates.try_recv().is_err()
            && hooks.try_recv().is_err()
    );
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    assert!(
        world
            .command_specs()
            .await
            .unwrap()
            .iter()
            .all(|c| c.name != "kit" && !c.name.starts_with("sculith_test_"))
    );
    drop(actor);
    world
        .plugin_inventory("owner".into(), id, 44, InventoryOperation::Get)
        .await
        .unwrap();
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::Left {id:retired} if *retired==id)
    );
    assert!(matches!(
        hooks.recv().await.unwrap().as_ref(),
        WorldEvent::PluginInventoryResult {
            request_id: 44,
            result: InventoryResult::Rejected {
                reason: InventoryError::PlayerUnavailable
            },
            ..
        }
    ));
    assert!(world.player_snapshot(id).await.is_err());
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn inventory_commits_are_private_with_counted_public_hands_and_idempotent_replays() {
    use sculith_game::inventory::{Action, Request, Slot, SlotRef};
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let actor = world
        .reserve(profile("inventory"), Permissions { can_build: true })
        .await
        .unwrap();
    let viewer = world
        .reserve(profile("viewer"), Permissions::default())
        .await
        .unwrap();
    let id = actor.player.id;
    assert!(
        world
            .creative_slot(id, 0, Some(BlockKind::Dirt), 5)
            .await
            .is_err()
    );
    world.activate(id).await.unwrap();
    world.activate(viewer.player.id).await.unwrap();
    seed_inventory(&world, id).await;
    world.configure_observations(crate::observations::INVENTORY | crate::observations::EQUIPMENT);
    let mut hooks = world.subscribe_game();
    let mut normal = world.subscribe();
    let mut peers = world.observe_players(viewer.player.id).await.unwrap();
    let revision = world.capture().await.unwrap().revision();
    let request = Request {
        id: -1,
        actions: vec![
            Action::Destroy {
                source: SlotRef {
                    slot: Slot::Inventory(0),
                    expected_id: 1,
                },
                count: 64,
            },
            Action::CraftCreative { catalogue_id: 2 },
            Action::Transfer {
                source: SlotRef {
                    slot: Slot::CreatedOutput,
                    expected_id: -1,
                },
                destination: SlotRef {
                    slot: Slot::Inventory(0),
                    expected_id: 0,
                },
                count: 5,
            },
        ],
    };
    let outcome = world.inventory_request(id, request.clone()).await.unwrap();
    assert!(outcome.response.result.is_ok());
    assert_eq!(
        outcome.player.held().map(|s| (s.kind, s.count)),
        Some((BlockKind::Dirt, 5))
    );
    assert_eq!(outcome.player.hotbar[0], Some(BlockKind::Dirt));
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::InventoryChanged {player_id,..} if *player_id==id)
    );
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.held().unwrap().count==5)
    );
    assert!(
        matches!(peers.updates.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.id==id)
    );
    assert!(normal.try_recv().is_err() && peers.updates.try_recv().is_err());
    assert!(
        world
            .inventory_request(id, request)
            .await
            .unwrap()
            .response
            .replayed
    );
    assert!(hooks.try_recv().is_err() && peers.updates.try_recv().is_err());
    let invalid = Request {
        id: -2,
        actions: vec![Action::Destroy {
            source: SlotRef {
                slot: Slot::Inventory(0),
                expected_id: 99999,
            },
            count: 5,
        }],
    };
    let rejected = world.inventory_request(id, invalid).await.unwrap();
    assert!(rejected.response.result.is_err());
    assert_eq!(rejected.player.inventory, outcome.player.inventory);
    assert!(hooks.try_recv().is_err() && peers.updates.try_recv().is_err());
    for request in [
        Request {
            id: -3,
            actions: vec![Action::Mine {
                source: SlotRef {
                    slot: Slot::Inventory(0),
                    expected_id: outcome.player.held().unwrap().net_id,
                },
            }],
        },
        Request {
            id: -4,
            actions: vec![Action::Swap {
                source: SlotRef {
                    slot: Slot::Inventory(10),
                    expected_id: 0,
                },
                destination: SlotRef {
                    slot: Slot::Inventory(11),
                    expected_id: 0,
                },
            }],
        },
    ] {
        let response = world.inventory_request(id, request).await.unwrap();
        assert!(!response.response.result.unwrap().is_empty());
        assert!(hooks.try_recv().is_err() && peers.updates.try_recv().is_err());
    }
    assert_eq!(world.capture().await.unwrap().revision(), revision);
    assert!(
        world
            .creative_slot(viewer.player.id, 0, Some(BlockKind::Grass), 2)
            .await
            .unwrap()
            .response
            .result
            .is_err()
    );
    drop(actor);
    assert!(
        world
            .creative_slot(id, 0, Some(BlockKind::Stone), 1)
            .await
            .is_err()
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn command_arguments_only_enter_the_registered_plugin_stream() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    world
        .register_commands(
            "owner".into(),
            vec![sculith_core::CommandSpec {
                name: "hello".into(),
                description: "Say hello".into(),
            }],
        )
        .await
        .unwrap();
    let lease = world
        .reserve(profile("author"), Permissions::default())
        .await
        .unwrap();
    let id = lease.player.id;
    assert!(
        world
            .submit_command(id, "hello secret".into())
            .await
            .is_err()
    );
    world.activate(id).await.unwrap();
    let mut public = world.subscribe();
    let mut hooks = world.subscribe_game();
    let mut peers = world.observe_players(id).await.unwrap();
    world
        .submit_command(id, "/hello secret 😀".into())
        .await
        .unwrap();
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::CommandSubmitted(c) if c.player_id==id && c.plugin=="owner" && c.command=="hello" && c.arguments=="secret 😀" && c.name=="author")
    );
    assert!(public.try_recv().is_err() && peers.updates.try_recv().is_err());
    world.disable_commands("owner".into()).await.unwrap();
    world
        .submit_command(id, "hello secret".into())
        .await
        .unwrap();
    assert!(hooks.try_recv().is_err());
    assert!(
        matches!(public.recv().await.unwrap().as_ref(),WorldEvent::Message {player_id,message} if *player_id==id && message=="This command is temporarily unavailable.")
    );
    drop(lease);
    assert!(world.submit_command(id, "help".into()).await.is_err());
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn chat_requires_an_initialized_live_author_and_rejects_private_commands() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let lease = world
        .reserve(profile("author"), Permissions::default())
        .await
        .unwrap();
    let id = lease.player.id;
    assert!(world.chat(id, "before join".into()).await.is_err());
    world.activate(id).await.unwrap();
    let mut events = world.subscribe();
    let mut hooks = world.subscribe_game();
    let mut peers = world.observe_players(id).await.unwrap();
    let revision = world.capture().await.unwrap().revision();
    assert!(world.chat(id, "/password secret".into()).await.is_err());
    assert!(world.chat(id, "line\nbreak".into()).await.is_err());
    assert!(events.try_recv().is_err() && hooks.try_recv().is_err());
    world.chat(id, "안녕 😀".into()).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap().as_ref(),WorldEvent::Chat(c) if c.player_id==id && c.name=="author" && c.message=="안녕 😀")
    );
    assert!(matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::Chat(c) if c.player_id==id));
    assert!(peers.updates.try_recv().is_err());
    world.chat(id, "last burst".into()).await.unwrap();
    assert!(world.chat(id, "over budget".into()).await.is_err());
    assert_eq!(world.capture().await.unwrap().revision(), revision);
    drop(lease);
    assert!(world.chat(id, "retired".into()).await.is_err());
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opted_in_observations_are_owner_validated_and_never_enter_private_or_peer_streams() {
    use sculith_core::{PacketDispatch, PacketInfo};
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let mut identity = profile("observer");
    identity.edition = Edition::Java;
    let lease = world
        .reserve(identity, Permissions::default())
        .await
        .unwrap();
    let id = lease.player.id;
    let packet = PacketInfo {
        edition: sculith_core::Edition::Java,
        protocol: 777,
        phase: "play".into(),
        id: 22,
        name: "minecraft:custom_payload".into(),
        dispatch: PacketDispatch::Unsupported,
        sequence: 1,
    };
    world.configure_observations(crate::observations::ALL);
    assert!(world.observe_packet(id, packet.clone()).await.is_err());
    world.activate(id).await.unwrap();
    let mut game = world.subscribe_game();
    let mut normal = world.subscribe();
    let mut peers = world.observe_players(id).await.unwrap();
    let revision = world.capture().await.unwrap().revision();
    let mut wrong_edition = packet.clone();
    wrong_edition.edition = sculith_core::Edition::Bedrock;
    assert!(world.observe_packet(id, wrong_edition).await.is_err());
    assert!(game.try_recv().is_err());
    world.observe_packet(id, packet.clone()).await.unwrap();
    assert!(
        matches!(game.recv().await.unwrap().as_ref(),WorldEvent::PacketReceived {player_id,packet} if *player_id==id && packet.name=="minecraft:custom_payload")
    );
    assert!(normal.try_recv().is_err());
    assert!(peers.updates.try_recv().is_err());
    world.select_slot(id, 1).await.unwrap();
    assert!(
        matches!(game.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.selected_slot==1)
    );
    world.send_message(id, "private".into()).await.unwrap();
    assert!(game.try_recv().is_err());
    assert_eq!(world.capture().await.unwrap().revision(), revision);
    world.configure_observations(0);
    world.observe_packet(id, packet.clone()).await.unwrap();
    world.select_slot(id, 2).await.unwrap();
    assert!(game.try_recv().is_err());
    world.configure_observations(crate::observations::PACKETS);
    drop(lease);
    assert!(world.observe_packet(id, packet).await.is_err());
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn held_slot_changes_publish_only_committed_public_equipment() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let viewer = world
        .reserve(profile("viewer"), Permissions::default())
        .await
        .unwrap();
    let peer = world
        .reserve(profile("peer"), Permissions::default())
        .await
        .unwrap();
    world.activate(viewer.player.id).await.unwrap();
    assert!(world.select_slot(peer.player.id, 1).await.is_err());
    world.activate(peer.player.id).await.unwrap();
    seed_inventory(&world, peer.player.id).await;
    let mut observation = world.observe_players(viewer.player.id).await.unwrap();
    let mut normal = world.subscribe();
    let mut game = world.subscribe_game();
    let revision = world.capture().await.unwrap().revision();
    let selected = world.select_slot(peer.player.id, 1).await.unwrap();
    assert_eq!(selected.hotbar[1], Some(BlockKind::Dirt));
    assert!(
        matches!(observation.updates.recv().await.unwrap().as_ref(),WorldEvent::Equipped(p) if p.id==peer.player.id && p.selected_slot==1 && p.hotbar[1]==Some(BlockKind::Dirt))
    );
    world.select_slot(peer.player.id, 1).await.unwrap();
    assert!(world.select_slot(peer.player.id, 9).await.is_err());
    assert!(observation.updates.try_recv().is_err());
    assert!(normal.try_recv().is_err());
    assert!(game.try_recv().is_err());
    assert_eq!(world.capture().await.unwrap().revision(), revision);
    let fresh = world.observe_players(viewer.player.id).await.unwrap();
    assert_eq!(fresh.players[0].selected_slot, 1);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

pub fn profile(name: &str) -> Profile {
    Profile {
        subject: format!("test:{name}"),
        name: name.into(),
        uuid: uuid::Uuid::new_v4(),
        edition: Edition::Bedrock,
    }
}

#[tokio::test]
async fn peer_observation_has_an_atomic_initial_membership_boundary_and_no_pending_players() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let viewer = world
        .reserve(profile("viewer"), Permissions::default())
        .await
        .unwrap();
    assert!(world.observe_players(viewer.player.id).await.is_err());
    world.activate(viewer.player.id).await.unwrap();
    let peer = world
        .reserve(profile("peer"), Permissions::default())
        .await
        .unwrap();
    let mut first = world.observe_players(viewer.player.id).await.unwrap();
    assert!(first.players.is_empty());
    world.activate(peer.player.id).await.unwrap();
    assert!(
        matches!(first.updates.recv().await.unwrap().as_ref(),WorldEvent::Joined(p) if p.id==peer.player.id)
    );
    let mut second = world.observe_players(viewer.player.id).await.unwrap();
    assert_eq!(second.players.len(), 1);
    assert_eq!(second.players[0].id, peer.player.id);
    assert!(matches!(
        second.updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    world
        .send_message(viewer.player.id, "private".into())
        .await
        .unwrap();
    assert!(matches!(
        second.updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let id = peer.player.id;
    drop(peer);
    // The next mailbox command first retires dead leases and then snapshots.
    let mut third = world.observe_players(viewer.player.id).await.unwrap();
    assert!(third.players.is_empty());
    assert!(
        matches!(second.updates.recv().await.unwrap().as_ref(),WorldEvent::Left {id:current} if *current==id)
    );
    assert!(matches!(
        third.updates.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let id = viewer.player.id;
    drop(viewer);
    assert!(world.observe_players(id).await.is_err());
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn model_effects_are_private_presentation_and_hide_is_idempotent() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = with_region_and_content(
        Region::flat_origin(),
        20,
        receiver,
        Default::default(),
        ["sculith:demo_statue".into()].into(),
    )
    .unwrap();
    let task = tokio::spawn(task);
    let lease = world
        .reserve(profile("model"), Permissions { can_build: true })
        .await
        .unwrap();
    let id = lease.player.id;
    world.activate(id).await.unwrap();
    let mut events = world.subscribe();
    let mut hooks = world.subscribe_game();
    world
        .show_model(id, "sculith:demo_statue".into())
        .await
        .unwrap();
    let event = events.recv().await.unwrap();
    let WorldEvent::ModelShown(view) = event.as_ref() else {
        panic!("missing model")
    };
    assert_eq!(view.player_id, id);
    let entity_id = view.entity_id;
    world.hide_model(id).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap().as_ref(),WorldEvent::ModelHidden {player_id,entity_id:current} if *player_id==id && *current==entity_id)
    );
    world.hide_model(id).await.unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        hooks.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    drop(lease);
    assert!(world.hide_model(id).await.is_err());
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn private_sound_requires_live_initialized_registered_player_and_never_changes_world_or_game_hooks()
 {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = with_region_and_sounds(
        Region::flat_origin(),
        20,
        receiver,
        ["sculith:demo_tone".into()].into(),
    )
    .unwrap();
    let task = tokio::spawn(task);
    let lease = world
        .reserve(profile("sound"), Permissions { can_build: true })
        .await
        .unwrap();
    let id = lease.player.id;
    assert!(
        world
            .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
            .await
            .is_err()
    );
    world.activate(id).await.unwrap();
    let mut events = world.subscribe();
    let mut hooks = world.subscribe_game();
    let revision = world.capture().await.unwrap().revision();
    assert!(
        world
            .play_sound(id, "sculith:missing".into(), 0.7, 1.0)
            .await
            .is_err()
    );
    assert!(
        world
            .play_sound(id, "sculith:demo_tone".into(), f32::NAN, 1.0)
            .await
            .is_err()
    );
    for _ in 0..4 {
        world
            .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
            .await
            .unwrap();
        let event = events.recv().await.unwrap();
        let WorldEvent::Sound(effect) = event.as_ref() else {
            panic!("wrong effect")
        };
        assert_eq!(effect.player_id, id);
        assert_eq!(effect.position.x, lease.player.feet.x);
        assert_eq!(effect.position.y, lease.player.feet.y + 1.62);
    }
    assert!(
        world
            .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
            .await
            .is_err()
    );
    assert!(matches!(
        hooks.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(world.capture().await.unwrap().revision(), revision);
    drop(lease);
    assert!(
        world
            .play_sound(id, "sculith:demo_tone".into(), 0.7, 1.0)
            .await
            .is_err()
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_intent_queued_before_lease_drop_cannot_commit_after_disconnect() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let lease = world
        .reserve(profile("closed"), Permissions { can_build: true })
        .await
        .unwrap();
    let id = lease.player.id;
    world.activate(id).await.unwrap();
    let (reply, response) = oneshot::channel();
    world
        .commands
        .try_send(Command::Break {
            id,
            position: BlockPos { x: 1, y: 3, z: 1 },
            reply,
        })
        .unwrap();
    // No yield between enqueue and drop: the owner must see the dead lease
    // before handling this already queued request, not just before recv().
    drop(lease);
    assert!(matches!(
        response.await.unwrap(),
        BlockOutcome::Rejected {
            reason: sculith_game::RejectReason::UnknownPlayer,
            ..
        }
    ));
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    stop.send_replace(true);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn committed_runtime_state_survives_snapshot_reload() {
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let lease = world
        .reserve(profile("saved"), Permissions { can_build: true })
        .await
        .unwrap();
    world.activate(lease.player.id).await.unwrap();
    let position = BlockPos { x: 1, y: 3, z: 1 };
    assert!(matches!(
        world.break_block(lease.player.id, position).await.unwrap(),
        BlockOutcome::Committed { .. }
    ));
    let capture = world.capture().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("region.sculith");
    sculith_world::storage::save_snapshot(&path, &capture).unwrap();
    let region = sculith_world::storage::load(&path).unwrap();
    assert_eq!(region.revision(), 1);
    assert_eq!(region.get_block(position).unwrap(), BlockKind::Air);
    drop(lease);
    stop.send_replace(true);
    task.await.unwrap().unwrap();
    let (stop, receiver) = watch::channel(false);
    let (restored, task) = with_region(region, 20, receiver).unwrap();
    let task = tokio::spawn(task);
    assert_eq!(
        restored
            .snapshot(ChunkPos { x: 0, z: 0 })
            .await
            .unwrap()
            .get(1, 3, 1)
            .unwrap(),
        BlockKind::Air
    );
    stop.send_replace(true);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn private_messages_require_a_live_initialized_lease_and_never_reenter_game_hooks() {
    let (shutdown, stop) = watch::channel(false);
    let (world, task) = create(stop);
    let task = tokio::spawn(task);
    let mut hooks = world.subscribe_game();
    let lease = world
        .reserve(profile("recipient"), Permissions { can_build: true })
        .await
        .unwrap();
    let id = lease.player.id;
    assert!(world.send_message(id, "pending".into()).await.is_err());
    world.activate(id).await.unwrap();
    assert!(matches!(hooks.recv().await.unwrap().as_ref(), WorldEvent::Joined(p) if p.id == id));
    let mut view = world.subscribe();
    assert!(world.send_message(id, "bad\ntext".into()).await.is_err());
    world
        .send_message(id, "환영합니다 👋".into())
        .await
        .unwrap();
    assert!(
        matches!(view.recv().await.unwrap().as_ref(), WorldEvent::Message { player_id, message } if *player_id == id && message == "환영합니다 👋")
    );
    assert!(matches!(
        hooks.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    drop(lease);
    assert!(world.send_message(id, "closed".into()).await.is_err());
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(), WorldEvent::Left { id: left } if *left == id)
    );
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn form_results_are_private_semantic_events_and_dead_leases_cannot_reply() {
    let (shutdown, stop) = watch::channel(false);
    let (world, task) = create(stop);
    let task = tokio::spawn(task);
    let lease = world
        .reserve(profile("forms"), Permissions::default())
        .await
        .unwrap();
    let id = lease.player.id;
    let form = sculith_core::SimpleForm {
        title: "Menu".into(),
        body: "Choose".into(),
        buttons: vec!["A".into()],
    };
    assert!(
        world
            .show_form(id, "owner".into(), 7, form.clone())
            .await
            .is_err()
    );
    world.activate(id).await.unwrap();
    let mut view = world.subscribe();
    let mut hooks = world.subscribe_game();
    world
        .show_form(id, "owner".into(), 7, form.clone())
        .await
        .unwrap();
    let event = view.recv().await.unwrap();
    let WorldEvent::FormRequested { form_id, .. } = event.as_ref() else {
        panic!("expected private form presentation")
    };
    assert!(matches!(
        hooks.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    world.submit_form(id, *form_id, Some(0)).await.unwrap();
    assert!(
        matches!(hooks.recv().await.unwrap().as_ref(),WorldEvent::FormSubmitted(reply) if reply.plugin=="owner" && reply.request_id==7 && reply.selection==Some(0))
    );
    assert!(matches!(
        view.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(world.submit_form(id, *form_id, Some(0)).await.is_err());
    drop(lease);
    assert!(world.show_form(id, "owner".into(), 8, form).await.is_err());
    assert!(world.submit_form(id, *form_id, None).await.is_err());
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn committed_changes_are_shared_and_a_dropped_lease_cleans_up() {
    let (shutdown, stop) = watch::channel(false);
    let (world, task) = create(stop);
    let task = tokio::spawn(task);
    let player = world
        .reserve(profile("builder"), Permissions { can_build: true })
        .await
        .unwrap();
    world.activate(player.player.id).await.unwrap();
    let mut first = world.subscribe();
    let mut second = world.subscribe();
    let old = world.snapshot(ChunkPos { x: 0, z: 0 }).await.unwrap();
    let position = BlockPos { x: 1, y: 3, z: 1 };
    assert!(matches!(
        world.break_block(player.player.id, position).await.unwrap(),
        BlockOutcome::Committed { .. }
    ));
    let a = first.recv().await.unwrap();
    let b = second.recv().await.unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(old.get(1, 3, 1).unwrap(), BlockKind::Grass);
    assert_eq!(
        world
            .snapshot(ChunkPos { x: 0, z: 0 })
            .await
            .unwrap()
            .get(1, 3, 1)
            .unwrap(),
        BlockKind::Air
    );
    drop(player);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while world.online_players() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    shutdown.send_replace(true);
    task.await.unwrap().unwrap();
}
