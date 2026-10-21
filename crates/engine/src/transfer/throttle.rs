//! The total download speed limit, shared by every download body an engine
//! writes.
//!
//! [`Bandwidth`] is an exact token bucket. Writers take FIFO turns and are
//! granted at most one quantum per turn, so no transfer starves and bursts
//! stay bounded. Changing the limit never adds credit, and dropping a wait
//! takes nothing from the bucket. Waiting for a grant does not count as
//! remote inactivity.

use std::num::NonZeroU64;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;
use wayfetch_protocol::query::SpeedLimit;

/// Nanoseconds per second: the bucket counts bytes times nanoseconds.
const NANOS: i128 = 1_000_000_000;

/// Smallest grant, so a very low limit still writes in useful pieces.
pub const MIN_QUANTUM: u64 = 256;
/// Largest grant.
pub const MAX_QUANTUM: u64 = 64 * 1024;
/// Largest burst.
pub const MAX_BURST: u64 = 16 * 1024 * 1024;

/// Most bytes one grant at `rate` bytes per second may carry: an eighth of
/// a second's worth, within [`MIN_QUANTUM`] and [`MAX_QUANTUM`].
pub const fn quantum(rate: u64) -> u64 {
    clamp(rate / 8, MIN_QUANTUM, MAX_QUANTUM)
}

/// Most credit the bucket keeps at `rate` bytes per second: a quarter of a
/// second's worth, at least one quantum and at most [`MAX_BURST`].
pub const fn burst(rate: u64) -> u64 {
    clamp(rate / 4, quantum(rate), MAX_BURST)
}

const fn clamp(value: u64, min: u64, max: u64) -> u64 {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

#[derive(Debug)]
struct Bucket {
    /// Bytes per second; `None` is Unlimited.
    rate: Option<NonZeroU64>,
    /// Credit in byte-nanoseconds; negative is debt.
    balance: i128,
    /// When `balance` was last brought up to date.
    at: Instant,
}

impl Bucket {
    /// Credits the time since the last update at the current rate.
    fn refill(&mut self, now: Instant) {
        if let Some(rate) = self.rate {
            let elapsed = i128::try_from(now.saturating_duration_since(self.at).as_nanos())
                .unwrap_or(i128::MAX);
            let credit = elapsed.saturating_mul(i128::from(rate.get()));
            self.balance = self.balance.saturating_add(credit).min(capacity(rate));
        }
        self.at = now;
    }
}

/// The burst at `rate`, in byte-nanoseconds.
fn capacity(rate: NonZeroU64) -> i128 {
    i128::from(burst(rate.get())) * NANOS
}

fn rate_of(limit: SpeedLimit) -> Option<NonZeroU64> {
    match limit {
        SpeedLimit::Unlimited => None,
        SpeedLimit::BytesPerSecond(rate) => NonZeroU64::new(rate),
    }
}

/// The engine's shared download budget. See the module documentation.
#[derive(Debug)]
pub struct Bandwidth {
    bucket: Mutex<Bucket>,
    /// Wakes the writer waiting at the head when the limit changes.
    changed: Notify,
    /// Whose turn it is, in arrival order.
    turn: tokio::sync::Mutex<()>,
}

impl Bandwidth {
    /// A budget for `limit`, starting without credit. A limit of zero
    /// bytes per second, which policy never permits, is taken as Unlimited
    /// rather than as stopping every download.
    pub fn new(limit: SpeedLimit) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                rate: rate_of(limit),
                balance: 0,
                at: Instant::now(),
            }),
            changed: Notify::new(),
            turn: tokio::sync::Mutex::new(()),
        }
    }

    fn bucket(&self) -> MutexGuard<'_, Bucket> {
        // The bucket is consistent after every statement, so a panic
        // elsewhere leaves nothing half-done.
        self.bucket.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The limit in effect.
    pub fn limit(&self) -> SpeedLimit {
        self.bucket().rate.map_or(SpeedLimit::Unlimited, |rate| {
            SpeedLimit::BytesPerSecond(rate.get())
        })
    }

    /// Applies `limit` from now on, to transfers already running as well
    /// as later ones, without granting fresh credit.
    pub fn set(&self, limit: SpeedLimit) {
        let rate = rate_of(limit);
        {
            let mut bucket = self.bucket();
            bucket.refill(Instant::now());
            match (bucket.rate, rate) {
                (None, Some(_)) => bucket.balance = 0,
                (Some(_), Some(new)) => bucket.balance = bucket.balance.min(capacity(new)),
                (_, None) => {}
            }
            bucket.rate = rate;
        }
        self.changed.notify_waiters();
    }

    /// Waits until some of a chunk of `wanted` bytes may be written, and
    /// returns how much: all of it when Unlimited, else at least one byte
    /// and at most one quantum. An empty chunk needs no grant.
    pub async fn admit(&self, wanted: usize) -> usize {
        if wanted == 0 || self.bucket().rate.is_none() {
            return wanted;
        }
        let _turn = self.turn.lock().await;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Registered before the bucket is read, so a change made after
            // the read still wakes this wait.
            changed.as_mut().enable();
            let until = {
                let mut bucket = self.bucket();
                let now = Instant::now();
                bucket.refill(now);
                let Some(rate) = bucket.rate else {
                    return wanted;
                };
                if bucket.balance >= 0 {
                    let granted = quantum(rate.get()).min(wanted as u64);
                    bucket.balance -= i128::from(granted) * NANOS;
                    // At most `wanted`, so it fits.
                    return granted as usize;
                }
                let rate = i128::from(rate.get());
                let nanos = (-bucket.balance + rate - 1) / rate;
                let wait = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
                now.checked_add(wait)
                    .unwrap_or_else(|| now + Duration::from_secs(86_400))
            };
            tokio::select! {
                () = tokio::time::sleep_until(until) => {}
                () = &mut changed => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;

    fn rate(bytes: u64) -> SpeedLimit {
        SpeedLimit::BytesPerSecond(bytes)
    }

    /// Admits chunks of `chunk` bytes, whole, as a writer does, counting
    /// the bytes admitted.
    fn reader(
        bandwidth: &Arc<Bandwidth>,
        chunk: usize,
    ) -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
        let admitted = Arc::new(AtomicU64::new(0));
        let task = {
            let (bandwidth, admitted) = (bandwidth.clone(), admitted.clone());
            tokio::spawn(async move {
                loop {
                    let mut left = chunk;
                    while left > 0 {
                        let granted = bandwidth.admit(left).await;
                        assert!(granted > 0 && granted <= left);
                        left -= granted;
                        admitted.fetch_add(granted as u64, Ordering::SeqCst);
                    }
                }
            })
        };
        (admitted, task)
    }

    fn total(counters: &[Arc<AtomicU64>]) -> u64 {
        counters.iter().map(|c| c.load(Ordering::SeqCst)).sum()
    }

    #[test]
    fn grants_and_bursts_are_bounded() {
        assert_eq!((quantum(KIB), burst(KIB)), (MIN_QUANTUM, MIN_QUANTUM));
        assert_eq!((quantum(2 * MIB), burst(2 * MIB)), (MAX_QUANTUM, 512 * KIB));
        assert_eq!(burst(1 << 40), MAX_BURST);
        assert_eq!(quantum(1), MIN_QUANTUM);
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_admits_whole_chunks_and_empty_ones_need_nothing() {
        let bandwidth = Bandwidth::new(SpeedLimit::Unlimited);
        let start = Instant::now();
        assert_eq!(bandwidth.admit(16 * MIB as usize).await, 16 * MIB as usize);
        assert_eq!(bandwidth.admit(0).await, 0);
        assert_eq!(start.elapsed(), Duration::ZERO);
        let limited = Bandwidth::new(rate(KIB));
        assert_eq!(limited.admit(0).await, 0);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn transfers_share_one_budget_within_the_documented_bound() {
        let limit = 2 * MIB;
        let bandwidth = Arc::new(Bandwidth::new(rate(limit)));
        let readers: Vec<_> = (0..3).map(|_| reader(&bandwidth, 256 * 1024)).collect();
        let counters: Vec<_> = readers.iter().map(|(c, _)| c.clone()).collect();
        let seconds = 10;
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let admitted = total(&counters);
        let allowed = limit * seconds;
        // Starting without credit, the excess is at most one quantum; three
        // transfers do not get three budgets.
        assert!(admitted <= allowed + quantum(limit), "{admitted}");
        assert!(admitted >= allowed - quantum(limit), "{admitted}");
        // Turns are taken in order, so the shares are close.
        for counter in &counters {
            let share = counter.load(Ordering::SeqCst);
            assert!(
                share.abs_diff(admitted / 3) <= 2 * quantum(limit),
                "{share} of {admitted}"
            );
        }
        for (_, task) in readers {
            task.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_budget_bursts_at_most_its_burst() {
        let limit = 4 * MIB;
        let bandwidth = Arc::new(Bandwidth::new(rate(limit)));
        tokio::time::sleep(Duration::from_secs(60)).await;
        let (counter, task) = reader(&bandwidth, 256 * 1024);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let admitted = counter.load(Ordering::SeqCst);
        assert!(
            admitted <= burst(limit) + quantum(limit) + 2 * limit,
            "{admitted}"
        );
        assert!(
            admitted >= burst(limit) + 2 * limit - quantum(limit),
            "{admitted}"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn small_and_large_chunks_both_progress() {
        let limit = MIB;
        let bandwidth = Arc::new(Bandwidth::new(rate(limit)));
        let (small, a) = reader(&bandwidth, 512);
        let (large, b) = reader(&bandwidth, 1024 * 1024);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let (small, large) = (small.load(Ordering::SeqCst), large.load(Ordering::SeqCst));
        assert!(small + large <= 10 * limit + quantum(limit));
        // Each turn the small one takes 512 bytes and the large one a
        // quantum: the large one is not starved by many small turns, nor
        // the small one by its neighbour's oversized chunk.
        let turns = (small + large) / (512 + quantum(limit));
        assert!(small >= 512 * (turns - 2), "{small} {large}");
        assert!(large >= quantum(limit) * (turns - 2), "{small} {large}");
        a.abort();
        b.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_very_low_limit_writes_an_oversized_chunk_in_pieces() {
        let bandwidth = Bandwidth::new(rate(KIB));
        let start = Instant::now();
        let mut left = 4 * KIB as usize;
        let mut grants = 0;
        while left > 0 {
            let granted = bandwidth.admit(left).await;
            assert!(granted as u64 <= MIN_QUANTUM);
            left -= granted;
            grants += 1;
        }
        assert_eq!(grants, 16);
        // The first grant is free; the other fifteen are paid at 1 KiB/s.
        assert_eq!(start.elapsed(), Duration::from_millis(3750));
    }

    #[tokio::test(start_paused = true)]
    async fn changes_reach_waiting_transfers_at_once() {
        let bandwidth = Arc::new(Bandwidth::new(rate(KIB)));
        // Go into debt, so the next grant waits a quarter of a second.
        bandwidth.admit(MIN_QUANTUM as usize).await;
        let waiting = {
            let bandwidth = bandwidth.clone();
            tokio::spawn(async move {
                let start = Instant::now();
                bandwidth.admit(MAX_QUANTUM as usize).await;
                start.elapsed()
            })
        };
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Removing the limit releases the waiter without its full wait.
        bandwidth.set(SpeedLimit::Unlimited);
        assert_eq!(waiting.await.unwrap(), Duration::from_millis(10));
        assert_eq!(bandwidth.limit(), SpeedLimit::Unlimited);

        // Raising a limit shortens a wait already under way: debt of one
        // quantum at 1 KiB/s takes 250 ms, at 1 MiB/s far less.
        bandwidth.set(rate(KIB));
        bandwidth.admit(MIN_QUANTUM as usize).await;
        let waiting = {
            let bandwidth = bandwidth.clone();
            tokio::spawn(async move {
                let start = Instant::now();
                bandwidth.admit(1).await;
                start.elapsed()
            })
        };
        tokio::task::yield_now().await;
        bandwidth.set(rate(MIB));
        // The timer's resolution is a millisecond.
        assert!(waiting.await.unwrap() <= Duration::from_millis(1));

        // Lowering one lengthens it: the debt of a 64 KiB grant, less the
        // little credit left over, is repaid at the new rate, which takes
        // about a minute instead of the 62 ms it would at 1 MiB/s.
        bandwidth.admit(MAX_QUANTUM as usize).await;
        bandwidth.set(rate(KIB));
        let start = Instant::now();
        bandwidth.admit(1).await;
        let waited = start.elapsed();
        assert!(
            (Duration::from_secs(60)..=Duration::from_secs(64)).contains(&waited),
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_changes_grant_no_extra_credit() {
        let limit = 256 * KIB;
        let bandwidth = Arc::new(Bandwidth::new(rate(limit)));
        let (counter, task) = reader(&bandwidth, 256 * 1024);
        for _ in 0..1000 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            bandwidth.set(rate(limit));
        }
        let admitted = counter.load(Ordering::SeqCst);
        assert!(admitted <= 5 * limit + quantum(limit), "{admitted}");
        assert!(admitted >= 5 * limit - quantum(limit), "{admitted}");
        // Leaving Unlimited starts empty too.
        bandwidth.set(SpeedLimit::Unlimited);
        bandwidth.set(rate(limit));
        let before = counter.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let after = counter.load(Ordering::SeqCst) - before;
        assert!(after <= limit + 2 * quantum(limit), "{after}");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_wait_takes_nothing_and_blocks_no_one() {
        let bandwidth = Arc::new(Bandwidth::new(rate(KIB)));
        bandwidth.admit(MIN_QUANTUM as usize).await;
        // A wait at the head of the line is abandoned, as a cancellation
        // abandons it.
        let gave_up = tokio::time::timeout(Duration::from_millis(100), bandwidth.admit(100)).await;
        assert!(gave_up.is_err());
        // The next writer gets the turn, and the debt it waits out is only
        // the first grant's: 250 ms in all, not more.
        let start = Instant::now();
        assert_eq!(bandwidth.admit(100).await, 100);
        assert_eq!(start.elapsed(), Duration::from_millis(150));
    }
}
