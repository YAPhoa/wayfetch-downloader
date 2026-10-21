//! Engine configuration: engine settings only, no listener, window, or UI
//! options.
//!
//! Every queue, buffer, and retention window has a default and a validated
//! range, so nothing grows with the number of jobs, events, or clients.

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use wayfetch_protocol::query::SpeedLimit;

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
    /// A preference's policy permits nothing, or its default lies outside
    /// what it permits.
    #[error("{setting}: {problem}")]
    Policy {
        /// The setting's name.
        setting: &'static str,
        /// What is wrong.
        problem: String,
    },
    /// The credential key file is not an absolute path outside the state
    /// directory: inside it, a copy of the directory would carry the key
    /// with the credentials it protects.
    #[error("credential key {} must be an absolute path outside the state directory", .0.display())]
    CredentialKey(PathBuf),
}

/// Bounds on the engine's queues, buffers, and retained history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
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

/// Separate deadlines for each phase of a request.
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
    /// The operator's policy for preferences users change at run time.
    pub policy: Policy,
}

/// A preference users can change at run time, with a whole-number value:
/// its built-in default and the range the engine supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preference {
    /// The setting's name, as configuration and errors spell it.
    pub name: &'static str,
    /// Wayfetch's own default.
    pub built_in: u64,
    /// Smallest value the engine supports.
    pub min: u64,
    /// Largest value the engine supports.
    pub max: u64,
}

impl Preference {
    /// Files transferring at once, engine-wide. Each has one stream and at
    /// most one read buffer in memory; further jobs stay queued in the
    /// database, not in memory.
    pub const MAX_ACTIVE_DOWNLOADS: Self = Self {
        name: "downloads.max_active",
        built_in: 4,
        min: 1,
        max: 64,
    };
}

/// The operator's policy for one [`Preference`]: a default, the range a
/// user may choose from, and whether it is fixed. A default alone never
/// fixes the value; only `locked` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegerPolicy {
    /// The value without a saved preference; the built-in default if
    /// `None`.
    pub default: Option<u64>,
    /// Smallest value a user may choose.
    pub min: u64,
    /// Largest value a user may choose.
    pub max: u64,
    /// Whether the default is enforced: users cannot change it, and a
    /// saved preference is not used.
    pub locked: bool,
}

impl IntegerPolicy {
    /// No restriction beyond what the engine supports.
    pub const fn unrestricted(preference: Preference) -> Self {
        Self {
            default: None,
            min: preference.min,
            max: preference.max,
            locked: false,
        }
    }

    /// The value without a saved preference.
    pub fn default_value(&self, preference: Preference) -> u64 {
        self.default.unwrap_or(preference.built_in)
    }

    fn validate(&self, preference: Preference) -> Result<(), ConfigError> {
        let problem = |problem: String| ConfigError::Policy {
            setting: preference.name,
            problem,
        };
        let supported = preference.min..=preference.max;
        if !supported.contains(&self.min) || !supported.contains(&self.max) || self.min > self.max {
            return Err(problem(format!(
                "the permitted range {}..={} must lie within {}..={}",
                self.min, self.max, preference.min, preference.max
            )));
        }
        let default = self.default_value(preference);
        if !(self.min..=self.max).contains(&default) {
            return Err(problem(match self.default {
                Some(_) => format!(
                    "the default {default} lies outside the permitted range {}..={}",
                    self.min, self.max
                ),
                None => format!(
                    "the built-in default {default} lies outside the permitted range {}..={}; \
                     configure a default within it",
                    self.min, self.max
                ),
            }));
        }
        Ok(())
    }
}

/// The operator's policy for the total download speed limit, in bytes per
/// second. Unlimited is the built-in default. A finite `max` is a ceiling
/// Unlimited cannot bypass: with one, the default must be finite, and a
/// saved Unlimited runs at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedLimitPolicy {
    /// The limit without a saved preference; Unlimited if `None`.
    pub default: Option<SpeedLimit>,
    /// Smallest finite limit a user may choose.
    pub min: u64,
    /// Largest finite limit a user may choose, which also forbids
    /// Unlimited; `None` permits Unlimited and every supported finite
    /// limit from `min`.
    pub max: Option<u64>,
    /// Whether the default is enforced: users cannot change it, and a
    /// saved preference is not used.
    pub locked: bool,
}

impl SpeedLimitPolicy {
    /// The setting's name, as configuration and errors spell it.
    pub const NAME: &'static str = "downloads.speed_limit";
    /// Smallest finite limit the engine supports: 1 KiB/s.
    pub const MIN: u64 = KIB;
    /// Largest finite limit the engine supports: 1 TiB/s. It fits a
    /// JavaScript number exactly.
    pub const MAX: u64 = 1 << 40;

    /// No restriction beyond what the engine supports.
    pub const UNRESTRICTED: Self = Self {
        default: None,
        min: Self::MIN,
        max: None,
        locked: false,
    };

    /// The limit without a saved preference.
    pub fn default_value(&self) -> SpeedLimit {
        self.default.unwrap_or(SpeedLimit::Unlimited)
    }

    /// The largest finite limit permitted.
    pub fn ceiling(&self) -> u64 {
        self.max.unwrap_or(Self::MAX)
    }

    /// Whether `limit` is permitted.
    pub fn permits(&self, limit: SpeedLimit) -> bool {
        match limit {
            SpeedLimit::Unlimited => self.max.is_none(),
            SpeedLimit::BytesPerSecond(rate) => (self.min..=self.ceiling()).contains(&rate),
        }
    }

    /// The permitted limit nearest `limit`: a finite one clamped into the
    /// range, and Unlimited lowered to the ceiling when it is forbidden.
    pub fn nearest(&self, limit: SpeedLimit) -> SpeedLimit {
        match (limit, self.max) {
            (SpeedLimit::Unlimited, None) => SpeedLimit::Unlimited,
            (SpeedLimit::Unlimited, Some(max)) => SpeedLimit::BytesPerSecond(max),
            (SpeedLimit::BytesPerSecond(rate), _) => {
                SpeedLimit::BytesPerSecond(rate.clamp(self.min, self.ceiling()))
            }
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let problem = |problem: String| ConfigError::Policy {
            setting: Self::NAME,
            problem,
        };
        let supported = Self::MIN..=Self::MAX;
        if !supported.contains(&self.min)
            || !supported.contains(&self.ceiling())
            || self.min > self.ceiling()
        {
            return Err(problem(format!(
                "the permitted range {}..={} bytes per second must lie within {}..={}",
                self.min,
                self.ceiling(),
                Self::MIN,
                Self::MAX
            )));
        }
        match self.default_value() {
            limit if self.permits(limit) => Ok(()),
            SpeedLimit::Unlimited => Err(problem(
                "a maximum forbids Unlimited, so configure a finite default within it".to_owned(),
            )),
            SpeedLimit::BytesPerSecond(rate) => Err(problem(format!(
                "the default {rate} lies outside the permitted range {}..={} bytes per second",
                self.min,
                self.ceiling()
            ))),
        }
    }
}

/// The operator's policy for every run-time preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// [`Preference::MAX_ACTIVE_DOWNLOADS`].
    pub max_active_downloads: IntegerPolicy,
    /// The total download speed limit.
    pub download_speed_limit: SpeedLimitPolicy,
}

impl Policy {
    /// Built-in defaults, and every supported value permitted.
    pub const DEFAULT: Self = Self {
        max_active_downloads: IntegerPolicy::unrestricted(Preference::MAX_ACTIVE_DOWNLOADS),
        download_speed_limit: SpeedLimitPolicy::UNRESTRICTED,
    };

    fn validate(&self) -> Result<(), ConfigError> {
        self.max_active_downloads
            .validate(Preference::MAX_ACTIVE_DOWNLOADS)?;
        self.download_speed_limit.validate()
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::DEFAULT
    }
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
    /// Defaults for the server profile.
    pub const DEFAULT: Self = Self {
        read_buffer_bytes: NonZeroUsize::new(256 * 1024).unwrap(),
        checkpoint_bytes: NonZeroU64::new(8 * MIB).unwrap(),
        event_buffer: NonZeroUsize::new(1024).unwrap(),
        event_retention: NonZeroU64::new(10_000).unwrap(),
        command_retention: Duration::from_secs(24 * 3600),
    };

    fn validate(&self) -> Result<(), ConfigError> {
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
            policy: Policy::DEFAULT,
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
        self.timeouts.validate()?;
        self.policy.validate()
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
    fn a_policy_permits_its_default_within_what_is_supported() {
        let max_active = |default, min, max| {
            let mut c = config();
            c.policy.max_active_downloads = IntegerPolicy {
                default,
                min,
                max,
                locked: false,
            };
            c.validate()
        };
        assert_eq!(max_active(Some(8), 2, 16), Ok(()));
        assert_eq!(max_active(None, 1, 64), Ok(()));
        for (default, min, max) in [
            (None, 0, 8),
            (None, 1, 65),
            (None, 9, 8),
            (Some(20), 1, 16),
            // The built-in 4 is outside, so a default must be configured.
            (None, 8, 16),
        ] {
            assert!(
                matches!(
                    max_active(default, min, max),
                    Err(ConfigError::Policy {
                        setting: "downloads.max_active",
                        ..
                    })
                ),
                "{default:?} {min} {max}"
            );
        }
    }

    #[test]
    fn a_speed_limit_ceiling_forbids_unlimited() {
        let speed = |default, min, max| {
            let mut c = config();
            c.policy.download_speed_limit = SpeedLimitPolicy {
                default,
                min,
                max,
                locked: false,
            };
            c.validate()
        };
        let mib = SpeedLimit::BytesPerSecond(MIB);
        assert_eq!(speed(None, KIB, None), Ok(()));
        assert_eq!(speed(Some(mib), KIB, Some(4 * MIB)), Ok(()));
        assert_eq!(speed(Some(SpeedLimit::Unlimited), 64 * KIB, None), Ok(()));
        for (default, min, max) in [
            // Unlimited, built in or configured, would bypass a maximum.
            (None, KIB, Some(4 * MIB)),
            (Some(SpeedLimit::Unlimited), KIB, Some(4 * MIB)),
            (
                Some(SpeedLimit::BytesPerSecond(8 * MIB)),
                KIB,
                Some(4 * MIB),
            ),
            (Some(SpeedLimit::BytesPerSecond(KIB)), 2 * KIB, None),
            (None, 0, None),
            (None, KIB, Some(SpeedLimitPolicy::MAX + 1)),
            (Some(mib), 2 * MIB, Some(MIB)),
        ] {
            assert!(
                matches!(
                    speed(default, min, max),
                    Err(ConfigError::Policy {
                        setting: "downloads.speed_limit",
                        ..
                    })
                ),
                "{default:?} {min} {max:?}"
            );
        }
        let capped = SpeedLimitPolicy {
            max: Some(4 * MIB),
            ..SpeedLimitPolicy::UNRESTRICTED
        };
        assert!(!capped.permits(SpeedLimit::Unlimited));
        assert_eq!(
            capped.nearest(SpeedLimit::Unlimited),
            SpeedLimit::BytesPerSecond(4 * MIB)
        );
        assert_eq!(
            capped.nearest(SpeedLimit::BytesPerSecond(1)),
            SpeedLimit::BytesPerSecond(KIB)
        );
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
