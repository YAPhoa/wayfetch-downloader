//! Event subscriptions with bounded replay and a bounded live window
//! (architecture brief section 7.8, ADR-28; tests T47 and T66).
//!
//! A subscription replays at most [`crate::config::Limits::event_buffer`]
//! retained events, then follows the coordinator's live broadcast, whose
//! ring holds the same number. Memory therefore stays bounded whatever the
//! number of events or subscribers: a client further behind is told to
//! resynchronize ([`ResyncReason::Lagged`]) instead of being buffered for.
//! Subscriptions never affect job execution, and dropping one is enough to
//! end it.

use std::collections::VecDeque;

use tokio::sync::broadcast;
use wayfetch_protocol::event::{DurableEvent, ResyncReason};
use wayfetch_protocol::{Hello, StreamItem};

/// A live event stream. Yields [`StreamItem::Hello`] first; a
/// [`StreamItem::Resync`] is always last.
#[derive(Debug)]
pub struct Subscription {
    pending: VecDeque<StreamItem>,
    live: Option<broadcast::Receiver<DurableEvent>>,
    /// Position of the last event yielded or replayed.
    last: u64,
}

impl Subscription {
    /// A stream that only tells the client to resynchronize.
    pub(crate) fn resync(hello: Hello, reason: ResyncReason) -> Self {
        Self {
            pending: VecDeque::from([StreamItem::Hello(hello), StreamItem::Resync { reason }]),
            live: None,
            last: 0,
        }
    }

    /// A stream that replays `events`, which follow position `after`, then
    /// follows `live`. `live` must have subscribed before any event newer
    /// than the replayed ones was broadcast.
    pub(crate) fn replay(
        hello: Hello,
        after: u64,
        events: Vec<DurableEvent>,
        live: broadcast::Receiver<DurableEvent>,
    ) -> Self {
        let last = events.last().map_or(after, |e| e.cursor.position);
        let mut pending = VecDeque::with_capacity(events.len() + 1);
        pending.push_back(StreamItem::Hello(hello));
        pending.extend(events.into_iter().map(StreamItem::Event));
        Self {
            pending,
            live: Some(live),
            last,
        }
    }

    /// The next item, or `None` once the stream ended: after a resync, or
    /// when the engine stopped.
    pub async fn next(&mut self) -> Option<StreamItem> {
        if let Some(item) = self.pending.pop_front() {
            return Some(item);
        }
        let live = self.live.as_mut()?;
        loop {
            match live.recv().await {
                // Already replayed.
                Ok(event) if event.cursor.position <= self.last => {}
                Ok(event) => {
                    self.last = event.cursor.position;
                    return Some(StreamItem::Event(event));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.live = None;
                    return Some(StreamItem::Resync {
                        reason: ResyncReason::Lagged,
                    });
                }
                Err(broadcast::error::RecvError::Closed) => {
                    self.live = None;
                    return None;
                }
            }
        }
    }
}
