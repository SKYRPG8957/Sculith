use std::{
    collections::VecDeque,
    hash::Hasher,
    io::Error,
    sync::{LazyLock, Mutex},
};

use bytes::Bytes;
use rustc_hash::{FxHashMap, FxHasher};
use sculith_protocol::bedrock::client::client_cache_miss_response::{
    CClientCacheMissResponse, MissingBlobData,
};

/// Blob bytes kept per client before the oldest are evicted.
const MAX_BLOB_CACHE_BYTES: usize = 16 * 1024 * 1024;

const SHARED_REPLY_SHARDS: usize = 64;
const SHARED_REPLY_BYTES_PER_SHARD: usize = 128 * 1024;
const SHARED_REPLIES_PER_SHARD: usize = 4;
const MIN_SHARED_REPLY_PAYLOAD: usize = 8 * 1024;

struct SharedReply {
    fingerprint: u64,
    blobs: Vec<MissingBlobData>,
    frame: Bytes,
    retained_bytes: usize,
}

static SHARED_REPLIES: LazyLock<[Mutex<VecDeque<SharedReply>>; SHARED_REPLY_SHARDS]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::default()));

/// Reuses raw responses before per-viewer packet events and compression.
pub(super) fn share_blob_reply(packet: CClientCacheMissResponse) -> Result<Bytes, Error> {
    let encode = || {
        let mut frame = Vec::new();
        super::BedrockClient::write_raw_packet(&packet, &mut frame)?;
        Ok(Bytes::from(frame.into_boxed_slice()))
    };
    let payload_bytes = packet
        .missing_blobs
        .iter()
        .map(|blob| blob.blob_data.len())
        .sum::<usize>();
    if !(MIN_SHARED_REPLY_PAYLOAD..=SHARED_REPLY_BYTES_PER_SHARD / 2).contains(&payload_bytes) {
        return encode();
    }
    let mut hasher = FxHasher::default();
    for blob in &packet.missing_blobs {
        hasher.write_u64(blob.blob_id);
    }
    let fingerprint = hasher.finish();
    let shard = &SHARED_REPLIES[fingerprint as usize % SHARED_REPLY_SHARDS];
    let find = |replies: &VecDeque<SharedReply>| {
        replies
            .iter()
            .find(|reply| {
                reply.fingerprint == fingerprint
                    && reply.blobs.len() == packet.missing_blobs.len()
                    && reply
                        .blobs
                        .iter()
                        .zip(&packet.missing_blobs)
                        .all(|(prior, current)| {
                            prior.blob_id == current.blob_id
                                && prior.blob_data.len() == current.blob_data.len()
                                && (prior.blob_data.as_ptr() == current.blob_data.as_ptr()
                                    || prior.blob_data == current.blob_data)
                        })
            })
            .map(|reply| reply.frame.clone())
    };
    {
        let replies = shard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(frame) = find(&replies) {
            return Ok(frame);
        }
    }

    let frame = encode()?;
    let retained_bytes = payload_bytes
        + frame.len()
        + packet.missing_blobs.capacity() * size_of::<MissingBlobData>();
    if retained_bytes > SHARED_REPLY_BYTES_PER_SHARD {
        return Ok(frame);
    }
    let mut replies = shard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(frame) = find(&replies) {
        return Ok(frame);
    }
    let mut size = retained_bytes
        + replies
            .iter()
            .map(|reply| reply.retained_bytes)
            .sum::<usize>();
    while size > SHARED_REPLY_BYTES_PER_SHARD || replies.len() >= SHARED_REPLIES_PER_SHARD {
        let Some(previous) = replies.pop_front() else {
            break;
        };
        size -= previous.retained_bytes;
    }
    replies.push_back(SharedReply {
        fingerprint,
        blobs: packet.missing_blobs,
        frame: frame.clone(),
        retained_bytes,
    });
    Ok(frame)
}

struct Entry {
    blob: Bytes,
    /// Identifies this insertion in the queues; older queue entries of the hash are stale.
    seq: u64,
    /// The client has reported on the blob (`Missing Ids` served or `Found Ids`).
    answered: bool,
}

/// Chunk blobs a client may still request by hash. Over the cap, answered blobs go first
/// since the client has them; blobs it has not answered yet go only once nothing else is left.
#[derive(Default)]
pub struct BlobCache {
    blobs: FxHashMap<u64, Entry>,
    /// `(hash, seq)` of blobs the client has not answered yet, oldest first.
    unanswered: VecDeque<(u64, u64)>,
    /// `(hash, seq)` of answered blobs, oldest first.
    answered: VecDeque<(u64, u64)>,
    next_seq: u64,
    bytes: usize,
}

impl BlobCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, hash: u64, blob: Bytes) {
        let seq = self.take_seq();
        // Equal hash means equal content. A new chunk may get it requested again, so an
        // answered copy goes back to waiting for an answer
        if let Some(entry) = self.blobs.get_mut(&hash) {
            if entry.answered {
                entry.answered = false;
                entry.seq = seq;
                self.unanswered.push_back((hash, seq));
                self.compact();
            }
            return;
        }
        self.bytes += blob.len();
        self.blobs.insert(
            hash,
            Entry {
                blob,
                seq,
                answered: false,
            },
        );
        self.unanswered.push_back((hash, seq));
        self.compact();
        self.evict();
    }

    #[must_use]
    pub fn get(&self, hash: u64) -> Option<&Bytes> {
        self.blobs.get(&hash).map(|entry| &entry.blob)
    }

    /// The client reported the blob as held (`Found Ids`), so it is not requested again.
    pub fn remove(&mut self, hash: u64) {
        if let Some(entry) = self.blobs.remove(&hash) {
            self.bytes -= entry.blob.len();
        }
    }

    /// The blob was served on a miss, so it is the first to go when the cache is full.
    pub fn mark_answered(&mut self, hash: u64) {
        let seq = self.take_seq();
        let Some(entry) = self.blobs.get_mut(&hash) else {
            return;
        };
        if entry.answered {
            return;
        }
        entry.answered = true;
        entry.seq = seq;
        self.answered.push_back((hash, seq));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }

    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    const fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Removed and re-queued hashes leave stale queue entries; drop them once they
    /// outnumber the live blobs, which leaves at most one entry per blob.
    fn compact(&mut self) {
        let limit = 2 * self.blobs.len() + 64;
        let blobs = &self.blobs;
        for queue in [&mut self.unanswered, &mut self.answered] {
            if queue.len() > limit {
                queue.retain(|&(hash, seq)| blobs.get(&hash).is_some_and(|e| e.seq == seq));
            }
        }
    }

    fn evict(&mut self) {
        while self.bytes > MAX_BLOB_CACHE_BYTES {
            let Some((hash, seq)) = self
                .answered
                .pop_front()
                .or_else(|| self.unanswered.pop_front())
            else {
                break;
            };
            if self.blobs.get(&hash).is_some_and(|e| e.seq == seq) {
                self.remove(hash);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(size: usize) -> Bytes {
        Bytes::from(vec![0; size])
    }

    #[test]
    fn blob_response_reuse_checks_the_actual_payload() {
        let frame = |byte| {
            let packet = CClientCacheMissResponse {
                missing_blobs: vec![MissingBlobData {
                    blob_id: 12_345,
                    blob_data: Bytes::from(vec![byte; MIN_SHARED_REPLY_PAYLOAD]),
                }],
            };
            share_blob_reply(packet).unwrap()
        };
        let first = frame(7);
        assert_eq!(first.as_ptr(), frame(7).as_ptr());
        assert_ne!(first, frame(8));
        assert_eq!(first.as_ptr(), frame(7).as_ptr());
    }

    #[test]
    fn reinserted_blob_is_not_evicted_through_its_stale_queue_entry() {
        let mut cache = BlobCache::new();
        cache.insert(1, blob(1));
        cache.remove(1);
        cache.insert(2, blob(MAX_BLOB_CACHE_BYTES - 1));
        cache.insert(1, blob(1));

        // Over the cap: 2 is older than the new copy of 1 and goes first
        cache.insert(3, blob(1));

        assert!(cache.get(2).is_none());
        assert!(cache.get(1).is_some());
        assert!(cache.get(3).is_some());
    }

    #[test]
    fn answered_blobs_are_evicted_before_unanswered_ones() {
        let mut cache = BlobCache::new();
        cache.insert(1, blob(MAX_BLOB_CACHE_BYTES / 2));
        cache.insert(2, blob(MAX_BLOB_CACHE_BYTES / 2));
        cache.mark_answered(2);

        cache.insert(3, blob(1));

        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
        assert_eq!(cache.bytes(), MAX_BLOB_CACHE_BYTES / 2 + 1);
    }

    #[test]
    fn answered_blob_sent_again_is_not_evicted_first() {
        let mut cache = BlobCache::new();
        cache.insert(1, blob(MAX_BLOB_CACHE_BYTES / 2));
        cache.insert(2, blob(MAX_BLOB_CACHE_BYTES / 2));
        cache.mark_answered(2);
        cache.mark_answered(1);
        // A new chunk references 2 again before the client answered for it
        cache.insert(2, blob(MAX_BLOB_CACHE_BYTES / 2));

        cache.insert(3, blob(1));

        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_some());
    }

    #[test]
    fn queues_stay_bounded_when_a_common_hash_cycles() {
        let mut cache = BlobCache::new();
        cache.insert(0, blob(1));
        for _ in 0..1000 {
            cache.insert(1, blob(1));
            cache.remove(1);
        }

        assert!(cache.unanswered.len() <= 2 * cache.len() + 64 + 1);
    }
}
