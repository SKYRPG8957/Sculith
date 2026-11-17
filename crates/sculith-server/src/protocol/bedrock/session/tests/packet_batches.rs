use super::*;
use bedrock::protocol::v662::packets::NetworkStackLatencyPacket;

fn controls(count: usize) -> Vec<V2225> {
    let mut batch = vec![
        V2225::ClientCacheStatusPacket(Box::new(ClientCacheStatusPacket {
            is_cache_supported: false,
        }));
        count - 1
    ];
    batch.push(V2225::NetworkStackLatencyPacket(Box::new(
        NetworkStackLatencyPacket {
            creation_time: count as u64,
            is_from_server: true,
        },
    )));
    batch
}

#[tokio::test]
async fn native_batches_above_64_preserve_tail_and_subsequent_gameplay() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    let id = client.enter().await.unwrap();
    for count in [67, 129, bedrock::network::codec::MAX_BATCH_PACKETS] {
        client.conn.send(&controls(count)).await.unwrap();
        assert!(
            matches!(client.next().await.unwrap(), V2225::NetworkStackLatencyPacket(p)
            if p.creation_time == count as u64 && !p.is_from_server)
        );
        assert_eq!(server.world.online_players(), 1);
        assert_eq!(server.world.capture().await.unwrap().revision(), 0);
    }
    client.request_break(id, 1, 3, 1).await.unwrap();
    assert!(matches!(
        client.next().await.unwrap(),
        V2225::UpdateBlockPacket(_)
    ));
    assert_eq!(server.world.capture().await.unwrap().revision(), 1);
    client.conn.close().await;
    server.stop().await;
}

#[tokio::test]
async fn native_batches_still_reject_codec_frame_overflow() {
    let server = Harness::start(Authentication::offline(), Duration::from_secs(10)).await;
    let mut client = TestClient::connect(server.address).await.unwrap();
    client.enter().await.unwrap();
    client
        .conn
        .send(&controls(bedrock::network::codec::MAX_BATCH_PACKETS + 1))
        .await
        .unwrap();
    assert!(client.next().await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.world.online_players() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.world.capture().await.unwrap().revision(), 0);
    client.conn.close().await;
    server.stop().await;
}
