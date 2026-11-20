// Last verified for v2169

use pumpkin_macros::packet;

use crate::serial::{PacketRead, PacketWrite};

/// Sent by the server to inform the client of login results, version compatibility, or player spawn readiness.
#[derive(Clone, Copy, PacketRead, PacketWrite)]
#[repr(i32)]
#[serial(big_endian)]
#[packet(pumpkin_data::bedrock_packet::PLAY_STATUS)]
pub enum CPlayStatus {
    /// Authentication and login succeeded; client proceeds to resource pack handshake.
    LoginSuccess = 0,
    /// Client protocol version is older than required by the server.
    OutdatedClient = 1,
    /// Server protocol version is older than the client version.
    OutdatedServer = 2,
    /// Server is ready for the player to spawn into the world.
    PlayerSpawn = 3,
    /// Client tenant ID is invalid.
    InvalidTenant = 4,
    /// Educational edition client attempting to join a vanilla server.
    EditionMismatchEduToVanilla = 5,
    /// Vanilla client attempting to join an educational server.
    EditionMismatchVanillaToEdu = 6,
    /// Server is full for local split-screen sub-clients.
    ServerFullSubClient = 7,
    /// Editor mode client attempting to join a vanilla world.
    EditorMismatchEditorToVanilla = 8,
    /// Vanilla client attempting to join an editor world.
    EditorMismatchVanillaToEditor = 9,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bedrock::packet_encoder::serialize_packet;

    #[test]
    fn writes_vanilla_version_rejection_codes() {
        let mut outdated_client = Vec::new();
        CPlayStatus::OutdatedClient
            .write(&mut outdated_client)
            .unwrap();
        assert_eq!(outdated_client, [0, 0, 0, 1]);
        assert!(matches!(
            CPlayStatus::read(&mut outdated_client.as_slice()).unwrap(),
            CPlayStatus::OutdatedClient
        ));

        let mut outdated_server = Vec::new();
        CPlayStatus::OutdatedServer
            .write(&mut outdated_server)
            .unwrap();
        assert_eq!(outdated_server, [0, 0, 0, 2]);

        assert_eq!(
            serialize_packet(&CPlayStatus::OutdatedServer)
                .unwrap()
                .as_ref(),
            [0xfe, 5, 2, 0, 0, 0, 2]
        );
    }
}
