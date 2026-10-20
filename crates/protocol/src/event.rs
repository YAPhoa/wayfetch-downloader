//! Event subscriptions and resynchronization (architecture brief sections
//! 4.7, 7.8 and ADR-28; tests T47 and T66).
//!
//! There are two kinds of events:
//!
//! * **Durable events** come from the engine's outbox. Each has a position
//!   that increases by one per event and is committed with the state change
//!   it describes, so it survives restarts. Delivery is at least once:
//!   clients ignore any position they have already applied.
//! * **Progress samples** are ephemeral, coalesced, and may be dropped. They
//!   are tagged with the boot epoch and job generation; a client discards
//!   samples from a previous engine run or an older generation.
//!
//! A client keeps an authoritative view with this loop:
//!
//! 1. Query a snapshot. Its `as_of` cursor marks the last event it reflects.
//! 2. Subscribe after that cursor. The engine replays newer events, then
//!    streams live ones.
//! 3. On [`StreamItem::Resync`], drop the subscription and go back to step 1.
//!
//! The engine never buffers without bound for a slow client: when a client's
//! queue fills, it sends `Resync` with [`ResyncReason::Lagged`] and ends the
//! stream. Nothing about a subscription affects job execution.

use serde::{Deserialize, Serialize};
use wayfetch_domain::id::{BatchId, JobId};
use wayfetch_domain::version::{BootEpoch, Generation, Revision};

use crate::instance::{Hello, InstanceId};
use crate::query::JobSummary;

/// A position in one instance's durable event sequence. Position 0 is
/// before the first event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct EventCursor {
    /// The instance the position belongs to.
    pub instance: InstanceId,
    /// Position of the last event covered.
    pub position: u64,
}

/// A change recorded in the outbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum EventKind {
    /// A batch was accepted for ingestion.
    BatchAccepted {
        /// The batch.
        batch: BatchId,
        /// Inputs in the batch.
        accepted: u32,
    },
    /// A job was created.
    JobAdded {
        /// Its state at creation.
        job: Box<JobSummary>,
    },
    /// A job's durable state changed. Replaces the client's copy.
    JobChanged {
        /// Its new state.
        job: Box<JobSummary>,
    },
    /// A job was removed from lists and counts. The client drops its copy.
    JobRemoved {
        /// The job.
        job: JobId,
    },
    /// The settings changed: a client reads them again, unless it holds
    /// unsaved changes, which would now conflict.
    SettingsChanged {
        /// The settings revision after the change.
        revision: Revision,
    },
    /// What is known about collected links changed without a command, such
    /// as a media link's title arriving: a client reads the Links list
    /// again.
    LinksChanged,
}

/// A durable event with its position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DurableEvent {
    /// Where this event sits in the sequence.
    pub cursor: EventCursor,
    /// What changed.
    pub kind: EventKind,
}

/// Ephemeral transfer progress for one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProgressSample {
    /// Engine run that produced the sample.
    pub boot_epoch: BootEpoch,
    /// The job.
    pub job: JobId,
    /// Generation of the work that produced the sample.
    pub generation: Generation,
    /// Bytes received, including bytes not yet durably checkpointed.
    pub bytes_received: u64,
    /// Expected total, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    /// Recent throughput.
    pub bytes_per_second: u64,
}

impl ProgressSample {
    /// Whether the sample belongs to the current engine run and the job
    /// generation the client knows. Anything else is stale and ignored.
    pub fn is_current(&self, boot_epoch: BootEpoch, generation: Generation) -> bool {
        self.boot_epoch == boot_epoch && self.generation == generation
    }
}

/// Why a subscription cannot continue from the requested cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ResyncReason {
    /// The cursor belongs to another instance.
    UnknownInstance,
    /// Events after the cursor were pruned by retention.
    CursorExpired,
    /// The cursor is ahead of the newest event, for example after the state
    /// directory was restored from an older backup.
    CursorAhead,
    /// The client fell behind and its bounded queue overflowed.
    Lagged,
}

/// A subscription request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscribe {
    /// Replay events after this cursor. Absent means "from now"; only use
    /// that when no snapshot needs to stay consistent.
    #[serde(default)]
    pub after: Option<EventCursor>,
}

/// One item on a subscription stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum StreamItem {
    /// Always first: the instance, boot epoch, and current head.
    Hello(Hello),
    /// A durable change.
    Event(DurableEvent),
    /// Ephemeral progress.
    Progress(ProgressSample),
    /// The client must re-query and subscribe again. Always last.
    Resync {
        /// Why.
        reason: ResyncReason,
    },
}

/// The durable events an instance can still replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retained {
    /// The instance.
    pub instance: InstanceId,
    /// Events at or before this position were pruned (0 if none were).
    pub pruned_through: u64,
    /// Position of the newest event (0 if there are none).
    pub head: u64,
}

/// How a subscription starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionStart {
    /// Replay events with positions greater than this, then stream live.
    ReplayAfter(u64),
    /// Send [`StreamItem::Resync`] and end the stream.
    Resync(ResyncReason),
}

impl Retained {
    /// Decides how to answer a subscription request. Every transport uses
    /// this, so HTTP and desktop clients resynchronize identically (T62).
    pub fn plan(&self, request: Subscribe) -> SubscriptionStart {
        let Some(after) = request.after else {
            return SubscriptionStart::ReplayAfter(self.head);
        };
        if after.instance != self.instance {
            SubscriptionStart::Resync(ResyncReason::UnknownInstance)
        } else if after.position > self.head {
            SubscriptionStart::Resync(ResyncReason::CursorAhead)
        } else if after.position < self.pruned_through {
            SubscriptionStart::Resync(ResyncReason::CursorExpired)
        } else {
            SubscriptionStart::ReplayAfter(after.position)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn retained(instance: InstanceId) -> Retained {
        Retained {
            instance,
            pruned_through: 10,
            head: 20,
        }
    }

    fn after(instance: InstanceId, position: u64) -> Subscribe {
        Subscribe {
            after: Some(EventCursor { instance, position }),
        }
    }

    #[test]
    fn replays_from_a_retained_cursor() {
        let id = InstanceId::generate();
        let r = retained(id);
        assert_eq!(r.plan(after(id, 10)), SubscriptionStart::ReplayAfter(10));
        assert_eq!(r.plan(after(id, 15)), SubscriptionStart::ReplayAfter(15));
        assert_eq!(r.plan(after(id, 20)), SubscriptionStart::ReplayAfter(20));
        assert_eq!(
            r.plan(Subscribe { after: None }),
            SubscriptionStart::ReplayAfter(20)
        );
    }

    #[test]
    fn resyncs_when_the_cursor_cannot_be_honoured() {
        let id = InstanceId::generate();
        let r = retained(id);
        assert_eq!(
            r.plan(after(id, 9)),
            SubscriptionStart::Resync(ResyncReason::CursorExpired)
        );
        assert_eq!(
            r.plan(after(id, 21)),
            SubscriptionStart::Resync(ResyncReason::CursorAhead)
        );
        assert_eq!(
            r.plan(after(InstanceId::generate(), 15)),
            SubscriptionStart::Resync(ResyncReason::UnknownInstance)
        );
    }

    #[test]
    fn empty_log_replays_from_zero() {
        let id = InstanceId::generate();
        let r = Retained {
            instance: id,
            pruned_through: 0,
            head: 0,
        };
        assert_eq!(r.plan(after(id, 0)), SubscriptionStart::ReplayAfter(0));
    }

    #[test]
    fn stale_progress_is_ignored() {
        let epoch = BootEpoch::INITIAL;
        let generation = Generation::INITIAL;
        let sample = ProgressSample {
            boot_epoch: epoch,
            job: JobId::from_u64(1).unwrap(),
            generation,
            bytes_received: 10,
            bytes_total: None,
            bytes_per_second: 5,
        };
        assert!(sample.is_current(epoch, generation));
        assert!(!sample.is_current(epoch.next().unwrap(), generation));
        assert!(!sample.is_current(epoch, generation.next().unwrap()));
    }

    #[test]
    fn older_decoders_ignore_new_fields_but_not_new_event_kinds() {
        // The compatibility rule on `ProtocolVersion`: a minor version may
        // add optional fields, but a new event kind needs a major version.
        let item = StreamItem::Event(DurableEvent {
            cursor: EventCursor {
                instance: InstanceId::generate(),
                position: 7,
            },
            kind: EventKind::BatchAccepted {
                batch: BatchId::from_u64(1).unwrap(),
                accepted: 2,
            },
        });
        let mut json = serde_json::to_value(&item).unwrap();
        json["added_in_minor"] = json!(true);
        json["kind"]["added_in_minor"] = json!("x");
        assert_eq!(
            serde_json::from_value::<StreamItem>(json.clone()).unwrap(),
            item
        );

        json["kind"]["type"] = json!("added_in_major");
        assert!(serde_json::from_value::<StreamItem>(json).is_err());
    }

    #[test]
    fn resync_wire_form() {
        let item = StreamItem::Resync {
            reason: ResyncReason::Lagged,
        };
        assert_eq!(
            serde_json::to_value(item).unwrap(),
            json!({"type": "resync", "reason": "lagged"})
        );
    }
}
