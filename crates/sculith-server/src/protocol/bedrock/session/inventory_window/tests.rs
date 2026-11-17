use super::*;
use bedrock::protocol::{v662::types::ActorRuntimeID, v898::packets::InteractPacket};
use sculith_game::{Edition, GameState, Permissions, Profile};
use sculith_world::Region;
use std::{mem::size_of, time::Duration};

fn player() -> PlayerSnapshot {
    let now = Instant::now();
    let mut game = GameState::new(Region::flat_origin(), 1).unwrap();
    let player = game
        .reserve(
            Profile {
                subject: "offline:inventory-window".into(),
                name: "window".into(),
                uuid: uuid::Uuid::new_v4(),
                edition: Edition::Bedrock,
            },
            Permissions { can_build: true },
            now,
        )
        .unwrap();
    game.activate(player.id).unwrap()
}

fn open(id: u64) -> V2225 {
    V2225::InteractPacket(Box::new(InteractPacket {
        action: InteractPacketAction::OpenInventory,
        target_runtime_id: ActorRuntimeID(id),
        mouse_position: None,
    }))
}

fn close(id: i8) -> V2225 {
    V2225::ContainerClosePacket(Box::new(ContainerClosePacket {
        container_id: ContainerID(id),
        container_type: ContainerType::Inventory,
        server_initiated_close: false,
    }))
}

fn opened(reply: Option<V2225>) -> i8 {
    let Some(V2225::ContainerOpenPacket(reply)) = reply else {
        panic!("expected inventory open")
    };
    assert!(matches!(reply.container_type, ContainerType::Inventory));
    assert_eq!(reply.target_actor_id.0, -1);
    reply.container_id.0
}

#[test]
fn self_inventory_opens_once_and_only_its_current_window_can_close() {
    let now = Instant::now();
    let mut state = InventoryWindow::new(now);
    let mut player = player();
    player.feet.x = -0.5;
    let request = open(player.id);
    let Some(V2225::ContainerOpenPacket(reply)) = state.handle(&request, &player, now) else {
        panic!("expected open")
    };
    assert_eq!((reply.container_id.0, reply.position.x), (2, -1));
    assert!(state.handle(&request, &player, now).is_none());
    assert!(state.handle(&close(3), &player, now).is_none());
    for (kind, server) in [
        (ContainerType::Container, false),
        (ContainerType::Inventory, true),
    ] {
        let invalid = V2225::ContainerClosePacket(Box::new(ContainerClosePacket {
            container_id: ContainerID(2),
            container_type: kind,
            server_initiated_close: server,
        }));
        assert!(state.handle(&invalid, &player, now).is_none());
    }
    let Some(V2225::ContainerClosePacket(reply)) = state.handle(&close(2), &player, now) else {
        panic!("expected close")
    };
    assert_eq!(reply.container_id.0, 2);
    assert!(!reply.server_initiated_close);
    assert!(matches!(reply.container_type, ContainerType::Inventory));
    assert!(state.handle(&close(2), &player, now).is_none());
    assert_eq!(opened(state.handle(&request, &player, now)), 3);
    assert!(state.handle(&close(2), &player, now).is_none());
    assert!(state.handle(&request, &player, now).is_none());
    let mut none_close = close(3);
    if let V2225::ContainerClosePacket(p) = &mut none_close {
        p.container_type = ContainerType::None;
    }
    assert!(state.handle(&none_close, &player, now).is_some());
}

#[test]
fn wrong_actor_loading_and_other_interactions_do_not_create_or_spend_windows() {
    let now = Instant::now();
    let mut state = InventoryWindow::new(now);
    let mut player = player();
    player.initialized = false;
    assert!(state.handle(&open(player.id), &player, now).is_none());
    player.initialized = true;
    for id in [0, player.id + 1] {
        assert!(state.handle(&open(id), &player, now).is_none());
    }
    for action in [
        InteractPacketAction::InteractUpdate,
        InteractPacketAction::Damage,
        InteractPacketAction::Invalid,
    ] {
        let request = V2225::InteractPacket(Box::new(InteractPacket {
            action,
            target_runtime_id: ActorRuntimeID(player.id),
            mouse_position: None,
        }));
        assert!(state.handle(&request, &player, now).is_none());
        assert_eq!(
            crate::protocol::bedrock::catalog::receipt(&request, 1).dispatch,
            sculith_core::PacketDispatch::Unsupported
        );
    }
    assert_eq!(opened(state.handle(&open(player.id), &player, now)), 2);
    assert_eq!(state.credits, 15.0);
    for request in [open(player.id), close(2)] {
        assert_eq!(
            crate::protocol::bedrock::catalog::receipt(&request, 1).dispatch,
            sculith_core::PacketDispatch::Control
        );
    }
}

#[test]
fn response_budget_keeps_close_usable_and_window_ids_wrap_in_fixed_storage() {
    let now = Instant::now();
    let mut state = InventoryWindow::new(now);
    let player = player();
    let request = open(player.id);
    for expected in 2..18 {
        let id = opened(state.handle(&request, &player, now));
        assert_eq!(id, expected);
        assert!(state.handle(&close(id), &player, now).is_some());
    }
    assert!(state.handle(&request, &player, now).is_none());
    assert_eq!(state.next, 18);
    assert!(
        state
            .handle(&request, &player, now - Duration::from_secs(1))
            .is_none()
    );
    let later = now + Duration::from_secs(1);
    for _ in 0..8 {
        let id = opened(state.handle(&request, &player, later));
        assert!(state.handle(&close(id), &player, later).is_some());
    }
    assert!(state.handle(&request, &player, later).is_none());
    for elapsed in 2..252 {
        let time = now + Duration::from_secs(elapsed);
        let id = opened(state.handle(&request, &player, time));
        assert!((FIRST_WINDOW..=LAST_WINDOW).contains(&id));
        assert_eq!(id, FIRST_WINDOW + ((24 + elapsed - 2) % 99) as i8);
        assert!(state.handle(&close(id), &player, time).is_some());
    }
    assert!(size_of::<InventoryWindow>() <= 64);
    println!("inventory_window_bytes={}", size_of::<InventoryWindow>());
}
