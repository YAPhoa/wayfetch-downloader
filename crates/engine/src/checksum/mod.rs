//! Checking complete downloads against checksums: those a checksum file
//! downloaded in the same batch lists, and those a provider gives.
//!
//! A checksum file's names only refer to recorded downloads; no file is ever
//! opened by a name it lists. See `docs/architecture.md`, "Amendment (file
//! checks)".

pub mod manifest;

use std::any::Any;
use std::fmt;
use std::io::Read;
use std::sync::Arc;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
pub use wayfetch_protocol::query::ChecksumAlgorithm as Algorithm;

/// Files hashed at once, engine-wide.
pub const MAX_HASHING: usize = 2;

/// Bytes read per step; cancellation is checked between steps.
const CHUNK: usize = 1 << 20;

/// Lowercase hexadecimal digits of a hash value.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Digits of a value of `algorithm`.
pub fn hex_len(algorithm: Algorithm) -> usize {
    match algorithm {
        Algorithm::Crc32 => 8,
        Algorithm::Md5 => 32,
        Algorithm::Sha1 => 40,
        Algorithm::Sha256 => 64,
    }
}

/// A hash value: its algorithm and its digits, lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algorithm: Algorithm,
    value: String,
}

impl Digest {
    /// `digits` as a value of `algorithm`, if it has exactly the right
    /// number of hexadecimal digits, in either case.
    pub fn parse(algorithm: Algorithm, digits: &str) -> Option<Self> {
        (digits.len() == hex_len(algorithm) && digits.bytes().all(|b| b.is_ascii_hexdigit())).then(
            || Self {
                algorithm,
                value: digits.to_ascii_lowercase(),
            },
        )
    }

    /// Its algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Its digits, lowercase.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl std::str::FromStr for Digest {
    type Err = ();

    /// Reads the form [`Digest`] displays as: `algorithm:hex`.
    fn from_str(s: &str) -> Result<Self, ()> {
        let (algorithm, value) = s.split_once(':').ok_or(())?;
        Self::parse(algorithm.parse().map_err(|_| ())?, value).ok_or(())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm, self.value)
    }
}

/// Hashes being computed over one stream of bytes.
#[derive(Default)]
struct Hashers {
    crc32: Option<crc32fast::Hasher>,
    md5: Option<Md5>,
    sha1: Option<Sha1>,
    sha256: Option<Sha256>,
}

impl Hashers {
    fn new(algorithms: &[Algorithm]) -> Self {
        let mut hashers = Self::default();
        for algorithm in algorithms {
            match algorithm {
                Algorithm::Crc32 => hashers.crc32 = Some(crc32fast::Hasher::new()),
                Algorithm::Md5 => hashers.md5 = Some(Md5::new()),
                Algorithm::Sha1 => hashers.sha1 = Some(Sha1::new()),
                Algorithm::Sha256 => hashers.sha256 = Some(Sha256::new()),
            }
        }
        hashers
    }

    fn update(&mut self, bytes: &[u8]) {
        if let Some(h) = &mut self.crc32 {
            h.update(bytes);
        }
        if let Some(h) = &mut self.md5 {
            h.update(bytes);
        }
        if let Some(h) = &mut self.sha1 {
            h.update(bytes);
        }
        if let Some(h) = &mut self.sha256 {
            h.update(bytes);
        }
    }

    fn finish(self) -> Vec<Digest> {
        let mut digests = Vec::new();
        let mut push = |algorithm, value| digests.push(Digest { algorithm, value });
        if let Some(h) = self.crc32 {
            push(Algorithm::Crc32, format!("{:08x}", h.finalize()));
        }
        if let Some(h) = self.md5 {
            push(Algorithm::Md5, hex(&h.finalize()));
        }
        if let Some(h) = self.sha1 {
            push(Algorithm::Sha1, hex(&h.finalize()));
        }
        if let Some(h) = self.sha256 {
            push(Algorithm::Sha256, hex(&h.finalize()));
        }
        digests
    }
}

/// Reads `source` to its end once, computing each of `algorithms`. `None`
/// if `stop` was cancelled first; it is checked between chunks, so a read
/// the kernel is blocked in still finishes first.
pub fn hash(
    source: &mut impl Read,
    algorithms: &[Algorithm],
    stop: &CancellationToken,
) -> std::io::Result<Option<Vec<Digest>>> {
    let mut hashers = Hashers::new(algorithms);
    let mut buffer = vec![0; CHUNK];
    loop {
        if stop.is_cancelled() {
            return Ok(None);
        }
        match source.read(&mut buffer) {
            Ok(0) => return Ok(Some(hashers.finish())),
            Ok(n) => hashers.update(&buffer[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// The engine-wide bound on files being hashed.
#[derive(Clone)]
pub(crate) struct Hashing {
    permits: Arc<Semaphore>,
    /// Held by every hashing thread until it returns, so the state
    /// directory stays locked while one may still read a file.
    guard: Arc<dyn Any + Send + Sync>,
}

impl Hashing {
    pub(crate) fn new(guard: Arc<dyn Any + Send + Sync>) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_HASHING)),
            guard,
        }
    }

    /// Runs `work` on a blocking thread once a permit is free. The permit
    /// and guard move into the thread and are released only when `work`
    /// returns, even if whoever awaits it gave up, since a started
    /// blocking thread cannot be aborted. `None` if it could not run.
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let permit = self.permits.clone().acquire_owned().await.ok()?;
        let guard = self.guard.clone();
        tokio::task::spawn_blocking(move || {
            let _held = (permit, guard);
            work()
        })
        .await
        .ok()
    }

    /// Runs `work`, which is not hashing, on a blocking thread without a
    /// permit, holding the guard until it returns.
    pub(crate) async fn run_unbounded<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let guard = self.guard.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            work()
        })
        .await
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        let digests = hash(
            &mut &b"The quick brown fox jumps over the lazy dog"[..],
            &[
                Algorithm::Crc32,
                Algorithm::Md5,
                Algorithm::Sha1,
                Algorithm::Sha256,
            ],
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let values: Vec<_> = digests.iter().map(ToString::to_string).collect();
        assert_eq!(
            values,
            [
                "crc32:414fa339",
                "md5:9e107d9d372bb6826bd81d3542a419d6",
                "sha1:2fd4e1c67a2d28fced849ee1bb76e7391b93eb12",
                "sha256:d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
            ]
        );
    }

    #[test]
    fn a_cancelled_hash_stops() {
        let stop = CancellationToken::new();
        stop.cancel();
        assert_eq!(
            hash(&mut &b"x"[..], &[Algorithm::Md5], &stop).unwrap(),
            None
        );
    }

    #[test]
    fn values_read_back_as_displayed() {
        let digest = Digest::parse(Algorithm::Sha256, &"Ab".repeat(32)).unwrap();
        assert_eq!(digest.to_string().parse::<Digest>(), Ok(digest));
        assert!("md5:00".parse::<Digest>().is_err());
        assert!("blake3:00".parse::<Digest>().is_err());
    }

    #[test]
    fn values_parse_by_length_and_digits() {
        assert_eq!(
            Digest::parse(Algorithm::Crc32, "414FA339").unwrap().value(),
            "414fa339"
        );
        assert!(Digest::parse(Algorithm::Crc32, "414fa33").is_none());
        assert!(Digest::parse(Algorithm::Crc32, "414fa33g").is_none());
        assert!(Digest::parse(Algorithm::Md5, "414fa339").is_none());
    }

    #[tokio::test]
    async fn hashing_holds_its_permit_until_the_thread_returns() {
        let hashing = Hashing::new(Arc::new(()));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = Arc::new(std::sync::Mutex::new(wait));
        let (started, mut running) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..MAX_HASHING {
            let (hashing, wait, started) = (hashing.clone(), wait.clone(), started.clone());
            let blocked = tokio::spawn(async move {
                hashing
                    .run(move || {
                        let _ = started.send(());
                        wait.lock().unwrap().recv()
                    })
                    .await
            });
            running.recv().await.unwrap();
            // Abandoning the awaiter does not free the permit.
            blocked.abort();
        }
        let next = hashing.run(|| ());
        tokio::pin!(next);
        tokio::select! {
            _ = &mut next => panic!("ran while every permit was held"),
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        release.send(()).unwrap();
        assert_eq!(next.await, Some(()));
        release.send(()).unwrap();
    }
}
