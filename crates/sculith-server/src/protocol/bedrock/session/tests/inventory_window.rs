use super::*;

async fn inventory_window_cycle<T: Transport>(
    client: &mut TestClient<T>,
    world: &crate::world::WorldHandle,
    id: u64,
) {
    use bedrock::protocol::{
        v662::{enums::ContainerID, packets::NetworkStackLatencyPacket},
        v685::packets::ContainerClosePacket,
        v898::packets::{InteractPacket, InteractPacketAction},
        v2225::enums::ContainerType,
    };
    let open = |target| {
        V2225::InteractPacket(Box::new(InteractPacket {
            action: InteractPacketAction::OpenInventory,
            target_runtime_id: ActorRuntimeID(target),
            mouse_position: None,
        }))
    };
    let close = |window, kind, server| {
        V2225::ContainerClosePacket(Box::new(ContainerClosePacket {
            container_id: ContainerID(window),
            container_type: kind,
            server_initiated_close: server,
        }))
    };
    let barrier = |stamp| {
        V2225::NetworkStackLatencyPacket(Box::new(NetworkStackLatencyPacket {
            creation_time: stamp,
            is_from_server: true,
        }))
    };
    // Open as the first play request, before inventory/movement outcomes can
    // refresh the connection's cached activation snapshot.
    client.conn.send(&[open(id)]).await.unwrap();
    let V2225::ContainerOpenPacket(reply) = client.next().await.unwrap() else {
        panic!("missing initial open")
    };
    assert_eq!(reply.container_id.0, 2);
    assert!(matches!(reply.container_type, ContainerType::Inventory));
    assert_eq!(reply.target_actor_id.0, -1);
    client.seed_stone().await.unwrap();
    world
        .inventory_click(
            id,
            sculith_game::inventory::Click::Pickup {
                slot: 0,
                right: true,
            },
        )
        .await
        .unwrap();
    world
        .inventory_request(
            id,
            sculith_game::inventory::Request {
                id: -2,
                actions: vec![sculith_game::inventory::Action::CraftCreative { catalogue_id: 3 }],
            },
        )
        .await
        .unwrap();
    let before = world.player_snapshot(id).await.unwrap();
    assert_eq!(before.cursor.unwrap().count, 32);
    assert_eq!(before.created_output.unwrap().count, 64);
    let revision = world.capture().await.unwrap().revision();
    client
        .conn
        .send(&[
            open(id),
            open(id + 1000),
            close(77, ContainerType::Inventory, false),
            close(2, ContainerType::Workbench, false),
            close(2, ContainerType::Inventory, true),
            barrier(11),
        ])
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap(), V2225::NetworkStackLatencyPacket(p) if p.creation_time==11 && !p.is_from_server)
    );
    client
        .conn
        .send(&[close(2, ContainerType::Inventory, false), open(id)])
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap(), V2225::ContainerClosePacket(p) if p.container_id.0==2 && !p.server_initiated_close)
    );
    assert!(
        matches!(client.next().await.unwrap(), V2225::ContainerOpenPacket(p) if p.container_id.0==3)
    );
    client
        .conn
        .send(&[
            close(2, ContainerType::Inventory, false),
            open(id),
            barrier(12),
        ])
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap(), V2225::NetworkStackLatencyPacket(p) if p.creation_time==12)
    );
    client
        .conn
        .send(&[close(3, ContainerType::None, false), barrier(13)])
        .await
        .unwrap();
    assert!(
        matches!(client.next().await.unwrap(), V2225::ContainerClosePacket(p) if p.container_id.0==3)
    );
    assert!(
        matches!(client.next().await.unwrap(), V2225::NetworkStackLatencyPacket(p) if p.creation_time==13)
    );
    assert_eq!(world.player_snapshot(id).await.unwrap(), before);
    assert_eq!(world.capture().await.unwrap().revision(), revision);
}

#[tokio::test]
async fn inventory_windows_keep_owner_stacks_and_ignore_duplicates_over_both_transports() {
    use crate::protocol::bedrock::nethernet::{ContextData, prepare};
    let server = Harness::start(Authentication::offline(), Duration::from_secs(15)).await;
    let directory = tempfile::tempdir().unwrap();
    let nether = prepare(
        "127.0.0.1:0".parse().unwrap(),
        &directory.path().join("identity.pem"),
        &Authentication::offline(),
        None,
    )
    .await
    .unwrap();
    let address = nether.local_addr().unwrap();
    let task = tokio::spawn(nether.serve(ContextData {
        settings: crate::protocol::bedrock::session::Settings {
            authentication: Authentication::offline(),
            timeout: Duration::from_secs(15),
            content: None,
            observer: crate::connections::Observer::default(),
        },
        world: server.world.clone(),
        status: server._status.subscribe(),
        shutdown: server.shutdown.subscribe(),
    }));
    let mut raknet = TestClient::connect(server.address).await.unwrap();
    let id = raknet.enter().await.unwrap();
    inventory_window_cycle(&mut raknet, &server.world, id).await;
    raknet.conn.close().await;
    let mut nethernet = nether_client(address).await.unwrap();
    let id = nethernet.enter().await.unwrap();
    inventory_window_cycle(&mut nethernet, &server.world, id).await;
    nethernet.conn.close().await;
    server.stop().await;
    task.await.unwrap().unwrap();
}
