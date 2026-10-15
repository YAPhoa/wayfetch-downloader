//! Engine configuration (architecture brief sections 4.6, 10 and 11.3,
//! `EngineConfig`).
//!
//! Holds engine settings only: no listener address, window, or UI option.
//! Every queue, buffer, and retention window the engine keeps is set here
//! with a default and a validated range, so nothing grows with the number
//! of jobs, events, or clients (section 10.3). The defaults are the brief's
//! initial targets, not measurements.

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

/// A configuration value is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A value lies outside its accepted range. Durations are in
    /// milliseconds.
    #[error("{setting} is {value}; it must be between {min} and {max}")]
    OutOfRange {
        /// The setting's name.
        setting: &'static str,
        /// The configured value.
        value: u64,
        /// Smallest accepted value.
        min: u64,
        /// Largest accepted value.
        max: u64,
    },
    /// A root is configured more than once.
    #[error("root {} is configured more than once", .0.display())]
    DuplicateRoot(PathBuf),
    /// A root's path is relative.
    #[error("root {} is not an absolute path", .0.display())]
    RelativeRoot(PathBuf),
    /// More roots than [`EngineConfig::MAX_ROOTS`].
    #[error("{0} roots are configured; at most {max} are supported", max = EngineConfig::MAX_ROOTS)]
    TooManyRoots(usize),
    /// The download root is not one of the configured roots.
    #[error("download root {} is not configured", .0.display())]
    UnknownDownloadRoot(PathBuf),
    /// The credential key file is not an absolute path outside the state
    /// directory: inside it, a copy of the directory would carry the key
    /// with the credentials it protects.
    #[error("credential key {} must be an absolute path outside the state directory", .0.display())]
    CredentialKey(PathBuf),
}

/// Bounds on the engine's queues, buffers, and retained history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Files transferring at once. Each has one stream and at most one
    /// read buffer in memory. Further jobs stay queued in the database, not
    /// in memory.
    pub active_files: NonZeroUsize,
    /// Largest body chunk a transport may hand over at once, the per-stream
    /// read buffer.
    pub read_buffer_bytes: NonZeroUsize,
    /// Bytes written between checkpoints. Smaller loses less on a crash and
    /// costs more syncs.
    pub checkpoint_bytes: NonZeroU64,
    /// Live events kept for subscribers. A subscriber that falls this far
    /// behind, or asks to replay more than this, is told to resynchronize.
    pub event_buffer: NonZeroUsize,
    /// Durable events kept in the outbox for replay; older ones are pruned.
    pub event_retention: NonZeroU64,
    /// How long a command's idempotency record is kept. A retry after this
    /// runs the command again.
    pub command_retention: Duration,
}

/// Separate deadlines for each phase of a request (section 10.4).
/// Operation and retry deadlines arrive with the retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Establishing a connection, enforced by the transport.
    pub connect: Duration,
    /// From sending a request to receiving the response headers, connecting
    /// included.
    pub response: Duration,
    /// Longest wait for the next body chunk. A stalled body fails instead
    /// of holding a transfer slot forever.
    pub idle_read: Duration,
}

/// Engine settings, checked by [`EngineConfig::validate`] when the engine
/// starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Directory the engine owns exclusively. Its parent must exist; the
    /// engine creates only the final component, so a missing mount is never
    /// papered over by creating a fresh tree.
    pub state_dir: PathBuf,
    /// Capacity of the coordinator's request queue. Requests beyond it are
    /// rejected with `Overloaded` instead of buffered.
    pub request_queue: NonZeroUsize,
    /// Default time allowed for a graceful shutdown before the engine
    /// reports unfinished work.
    pub shutdown_timeout: Duration,
    /// Destination roots the operator permits, by absolute path. Each must
    /// have been initialized with [`crate::initialize_root`]; the engine
    /// never does that implicitly. Clients may choose among these, and
    /// among roots a host registered on its user's request
    /// ([`crate::EngineHandle::register_root`]).
    pub roots: Vec<PathBuf>,
    /// Root that downloads are published in unless the user chose another
    /// default; one of `roots`. Without one, and without a chosen default,
    /// jobs stay queued.
    pub download_root: Option<PathBuf>,
    /// Queue, buffer, and retention bounds.
    pub limits: Limits,
    /// Request deadlines.
    pub timeouts: Timeouts,
    /// The file holding the key that encrypts provider credentials, kept
    /// outside the state directory and its database. Without one, no
    /// account can be added. The engine creates it, owner-only, only for a
    /// state directory that never stored a key; once one did, a missing or
    /// different key leaves accounts locked instead of being replaced.
    pub credential_key: Option<PathBuf>,
}

/// Accepted range of a setting, inclusive.
struct Bounds {
    setting: &'static str,
    min: u64,
    max: u64,
}

impl Bounds {
    fn check(&self, value: u64) -> Result<(), ConfigError> {
        if (self.min..=self.max).contains(&value) {
            Ok(())
        } else {
            Err(ConfigError::OutOfRange {
                setting: self.setting,
                value,
                min: self.min,
                max: self.max,
            })
        }
    }

    fn check_duration(&self, value: Duration) -> Result<(), ConfigError> {
        self.check(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }
}

const fn bounds(setting: &'static str, min: u64, max: u64) -> Bounds {
    Bounds { setting, min, max }
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const SECOND_MS: u64 = 1000;
const HOUR_MS: u64 = 3600 * SECOND_MS;

impl Limits {
    /// The brief's initial targets for the server profile (section 10.1).
    pub const DEFAULT: Self = Self {
        active_files: NonZeroUsize::new(4).unwrap(),
        read_buffer_bytes: NonZeroUsize::new(256 * 1024).unwrap(),
        checkpoint_bytes: NonZeroU64::new(8 * MIB).unwrap(),
        event_buffer: NonZeroUsize::new(1024).unwrap(),
        event_retention: NonZeroU64::new(10_000).unwrap(),
        command_retention: Duration::from_secs(24 * 3600),
    };

    fn validate(&self) -> Result<(), ConfigError> {
        bounds("limits.active_files", 1, 64).check(self.active_files.get() as u64)?;
        bounds("limits.read_buffer_bytes", 4 * KIB, 16 * MIB)
            .check(self.read_buffer_bytes.get() as u64)?;
        // A checkpoint covers at least one full read buffer.
        bounds(
            "limits.checkpoint_bytes",
            self.read_buffer_bytes.get() as u64,
            1024 * MIB,
        )
        .check(self.checkpoint_bytes.get())?;
        bounds("limits.event_buffer", 1, 65_536).check(self.event_buffer.get() as u64)?;
        bounds("limits.event_retention", 1, 10_000_000).check(self.event_retention.get())?;
        bounds(
            "limits.command_retention_ms",
            60 * SECOND_MS,
            30 * 24 * HOUR_MS,
        )
        .check_duration(self.command_retention)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Timeouts {
    /// Default deadlines.
    pub const DEFAULT: Self = Self {
        connect: Duration::from_secs(30),
        response: Duration::from_secs(60),
        idle_read: Duration::from_secs(60),
    };

    fn validate(&self) -> Result<(), ConfigError> {
        bounds("timeouts.connect_ms", 1, HOUR_MS).check_duration(self.connect)?;
        // Waiting for headers includes connecting.
        bounds(
            "timeouts.response_ms",
            u64::try_from(self.connect.as_millis()).unwrap_or(u64::MAX),
            HOUR_MS,
        )
        .check_duration(self.response)?;
        bounds("timeouts.idle_read_ms", 1, HOUR_MS).check_duration(self.idle_read)
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl EngineConfig {
    /// Default request queue capacity.
    pub const DEFAULT_REQUEST_QUEUE: NonZeroUsize = NonZeroUsize::new(256).unwrap();
    /// Largest accepted request queue capacity. The queue exists to bound
    /// pending work, so a capacity beyond this is a configuration mistake.
    pub const MAX_REQUEST_QUEUE: usize = 65_536;
    /// Default graceful shutdown budget.
    pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
    /// Most destination roots permitted at once, configured and registered
    /// together.
    pub const MAX_ROOTS: usize = 64;

    /// Configuration with defaults for everything but the state directory.
    /// It has no roots, so nothing is downloaded until one is added.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            request_queue: Self::DEFAULT_REQUEST_QUEUE,
            shutdown_timeout: Self::DEFAULT_SHUTDOWN_TIMEOUT,
            roots: Vec::new(),
            download_root: None,
            limits: Limits::DEFAULT,
            timeouts: Timeouts::DEFAULT,
            credential_key: None,
        }
    }

    /// Checks every value before the engine touches the state directory.
    pub fn validate(&self) -> Result<(), ConfigError> {
        bounds("request_queue", 1, Self::MAX_REQUEST_QUEUE as u64)
            .check(self.request_queue.get() as u64)?;
        bounds("shutdown_timeout_ms", 1, HOUR_MS).check_duration(self.shutdown_timeout)?;
        if self.roots.len() > Self::MAX_ROOTS {
            return Err(ConfigError::TooManyRoots(self.roots.len()));
        }
        for (i, root) in self.roots.iter().enumerate() {
            if !root.is_absolute() {
                return Err(ConfigError::RelativeRoot(root.clone()));
            }
            if self.roots[..i].contains(root) {
                return Err(ConfigError::DuplicateRoot(root.clone()));
            }
        }
        if let Some(root) = &self.download_root
            && !self.roots.contains(root)
        {
            return Err(ConfigError::UnknownDownloadRoot(root.clone()));
        }
        if let Some(key) = &self.credential_key
            && (!key.is_absolute() || key.starts_with(&self.state_dir))
        {
            return Err(ConfigError::CredentialKey(key.clone()));
        }
        self.limits.validate()?;
        self.timeouts.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EngineConfig {
        EngineConfig::new("/state")
    }

    fn out_of_range(config: &EngineConfig) -> &'static str {
        match config.validate() {
            Err(ConfigError::OutOfRange { setting, .. }) => setting,
            other => panic!("expected a range error, got {other:?}"),
        }
    }

    #[test]
    fn defaults_are_valid() {
        assert_eq!(config().validate(), Ok(()));
    }

    #[test]
    fn every_bound_is_enforced() {
        let mut c = config();
        c.request_queue = NonZeroUsize::MAX;
        assert_eq!(out_of_range(&c), "request_queue");

        let mut c = config();
        c.limits.active_files = NonZeroUsize::new(65).unwrap();
        assert_eq!(out_of_range(&c), "limits.active_files");

        let mut c = config();
        c.limits.read_buffer_bytes = NonZeroUsize::new(1).unwrap();
        assert_eq!(out_of_range(&c), "limits.read_buffer_bytes");

        let mut c = config();
        c.limits.checkpoint_bytes = NonZeroU64::new(KIB).unwrap();
        assert_eq!(out_of_range(&c), "limits.checkpoint_bytes");

        let mut c = config();
        c.limits.event_buffer = NonZeroUsize::new(1 << 20).unwrap();
        assert_eq!(out_of_range(&c), "limits.event_buffer");

        let mut c = config();
        c.limits.event_retention = NonZeroU64::MAX;
        assert_eq!(out_of_range(&c), "limits.event_retention");

        let mut c = config();
        c.limits.command_retention = Duration::from_secs(1);
        assert_eq!(out_of_range(&c), "limits.command_retention_ms");

        let mut c = config();
        c.timeouts.connect = Duration::ZERO;
        assert_eq!(out_of_range(&c), "timeouts.connect_ms");

        let mut c = config();
        c.timeouts.response = c.timeouts.connect / 2;
        assert_eq!(out_of_range(&c), "timeouts.response_ms");

        let mut c = config();
        c.timeouts.idle_read = Duration::MAX;
        assert_eq!(out_of_range(&c), "timeouts.idle_read_ms");

        let mut c = config();
        c.shutdown_timeout = Duration::ZERO;
        assert_eq!(out_of_range(&c), "shutdown_timeout_ms");
    }

    #[test]
    fn roots_must_be_unique_absolute_bounded_and_known() {
        let root = PathBuf::from("/downloads");
        let mut c = config();
        c.download_root = Some(root.clone());
        assert_eq!(
            c.validate(),
            Err(ConfigError::UnknownDownloadRoot(root.clone()))
        );
        c.roots.push(root.clone());
        assert_eq!(c.validate(), Ok(()));
        c.roots.push(root.clone());
        assert_eq!(c.validate(), Err(ConfigError::DuplicateRoot(root)));

        let mut c = config();
        c.roots.push("downloads".into());
        assert_eq!(
            c.validate(),
            Err(ConfigError::RelativeRoot("downloads".into()))
        );

        let mut c = config();
        c.roots = (0..=EngineConfig::MAX_ROOTS)
            .map(|i| PathBuf::from(format!("/r{i}")))
            .collect();
        assert_eq!(
            c.validate(),
            Err(ConfigError::TooManyRoots(EngineConfig::MAX_ROOTS + 1))
        );
    }
}
