//! Streaming a response body into a staging file with backpressure,
//! cancellation, and checkpoints (sections 7.5 and 7.6; tests T08, T09,
//! T23, and T27).
//!
//! The writer pulls the next chunk only after the previous one is on disk,
//! so a slow disk slows the network read instead of filling memory: at most
//! one chunk per writer is resident, plus whatever the HTTP client buffers.
//! Writes run on the blocking pool and never stall the runtime.

use std::future::Future;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::fs::{StagingFile, SyncedRanges};

/// A response body, read chunk by chunk. The HTTP layer adapts its client's
/// body stream to this.
pub trait ChunkSource: Send {
    /// The next chunk, or `None` at the end of the body.
    fn next_chunk(&mut self) -> impl Future<Output = io::Result<Option<Bytes>>> + Send;
}

/// Why streaming stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// Reading the body failed.
    #[error("reading the body: {0}")]
    Source(#[source] io::Error),
    /// Writing or synchronizing the staging file failed. Never progress
    /// (section 7.6).
    #[error("writing the staging file: {0}")]
    File(#[source] io::Error),
    /// The body extends past the writer's interval; the excess chunk was not
    /// written.
    #[error("the body extends past byte {end}")]
    Overflow {
        /// End of the writer's interval.
        end: u64,
    },
    /// The body ended before the writer's interval was filled.
    #[error("the body ended at byte {at}, expected {end}")]
    Truncated {
        /// Where the body ended.
        at: u64,
        /// End of the writer's interval.
        end: u64,
    },
    /// Nobody receives checkpoints any more.
    #[error("checkpoint receiver closed")]
    CheckpointsClosed,
}

/// How streaming ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The body ended and every written byte was certified.
    Finished,
    /// Cancellation was requested. Bytes written since the last checkpoint
    /// are not certified; the caller may still [`IntervalWriter::checkpoint`]
    /// for a pause, which keeps the generation.
    Cancelled,
}

/// Writes one interval of a staging file, sequentially from its start.
/// Each writer owns its interval and cannot write outside it (rule 7.5.9).
#[derive(Debug)]
pub struct IntervalWriter {
    file: Arc<StagingFile>,
    end: Option<u64>,
    position: u64,
    certified: u64,
}

impl IntervalWriter {
    /// A writer for `start..end` of `file`, or from `start` to the end of
    /// the body when `end` is `None`.
    pub fn new(file: Arc<StagingFile>, start: u64, end: Option<u64>) -> Self {
        Self {
            file,
            end,
            position: start,
            certified: start,
        }
    }

    /// The next byte to write.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Writes `chunk` at the current position. A chunk that would cross the
    /// interval's end is refused whole.
    pub async fn write(&mut self, chunk: Bytes) -> Result<(), WriteError> {
        let len = chunk.len() as u64;
        let next = self
            .position
            .checked_add(len)
            .ok_or(WriteError::Overflow { end: u64::MAX })?;
        if let Some(end) = self.end
            && next > end
        {
            return Err(WriteError::Overflow { end });
        }
        if len == 0 {
            return Ok(());
        }
        let (file, offset) = (self.file.clone(), self.position);
        blocking(move || file.write_all_at(&chunk, offset)).await?;
        self.position = next;
        Ok(())
    }

    /// Synchronizes the file and certifies the bytes written since the last
    /// checkpoint, or returns `None` if there are none. The store commits
    /// them; only then are they restart-durable.
    pub async fn checkpoint(&mut self) -> Result<Option<SyncedRanges>, WriteError> {
        if self.position == self.certified {
            return Ok(None);
        }
        let (file, range) = (self.file.clone(), self.certified..self.position);
        let synced = blocking(move || file.sync_ranges(vec![range])).await?;
        self.certified = self.position;
        Ok(Some(synced))
    }

    /// Streams `source` into the interval until the body ends or `cancel`
    /// fires, sending a checkpoint to `checkpoints` whenever `every` bytes
    /// are uncertified and once at the end. Sending waits for room in the
    /// channel, so a coordinator that commits slowly slows the transfer
    /// instead of queueing certificates.
    ///
    /// Cancellation is checked before each read and while waiting; a write
    /// already handed to the blocking pool finishes first. A finished body
    /// must have filled the interval exactly when its end is known.
    pub async fn stream(
        &mut self,
        source: &mut impl ChunkSource,
        cancel: &CancellationToken,
        checkpoints: &mpsc::Sender<SyncedRanges>,
        every: u64,
    ) -> Result<StreamEnd, WriteError> {
        loop {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(StreamEnd::Cancelled),
                chunk = source.next_chunk() => chunk.map_err(WriteError::Source)?,
            };
            let Some(chunk) = chunk else { break };
            self.write(chunk).await?;
            if self.position - self.certified >= every
                && let Some(synced) = self.checkpoint().await?
                && !send(checkpoints, synced, cancel).await?
            {
                return Ok(StreamEnd::Cancelled);
            }
        }
        if let Some(end) = self.end
            && self.position != end
        {
            return Err(WriteError::Truncated {
                at: self.position,
                end,
            });
        }
        if let Some(synced) = self.checkpoint().await?
            && !send(checkpoints, synced, cancel).await?
        {
            return Ok(StreamEnd::Cancelled);
        }
        Ok(StreamEnd::Finished)
    }
}

/// Sends a checkpoint, or returns `false` if cancelled while waiting.
async fn send(
    checkpoints: &mpsc::Sender<SyncedRanges>,
    synced: SyncedRanges,
    cancel: &CancellationToken,
) -> Result<bool, WriteError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Ok(false),
        sent = checkpoints.send(synced) => sent.map(|()| true).map_err(|_| WriteError::CheckpointsClosed),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, WriteError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| WriteError::File(io::Error::other(e)))?
        .map_err(WriteError::File)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use wayfetch_domain::id::JobId;
    use wayfetch_domain::version::Generation;

    use super::*;
    use crate::fs::Root;

    struct Chunks(VecDeque<io::Result<Bytes>>);

    impl Chunks {
        fn of(parts: &[&'static [u8]]) -> Self {
            Self(parts.iter().map(|p| Ok(Bytes::from_static(p))).collect())
        }
    }

    impl ChunkSource for Chunks {
        async fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
            self.0.pop_front().transpose()
        }
    }

    /// Yields its chunks, then never ends: a stalled body. Counts pulls.
    struct Stalls(Chunks, Arc<AtomicUsize>);

    impl ChunkSource for Stalls {
        async fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            match self.0.next_chunk().await? {
                Some(chunk) => Ok(Some(chunk)),
                None => std::future::pending().await,
            }
        }
    }

    fn staging() -> (tempfile::TempDir, Root, Arc<StagingFile>) {
        let (tmp, root) = crate::fs::test_root();
        let file = root
            .create_staging(JobId::from_u64(1).unwrap(), Generation::INITIAL)
            .unwrap();
        (tmp, root, Arc::new(file))
    }

    #[tokio::test]
    async fn streams_into_the_interval_with_checkpoints() {
        let (tmp, _root, file) = staging();
        let (tx, mut rx) = mpsc::channel(8);
        let mut writer = IntervalWriter::new(file.clone(), 4, Some(10));
        let mut body = Chunks::of(&[b"ab", b"", b"cd", b"ef"]);
        let end = writer
            .stream(&mut body, &CancellationToken::new(), &tx, 3)
            .await
            .unwrap();
        assert_eq!(end, StreamEnd::Finished);
        drop(tx);
        let mut certified = Vec::new();
        while let Some(synced) = rx.recv().await {
            assert_eq!(synced.file(), file.record());
            certified.extend_from_slice(synced.ranges());
        }
        // Checkpoints cover the interval once, in order, without overlap.
        assert_eq!(certified, vec![4..8, 8..10]);
        let path = crate::fs::staging_path(tmp.path(), &file.record().name);
        assert_eq!(std::fs::read(path).unwrap(), b"\0\0\0\0abcdef");
    }

    #[tokio::test]
    async fn excess_and_missing_bytes_are_errors() {
        let (_tmp, _root, file) = staging();
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let mut writer = IntervalWriter::new(file.clone(), 0, Some(4));
        let err = writer
            .stream(&mut Chunks::of(&[b"abc", b"de"]), &cancel, &tx, 100)
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Overflow { end: 4 }));
        // The refused chunk was not written.
        assert_eq!(writer.position(), 3);

        let mut writer = IntervalWriter::new(file, 0, Some(4));
        let err = writer
            .stream(&mut Chunks::of(&[b"abc"]), &cancel, &tx, 100)
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Truncated { at: 3, end: 4 }));
    }

    /// Waits until `count` reaches `n`.
    async fn reaches(count: &AtomicUsize, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while count.load(Ordering::SeqCst) < n {
            assert!(tokio::time::Instant::now() < deadline, "writer stalled");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    #[expect(clippy::single_range_in_vec_init, reason = "a list of one byte range")]
    async fn cancellation_stops_a_stalled_body() {
        let (_tmp, _root, file) = staging();
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let mut writer = IntervalWriter::new(file, 0, None);
        let pulled = Arc::new(AtomicUsize::new(0));
        let mut body = Stalls(Chunks::of(&[b"abc"]), pulled.clone());
        let task = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let end = writer.stream(&mut body, &cancel, &tx, 100).await;
                (end, writer)
            })
        };
        // The first chunk is written and the writer waits on the stalled body.
        reaches(&pulled, 2).await;
        cancel.cancel();
        let (end, mut writer) = task.await.unwrap();
        assert_eq!(end.unwrap(), StreamEnd::Cancelled);
        // Nothing was certified on the way out; a pause may still do so.
        assert!(rx.try_recv().is_err());
        let synced = writer.checkpoint().await.unwrap().unwrap();
        assert_eq!(synced.ranges(), &[0..3]);
        assert!(writer.checkpoint().await.unwrap().is_none());
    }

    /// Counts how many chunks were pulled.
    struct Counted(Chunks, Arc<AtomicUsize>);

    impl ChunkSource for Counted {
        async fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.next_chunk().await
        }
    }

    #[tokio::test]
    async fn a_full_checkpoint_channel_holds_the_transfer_back() {
        let (_tmp, _root, file) = staging();
        let (tx, mut rx) = mpsc::channel(1);
        let pulled = Arc::new(AtomicUsize::new(0));
        let mut body = Counted(Chunks::of(&[b"a", b"b", b"c"]), pulled.clone());
        let mut writer = IntervalWriter::new(file, 0, None);
        let task = tokio::spawn(async move {
            writer
                .stream(&mut body, &CancellationToken::new(), &tx, 1)
                .await
        });
        // The first certificate fills the channel; the second waits for
        // room, so the third chunk is not read.
        reaches(&pulled, 2).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pulled.load(Ordering::SeqCst), 2);

        let mut certified = Vec::new();
        while let Some(synced) = rx.recv().await {
            certified.extend_from_slice(synced.ranges());
        }
        assert_eq!(task.await.unwrap().unwrap(), StreamEnd::Finished);
        assert_eq!(certified, vec![0..1, 1..2, 2..3]);
    }

    #[tokio::test]
    async fn source_errors_are_not_progress() {
        let (_tmp, _root, file) = staging();
        let (tx, mut rx) = mpsc::channel(8);
        let mut writer = IntervalWriter::new(file, 0, None);
        let mut body = Chunks(VecDeque::from([
            Ok(Bytes::from_static(b"ab")),
            Err(io::ErrorKind::ConnectionReset.into()),
        ]));
        let err = writer
            .stream(&mut body, &CancellationToken::new(), &tx, 100)
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Source(_)));
        assert!(rx.try_recv().is_err());
    }
}
