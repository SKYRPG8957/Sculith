use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};

use crate::server::TickDeadline;

use super::BedrockClient;

impl BedrockClient {
    pub(crate) fn begin_chunk_view(&self) -> u64 {
        self.activity
            .chunk_view_revision
            .fetch_add(1, AcqRel)
            .wrapping_add(1)
    }

    pub(crate) fn publish_chunk_view(&self, revision: u64) {
        self.activity
            .published_chunk_view
            .fetch_max(revision, Release);
    }

    pub(crate) fn chunk_view_published(&self) -> bool {
        let published = self.activity.published_chunk_view.load(Acquire);
        published != 0 && published == self.activity.chunk_view_revision.load(Acquire)
    }

    /// The chunk sender lock serializes the world and regional callers.
    pub(crate) fn claim_chunk_step(&self, deadline: Option<TickDeadline>) -> bool {
        TickDeadline::claim(&self.activity.last_chunk_step, deadline)
    }
}
