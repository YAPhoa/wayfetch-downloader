//! Event subscriptions delivered to renderers, with flow control Tauri
//! channels lack.
//!
//! Each subscription keeps at most [`WINDOW`] unacknowledged items in flight
//! and ends after [`ACK_TIMEOUT`] without acknowledgement. A renderer holds
//! at most [`MAX_PER_RENDERER`], and a page load closes them all.
//! Subscriptions only watch; they never affect jobs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use wayfetch_engine::Subscription;
use wayfetch_engine::protocol::{ErrorCode, ProtocolError, StreamItem};

/// Subscriptions one renderer may hold at once.
pub const MAX_PER_RENDERER: usize = 4;
/// Items sent to a renderer that it has not acknowledged yet, at most.
pub const WINDOW: u64 = 64;
/// How long a subscription waits for an acknowledgement before it ends.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// Names a subscription to its renderer.
pub type SubscriptionId = u64;

/// Why a subscription ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    /// The renderer unsubscribed, reloaded, or closed.
    Closed,
    /// The engine told the renderer to resynchronize.
    Resync,
    /// The engine stopped.
    EngineStopped,
    /// The renderer stopped acknowledging items.
    Stalled,
    /// Items could not be delivered: the renderer is gone.
    Undeliverable,
}

/// Called with the number of open subscriptions after each change, and
/// why the change happened: `None` when one opened.
pub type OnChange = Arc<dyn Fn(usize, Option<Ended>) + Send + Sync>;

struct Entry {
    renderer: String,
    /// The count of items the renderer acknowledged.
    acknowledged: watch::Sender<u64>,
    /// Ends the pump when sent or dropped; taken once it runs.
    stop: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct Registry {
    next: SubscriptionId,
    entries: HashMap<SubscriptionId, Entry>,
}

/// Every renderer's subscriptions.
#[derive(Clone)]
pub struct Subscriptions {
    registry: Arc<Mutex<Registry>>,
    on_change: OnChange,
}

impl Subscriptions {
    /// No subscriptions yet.
    pub fn new(on_change: OnChange) -> Self {
        Self {
            registry: Arc::default(),
            on_change,
        }
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        // A panic while holding the lock leaves no partial update behind.
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserves a subscription for `renderer`, or refuses with
    /// [`ErrorCode::Overloaded`] at its limit.
    pub fn reserve(&self, renderer: &str) -> Result<Slot, ProtocolError> {
        let mut registry = self.registry();
        let held = registry
            .entries
            .values()
            .filter(|e| e.renderer == renderer)
            .count();
        if held >= MAX_PER_RENDERER {
            return Err(ProtocolError::new(
                ErrorCode::Overloaded,
                "too many open event subscriptions",
            ));
        }
        registry.next += 1;
        let id = registry.next;
        let (acknowledged, _) = watch::channel(0);
        registry.entries.insert(
            id,
            Entry {
                renderer: renderer.to_owned(),
                acknowledged,
                stop: None,
            },
        );
        let open = registry.entries.len();
        drop(registry);
        (self.on_change)(open, None);
        Ok(Slot {
            subscriptions: self.clone(),
            id,
            started: false,
        })
    }

    /// Records `renderer`'s acknowledgement of the first `received` items.
    /// Another renderer's subscription is ignored.
    pub fn acknowledge(&self, renderer: &str, id: SubscriptionId, received: u64) {
        if let Some(entry) = self.registry().entries.get(&id)
            && entry.renderer == renderer
        {
            entry.acknowledged.send_if_modified(|count| {
                let newer = received > *count;
                *count = (*count).max(received);
                newer
            });
        }
    }

    /// Ends one of `renderer`'s subscriptions.
    pub fn close(&self, renderer: &str, id: SubscriptionId) {
        let owned = self
            .registry()
            .entries
            .get(&id)
            .is_some_and(|e| e.renderer == renderer);
        if owned {
            self.remove(id, Ended::Closed);
        }
    }

    /// Ends every subscription of `renderer`, for example because its page
    /// is reloading.
    pub fn close_renderer(&self, renderer: &str) {
        self.close_where(|e| e.renderer == renderer);
    }

    /// Ends every subscription, once the window is gone.
    pub fn close_all(&self) {
        self.close_where(|_| true);
    }

    fn close_where(&self, matches: impl Fn(&Entry) -> bool) {
        let ids: Vec<_> = self
            .registry()
            .entries
            .iter()
            .filter(|(_, e)| matches(e))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.remove(id, Ended::Closed);
        }
    }

    /// The number of open subscriptions.
    #[cfg(test)]
    fn open(&self) -> usize {
        self.registry().entries.len()
    }

    /// Removes a subscription, which stops its pump. Reports the change
    /// only if it was still open.
    fn remove(&self, id: SubscriptionId, why: Ended) {
        let mut registry = self.registry();
        if registry.entries.remove(&id).is_none() {
            return;
        }
        let open = registry.entries.len();
        drop(registry);
        (self.on_change)(open, Some(why));
    }
}

/// A reserved subscription, released if dropped before it starts.
pub struct Slot {
    subscriptions: Subscriptions,
    id: SubscriptionId,
    started: bool,
}

impl Slot {
    /// Starts delivering `subscription`'s items through `send`, which
    /// returns `false` once the renderer cannot receive them. `None` tells
    /// the renderer the stream ended. Returns the subscription's ID.
    pub fn start(
        mut self,
        subscription: Subscription,
        send: impl FnMut(Option<StreamItem>) -> bool + Send + 'static,
    ) -> SubscriptionId {
        let (stop_tx, stop) = oneshot::channel();
        let acknowledged = {
            let mut registry = self.subscriptions.registry();
            let Some(entry) = registry.entries.get_mut(&self.id) else {
                // Closed while the engine was subscribing.
                return self.id;
            };
            entry.stop = Some(stop_tx);
            entry.acknowledged.subscribe()
        };
        self.started = true;
        let (subscriptions, id) = (self.subscriptions.clone(), self.id);
        tokio::spawn(async move {
            let ended = pump(subscription, send, acknowledged, stop).await;
            subscriptions.remove(id, ended);
        });
        self.id
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.started {
            self.subscriptions.remove(self.id, Ended::Closed);
        }
    }
}

/// Moves items from the engine to the renderer, never more than [`WINDOW`]
/// ahead of its acknowledgements.
async fn pump(
    mut subscription: Subscription,
    mut send: impl FnMut(Option<StreamItem>) -> bool,
    mut acknowledged: watch::Receiver<u64>,
    mut stop: oneshot::Receiver<()>,
) -> Ended {
    let mut sent: u64 = 0;
    // Only counts the renderer could have received; a larger
    // acknowledgement must not buy credit for items not yet sent.
    let mut confirmed: u64 = 0;
    loop {
        while sent - confirmed >= WINDOW {
            tokio::select! {
                changed = tokio::time::timeout(ACK_TIMEOUT, acknowledged.changed()) => {
                    match changed {
                        Ok(Ok(())) => {
                            confirmed = (*acknowledged.borrow_and_update()).clamp(confirmed, sent);
                        }
                        // Removed from the registry.
                        Ok(Err(_)) => return Ended::Closed,
                        Err(_) => {
                            send(None);
                            return Ended::Stalled;
                        }
                    }
                }
                _ = &mut stop => return Ended::Closed,
            }
        }
        let item = tokio::select! {
            item = subscription.next() => item,
            _ = &mut stop => return Ended::Closed,
        };
        let Some(item) = item else {
            send(None);
            return Ended::EngineStopped;
        };
        let resync = matches!(item, StreamItem::Resync { .. });
        if !send(Some(item)) {
            return Ended::Undeliverable;
        }
        sent += 1;
        if acknowledged.has_changed().unwrap_or(false) {
            confirmed = (*acknowledged.borrow_and_update()).clamp(confirmed, sent);
        }
        if resync {
            return Ended::Resync;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn subscriptions() -> (Subscriptions, mpsc::Receiver<(usize, Option<Ended>)>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let subscriptions = Subscriptions::new(Arc::new(move |open, why| {
            let _ = tx.lock().unwrap().send((open, why));
        }));
        (subscriptions, rx)
    }

    #[test]
    fn each_renderer_has_a_bounded_number() {
        let (subscriptions, _changes) = subscriptions();
        let slots: Vec<_> = (0..MAX_PER_RENDERER)
            .map(|_| subscriptions.reserve("main").unwrap())
            .collect();
        let refused = subscriptions.reserve("main").err().unwrap();
        assert_eq!(refused.code, ErrorCode::Overloaded);
        // Another renderer has its own allowance.
        let other = subscriptions.reserve("other").unwrap();
        // Unstarted slots release themselves.
        drop(slots);
        assert_eq!(subscriptions.open(), 1);
        drop(other);
        assert_eq!(subscriptions.open(), 0);
    }

    #[test]
    fn a_renderer_closes_only_its_own() {
        let (subscriptions, changes) = subscriptions();
        let mine = subscriptions.reserve("main").unwrap();
        let theirs = subscriptions.reserve("other").unwrap();
        subscriptions.close("other", mine.id);
        assert_eq!(subscriptions.open(), 2);
        subscriptions.close_renderer("main");
        assert_eq!(subscriptions.open(), 1);
        let last: Vec<_> = changes.try_iter().collect();
        assert_eq!(last.last(), Some(&(1, Some(Ended::Closed))));
        drop((mine, theirs));
        assert_eq!(subscriptions.open(), 0);
    }
}
