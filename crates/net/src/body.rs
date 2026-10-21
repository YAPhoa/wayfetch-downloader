//! A response body as the engine's [`ChunkSource`]: chunks never exceed
//! the engine's read buffer, and only one is resident at a time.

use std::io;

use bytes::Bytes;
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};

/// The body of an open response.
pub(crate) struct Body {
    response: reqwest::Response,
    /// The rest of the last chunk the client delivered, if it was larger
    /// than `max`.
    pending: Bytes,
    max: usize,
}

impl Body {
    pub(crate) fn new(response: reqwest::Response, max: usize) -> Self {
        Self {
            response,
            pending: Bytes::new(),
            max: max.max(1),
        }
    }
}

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            while self.pending.is_empty() {
                match self.response.chunk().await.map_err(io::Error::other)? {
                    Some(chunk) => self.pending = chunk,
                    None => return Ok(None),
                }
            }
            let take = self.pending.len().min(self.max);
            Ok(Some(self.pending.split_to(take)))
        })
    }
}
