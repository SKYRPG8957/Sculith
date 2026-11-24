// Last verified for v2169

use crate::serial::PacketRead;
use pumpkin_macros::packet;

/// Sent by the client during login to inform the server whether client-side chunk blob caching is supported.
#[derive(PacketRead)]
#[packet(pumpkin_data::bedrock_packet::CLIENT_CACHE_STATUS)]
pub struct SClientCacheStatus {
    /// True if the client supports caching sub-chunk blobs by hash.
    pub is_cache_supported: bool,
}
