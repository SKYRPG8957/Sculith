#[allow(clippy::wildcard_imports)]
use super::*;
use crate::net::bedrock::BedrockVersions;
use pumpkin_config::networking::compression::CompressionInfo;
use std::sync::atomic::Ordering;

const INCOMPATIBLE_PROTOCOL_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Bedrock 1.20.60 (protocol 649) added a 1-byte compression method prefix after `0xFE`.
const PREFIXED_COMPRESSION_MIN_PROTOCOL: u32 = 649;

fn incompatible_protocol_status(
    client_network_version: i32,
    versions: &BedrockVersions,
) -> Option<CPlayStatus> {
    if let Ok(protocol) = u32::try_from(client_network_version)
        && versions.admits_protocol(protocol)
    {
        return None;
    }
    match client_network_version.cmp(&(CURRENT_BEDROCK_MC_PROTOCOL as i32)) {
        std::cmp::Ordering::Less => Some(CPlayStatus::OutdatedClient),
        std::cmp::Ordering::Greater => Some(CPlayStatus::OutdatedServer),
        std::cmp::Ordering::Equal => None,
    }
}

impl BedrockClient {
    async fn reject_incompatible_protocol(&self, status: CPlayStatus) {
        self.send_packet(&status).await;
        if let Err(error) = self
            .session
            .flush_reliable(INCOMPATIBLE_PROTOCOL_FLUSH_TIMEOUT)
            .await
        {
            debug!(
                address = %self.address,
                %error,
                "Failed to flush Bedrock version rejection"
            );
        }
        self.close().await;
    }

    /// Admits a pre-1.19.30 client that sends `SLogin` directly without `SRequestNetworkSettings`.
    pub async fn admit_legacy_login_protocol(
        &self,
        client_network_version: i32,
        server: &Server,
    ) -> bool {
        if let Some(status) = incompatible_protocol_status(
            client_network_version,
            &server.plugin_manager.bedrock_versions,
        ) {
            self.reject_incompatible_protocol(status).await;
            return false;
        }
        let protocol = client_network_version as u32;
        self.protocol_version.store(protocol, Ordering::Relaxed);
        self.version
            .store(BedrockMinecraftVersion::from_protocol(protocol));
        self.set_prefixed_compression(protocol >= PREFIXED_COMPRESSION_MIN_PROTOCOL)
            .await;
        self.set_compression(
            server
                .advanced_config
                .networking
                .bedrock
                .compression
                .bedrock_settings(),
        )
        .await;
        true
    }

    pub async fn handle_request_network_settings(
        &self,
        packet: SRequestNetworkSettings,
        server: &Server,
    ) -> bool {
        if let Some(status) = incompatible_protocol_status(
            packet.client_network_version,
            &server.plugin_manager.bedrock_versions,
        ) {
            self.reject_incompatible_protocol(status).await;
            return false;
        }

        let protocol = packet.client_network_version as u32;
        self.protocol_version.store(protocol, Ordering::Relaxed);
        self.version
            .store(BedrockMinecraftVersion::from_protocol(protocol));
        self.set_prefixed_compression(protocol >= PREFIXED_COMPRESSION_MIN_PROTOCOL)
            .await;

        let compression = server
            .advanced_config
            .networking
            .bedrock
            .compression
            .bedrock_settings();
        self.send_network_settings(compression).await;
        true
    }

    async fn send_network_settings(&self, compression: CompressionInfo) {
        // The peer can send a compressed Login as soon as it receives the settings.
        self.set_inbound_compression(compression.threshold).await;

        // Send the settings with the initial framing before switching the encoder.
        self.send_packet(&CNetworkSettings {
            compression_threshold: compression.threshold as u16,
            compression_algorithm: 0,
            client_throttle_enabled: false,
            client_throttle_threshold: 0,
            client_throttle_scalar: 0.0,
        })
        .await;
        self.set_outbound_compression(compression).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{PacketRateLimiter, bedrock::split_raw_packet};
    use pumpkin_protocol::{
        bedrock::{SubClient, packet_encoder::BedrockBatchEncoder},
        packet::Packet,
    };
    use std::{io::Cursor, pin::pin};
    use tokio::sync::Mutex;

    #[test]
    fn accepts_current_and_registered_bedrock_protocols() {
        let versions = BedrockVersions::new();
        assert!(matches!(
            incompatible_protocol_status(2224, &versions),
            Some(CPlayStatus::OutdatedClient)
        ));
        assert!(incompatible_protocol_status(2225, &versions).is_none());
        assert!(matches!(
            incompatible_protocol_status(2226, &versions),
            Some(CPlayStatus::OutdatedServer)
        ));

        versions.register("multiversion", [671, 2193]);
        assert!(incompatible_protocol_status(671, &versions).is_none());
        assert!(incompatible_protocol_status(2193, &versions).is_none());
        assert!(matches!(
            incompatible_protocol_status(-1, &versions),
            Some(CPlayStatus::OutdatedClient)
        ));
    }

    #[tokio::test]
    async fn accepts_login_before_network_settings_send_completes() {
        let address = "127.0.0.1:0".parse().unwrap();
        let session = crate::net::bedrock::NetherNetSession::test_session(address).await;
        let client = BedrockClient::new(
            0,
            session,
            address,
            Arc::new(Mutex::default()),
            PacketRateLimiter::new(false, 0.0, 0.0),
            std::sync::Weak::new(),
        );
        let mut outgoing = client
            .outgoing_packet_queue_recv
            .lock()
            .await
            .take()
            .unwrap();
        // Force the small, valid login fixture across the Deflate threshold.
        let compression = CompressionInfo {
            threshold: 1,
            ..CompressionInfo::default()
        };
        let deflate = (compression.threshold as usize, compression.level);
        let mut settings = pin!(client.send_network_settings(compression));
        assert!(futures::poll!(settings.as_mut()).is_pending());

        let response = outgoing.try_recv().expect("network settings queued");
        let (id, payload) = split_raw_packet(&response.data).unwrap();
        assert_eq!(i32::from(id), CNetworkSettings::PACKET_ID);
        let mut frame = Vec::new();
        let mut encoder = BedrockBatchEncoder::new();
        client
            .network_writer
            .read()
            .await
            .write_game_packet(id, SubClient::Main, SubClient::Main, &payload, &mut frame)
            .unwrap();
        let mut expected = Vec::new();
        encoder
            .write_game_packet(
                id,
                SubClient::Main,
                SubClient::Main,
                &payload,
                &mut expected,
            )
            .unwrap();
        assert_eq!(frame, expected, "network settings use the initial framing");

        // The peer has received the settings while the sender's completion is still pending.
        let mut login = CURRENT_BEDROCK_MC_PROTOCOL.to_be_bytes().to_vec();
        login.push(8); // Connection request containing two empty token fields.
        login.extend([0; 8]);
        frame.clear();
        encoder.set_compression(deflate);
        encoder
            .write_game_packet(
                SLogin::PACKET_ID as u16,
                SubClient::Main,
                SubClient::Main,
                &login,
                &mut frame,
            )
            .unwrap();
        let batch = client.get_packet_payload(frame).await.unwrap();
        let packet = client
            .network_reader
            .lock()
            .await
            .get_game_packet(&mut Cursor::new(batch))
            .expect("decode the first compressed login");
        assert_eq!(packet.id, SLogin::PACKET_ID);
        assert_eq!(packet.payload.as_ref(), login);

        response.completion.unwrap().send(()).unwrap();
        settings.await;
        let mut next_frame = Vec::new();
        client
            .network_writer
            .read()
            .await
            .write_packet(&CPlayStatus::LoginSuccess, &mut next_frame)
            .unwrap();
        assert_eq!(next_frame[1], 0, "subsequent packets use Deflate framing");
        client.close().await;
    }
}
