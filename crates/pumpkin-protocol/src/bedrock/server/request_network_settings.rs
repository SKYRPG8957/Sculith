// Last verified for v2169

use pumpkin_macros::packet;

use crate::serial::PacketRead;

/// Sent by the client to query network settings (such as compression algorithms and thresholds) from the server.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::REQUEST_NETWORK_SETTINGS)]
pub struct SRequestNetworkSettings {
    /// Network protocol version supported by the connecting client.
    /// If unsupported by the server, the connection will be rejected.
    #[serial(big_endian)]
    pub client_network_version: i32,
}
