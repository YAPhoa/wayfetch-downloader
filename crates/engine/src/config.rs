//! Engine configuration (architecture brief section 4.6, `EngineConfig`).
//!
//! Holds engine settings only: no listener address, window, or UI option.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

/// A configuration value is out of range.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The request queue capacity exceeds [`EngineConfig::MAX_REQUEST_QUEUE`].
    #[error("request queue capacity {0} exceeds the maximum of {max}", max = EngineConfig::MAX_REQUEST_QUEUE)]
    RequestQueueTooLarge(usize),
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
}

impl EngineConfig {
    /// Default request queue capacity.
    pub const DEFAULT_REQUEST_QUEUE: NonZeroUsize = NonZeroUsize::new(256).unwrap();
    /// Largest accepted request queue capacity. The queue exists to bound
    /// pending work, so a capacity beyond this is a configuration mistake.
    pub const MAX_REQUEST_QUEUE: usize = 65_536;
    /// Default graceful shutdown budget.
    pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

    /// Configuration with defaults for everything but the state directory.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            request_queue: Self::DEFAULT_REQUEST_QUEUE,
            shutdown_timeout: Self::DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }

    /// Checks every value before the engine touches the state directory.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let queue = self.request_queue.get();
        if queue > Self::MAX_REQUEST_QUEUE {
            return Err(ConfigError::RequestQueueTooLarge(queue));
        }
        Ok(())
    }
}
