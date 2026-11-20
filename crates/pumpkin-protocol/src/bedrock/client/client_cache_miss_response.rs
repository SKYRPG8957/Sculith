// Last verified for v2169

use bytes::Bytes;
use pumpkin_macros::packet;

use crate::serial::{PacketRead, PacketWrite};

/// Payload data for a requested chunk blob that was missing from the client cache.
#[derive(PacketRead, PacketWrite, Clone, Debug)]
pub struct MissingBlobData {
    /// 64-bit hash identifier of the blob.
    pub blob_id: u64,
    /// Raw serialized byte payload of the chunk blob.
    pub blob_data: Bytes,
}

/// Sent by the server in response to [`SClientCacheBlobStatus`](crate::bedrock::server::client_cache_blob_status::SClientCacheBlobStatus) with requested chunk blobs.
#[derive(PacketRead, PacketWrite)]
#[packet(pumpkin_data::bedrock_packet::CLIENT_CACHE_MISS_RESPONSE)]
pub struct CClientCacheMissResponse {
    /// Array of blob byte payloads corresponding to missed blob hashes.
    pub missing_blobs: Vec<MissingBlobData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_blob_ids_and_bytes() {
        let packet = CClientCacheMissResponse {
            missing_blobs: vec![MissingBlobData {
                blob_id: u64::MAX,
                blob_data: Bytes::from_static(b"terrain"),
            }],
        };
        let mut body = Vec::new();
        packet.write(&mut body).unwrap();
        let mut truncated = &body[..body.len() - 1];
        assert!(CClientCacheMissResponse::read(&mut truncated).is_err());

        let mut body = body.as_slice();
        let packet = CClientCacheMissResponse::read(&mut body).unwrap();
        assert!(body.is_empty());
        assert_eq!(packet.missing_blobs[0].blob_id, u64::MAX);
        assert_eq!(packet.missing_blobs[0].blob_data.as_ref(), b"terrain");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::PacketWrite;

    #[test]
    fn blobs_are_written_as_id_then_length_prefixed_data() {
        let packet = CClientCacheMissResponse {
            missing_blobs: vec![
                MissingBlobData {
                    blob_id: 0x0102_0304_0506_0708,
                    blob_data: Bytes::from_static(&[0xAA, 0xBB]),
                },
                MissingBlobData {
                    blob_id: 1,
                    blob_data: Bytes::new(),
                },
            ],
        };

        let mut bytes = Vec::new();
        packet.write(&mut bytes).unwrap();

        assert_eq!(
            bytes,
            [
                2, // blob count
                8, 7, 6, 5, 4, 3, 2, 1, // id, little endian
                2, 0xAA, 0xBB, // data
                1, 0, 0, 0, 0, 0, 0, 0, // id
                0, // empty data
            ]
        );
    }
}
