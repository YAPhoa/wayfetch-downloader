//! Live transfer progress (architecture brief section 4.7 and ADR-28):
//! ephemeral, coalesced samples, never persisted.
//!
//! A download writing a body holds a [`Metering`]; its writer records each
//! byte position it reaches with one atomic store. While any download is
//! metered, one sampler task reads every meter once per [`PERIOD`] and
//! publishes the whole set as the latest value of a `watch` channel.
//! Subscribers therefore only ever see the newest set: a slow one skips
//! samples instead of being buffered for or told to resynchronize, and
//! memory stays bounded by the number of active downloads. Nothing is sent
//! while nothing transfers.
//!
//! Speed is the bytes received over the last [`WINDOW`], like curl's
//! one-second slots, which evens out the bursts of the shared bandwidth
//! budget and of checkpoint syncs while following a speed-limit change
//! within a few seconds. Time left is the client's to derive.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::JobId;
use wayfetch_domain::version::{BootEpoch, Generation};
use wayfetch_protocol::event::ProgressSample;

/// How often samples are published.
pub(crate) const PERIOD: Duration = Duration::from_secs(1);

/// How far back speed is measured.
pub(crate) const WINDOW: Duration = Duration::from_secs(5);

/// The latest samples, one per metered download.
pub type Samples = Arc<[ProgressSample]>;

/// The engine's live progress: meters and the latest samples.
#[derive(Clone)]
pub(crate) struct Progress(Arc<Shared>);

struct Shared {
    boot_epoch: BootEpoch,
    meters: Mutex<Vec<Arc<Meter>>>,
    /// Numbers meters, so a new meter never inherits an old one's window.
    serials: AtomicU64,
    /// Wakes the sampler when a meter is added.
    added: Notify,
    samples: watch::Sender<Samples>,
}

/// One download's position, written by its body writer.
#[derive(Debug)]
pub(crate) struct Meter {
    serial: u64,
    job: JobId,
    generation: Generation,
    total: Option<u64>,
    /// When the meter was added and the position then, so the first
    /// sample already has a speed.
    started: (Instant, u64),
    received: AtomicU64,
}

impl Meter {
    /// Records that bytes up to `position` were written.
    pub(crate) fn reached(&self, position: u64) {
        self.received.store(position, Ordering::Relaxed);
    }
}

/// A download's meter, removed when dropped.
pub(crate) struct Metering {
    shared: Arc<Shared>,
    meter: Arc<Meter>,
}

impl Metering {
    /// The meter its writers record into.
    pub(crate) fn meter(&self) -> &Arc<Meter> {
        &self.meter
    }
}

impl Drop for Metering {
    fn drop(&mut self) {
        self.shared
            .meters()
            .retain(|meter| !Arc::ptr_eq(meter, &self.meter));
    }
}

impl Progress {
    pub(crate) fn new(boot_epoch: BootEpoch) -> Self {
        Self(Arc::new(Shared {
            boot_epoch,
            meters: Mutex::default(),
            serials: AtomicU64::new(0),
            added: Notify::new(),
            samples: watch::Sender::new(Samples::default()),
        }))
    }

    /// Meters `job`'s transfer under `generation`, which has `from` bytes
    /// and is expected to reach `total`, if known.
    pub(crate) fn meter(
        &self,
        job: JobId,
        generation: Generation,
        from: u64,
        total: Option<u64>,
    ) -> Metering {
        let meter = Arc::new(Meter {
            serial: self.0.serials.fetch_add(1, Ordering::Relaxed),
            job,
            generation,
            total,
            started: (Instant::now(), from),
            received: AtomicU64::new(from),
        });
        self.0.meters().push(meter.clone());
        self.0.added.notify_one();
        Metering {
            shared: self.0.clone(),
            meter,
        }
    }

    /// Follows the latest samples. The current set counts as unseen, so a
    /// new subscriber learns about running transfers at once.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Samples> {
        let mut samples = self.0.samples.subscribe();
        samples.mark_changed();
        samples
    }

    /// Publishes samples while anything is metered, until `cancel` fires.
    pub(crate) async fn sample(self, cancel: CancellationToken) {
        let mut windows = HashMap::<u64, Window>::new();
        loop {
            // Waits for a meter; `notify_one` keeps the wake-up of one
            // added before this waits.
            while self.0.meters().is_empty() {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = self.0.added.notified() => {}
                }
            }
            let mut ticks = tokio::time::interval_at(Instant::now() + PERIOD, PERIOD);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    _ = ticks.tick() => {}
                }
                let meters = self.0.meters().clone();
                windows.retain(|serial, _| meters.iter().any(|m| m.serial == *serial));
                let now = Instant::now();
                let samples: Samples = meters
                    .iter()
                    .map(|meter| {
                        let received = meter.received.load(Ordering::Relaxed);
                        let speed = windows
                            .entry(meter.serial)
                            .or_insert_with(|| Window::new(meter.started))
                            .push(now, received);
                        ProgressSample {
                            boot_epoch: self.0.boot_epoch,
                            job: meter.job,
                            generation: meter.generation,
                            bytes_received: received,
                            bytes_total: meter.total,
                            bytes_per_second: speed,
                        }
                    })
                    .collect();
                // An empty set is published too, ending the last ones.
                self.0.samples.send_replace(samples);
                if meters.is_empty() {
                    break;
                }
            }
        }
    }
}

impl Shared {
    fn meters(&self) -> MutexGuard<'_, Vec<Arc<Meter>>> {
        // A panic while holding the lock leaves a valid list.
        self.meters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Positions seen over the last [`WINDOW`].
struct Window(VecDeque<(Instant, u64)>);

impl Window {
    fn new(start: (Instant, u64)) -> Self {
        Self(VecDeque::from([start]))
    }

    /// Adds the position at `now` and returns the bytes per second since
    /// the newest position at least [`WINDOW`] old, or the oldest one.
    fn push(&mut self, now: Instant, received: u64) -> u64 {
        self.0.push_back((now, received));
        while let Some(&(at, _)) = self.0.get(1)
            && now - at >= WINDOW
        {
            self.0.pop_front();
        }
        let Some(&(from, first)) = self.0.front() else {
            return 0;
        };
        let elapsed = (now - from).as_secs_f64();
        if elapsed <= 0.0 {
            return 0;
        }
        // Positions only grow, but a rate is never negative.
        (received.saturating_sub(first) as f64 / elapsed).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    fn job(n: u64) -> JobId {
        JobId::new(NonZeroU64::new(n).unwrap())
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn speed_covers_the_last_window() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut window = Window::new((start, 100));
        assert_eq!(window.push(at(1), 100 + MIB), MIB);
        assert_eq!(window.push(at(2), 100 + 3 * MIB), 3 * MIB / 2);
        // Steady 1 MiB/s from second 2 replaces the burst once the window
        // has passed it.
        for s in 3..=7 {
            window.push(at(s), 100 + 3 * MIB + (s - 2) * MIB);
        }
        assert_eq!(window.push(at(8), 100 + 9 * MIB), MIB);
        // A stall reaches zero after one window.
        for s in 9..=12 {
            assert!(window.push(at(s), 100 + 9 * MIB) > 0);
        }
        assert_eq!(window.push(at(13), 100 + 9 * MIB), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn samples_follow_meters_and_stop_with_them() {
        let progress = Progress::new(BootEpoch::INITIAL);
        let cancel = CancellationToken::new();
        let sampler = tokio::spawn(progress.clone().sample(cancel.clone()));
        let mut samples = progress.subscribe();
        // Nothing transfers yet: the initial empty set only.
        assert!(samples.borrow_and_update().is_empty());

        let added = Instant::now();
        let metering = progress.meter(job(7), Generation::INITIAL, 4096, Some(4096 + 10 * MIB));
        tokio::time::advance(Duration::from_millis(400)).await;
        metering.meter().reached(4096 + MIB / 2);
        samples.changed().await.unwrap();
        let first = samples.borrow_and_update()[0];
        assert_eq!(first.job, job(7));
        assert_eq!(first.generation, Generation::INITIAL);
        assert_eq!(first.bytes_received, 4096 + MIB / 2);
        assert_eq!(first.bytes_total, Some(4096 + 10 * MIB));
        // Measured from when the meter was added, not from the first tick.
        let elapsed = (Instant::now() - added).as_secs_f64();
        assert!(elapsed > PERIOD.as_secs_f64());
        let expected = ((MIB / 2) as f64 / elapsed).round() as u64;
        assert_eq!(first.bytes_per_second, expected);

        drop(metering);
        samples.changed().await.unwrap();
        assert!(samples.borrow_and_update().is_empty());
        // And then nothing more is published.
        let quiet = tokio::time::timeout(10 * PERIOD, samples.changed()).await;
        assert!(quiet.is_err());

        // A new meter wakes the sampler again.
        let _again = progress.meter(job(8), Generation::INITIAL, 0, None);
        samples.changed().await.unwrap();
        let again = samples.borrow_and_update()[0];
        assert_eq!(
            (again.job, again.bytes_total, again.bytes_per_second),
            (job(8), None, 0)
        );

        cancel.cancel();
        sampler.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_subscriber_sees_running_transfers_at_once() {
        let progress = Progress::new(BootEpoch::INITIAL);
        let cancel = CancellationToken::new();
        tokio::spawn(progress.clone().sample(cancel.clone()));
        let _metering = progress.meter(job(1), Generation::INITIAL, 0, None);
        let mut early = progress.subscribe();
        early.changed().await.unwrap();
        early.borrow_and_update();
        early.changed().await.unwrap();
        let mut late = progress.subscribe();
        assert!(late.has_changed().unwrap());
        assert_eq!(late.borrow_and_update().len(), 1);
        cancel.cancel();
    }
}
