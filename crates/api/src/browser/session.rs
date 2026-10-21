//! Owner sessions: random tokens kept in memory only, as digests.
//!
//! A session ends when idle for [`IDLE`], at [`LIFETIME`], on sign-out, or
//! on restart, and the oldest is replaced beyond [`MAX_SESSIONS`]. Event
//! streams hold the session's [`SessionEnd`], so a stream never outlives its
//! session.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::secret;

/// A session unused this long ends.
pub(super) const IDLE: Duration = Duration::from_secs(12 * 60 * 60);
/// A session this old ends however recently it was used.
pub(super) const LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Sessions kept at once.
pub(super) const MAX_SESSIONS: usize = 32;

/// A new session's secrets, which only its browser gets.
#[derive(Debug)]
pub(super) struct Issued {
    /// The cookie value.
    pub token: String,
    /// The value each state-changing request repeats in a header.
    pub csrf: String,
}

#[derive(Debug)]
pub(super) struct Session {
    pub csrf: String,
    created: Instant,
    last_used: Instant,
    /// Cancelled when the session ends.
    ended: CancellationToken,
}

impl Session {
    fn live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) < LIFETIME
            && now.saturating_duration_since(self.last_used) < IDLE
    }

    /// When something opened with the session at `now` must stop.
    pub(super) fn end(&self, now: Instant) -> SessionEnd {
        SessionEnd {
            ended: self.ended.clone(),
            deadline: (self.created + LIFETIME).min(now + IDLE),
        }
    }
}

/// When a session ends: on revocation, expiry, or replacement, and at the
/// latest at a deadline by which it would expire if nothing else used it.
/// A browser's event stream reconnects after its deadline, which counts as
/// use, so a session in use keeps its streams.
#[derive(Debug, Clone)]
pub(crate) struct SessionEnd {
    ended: CancellationToken,
    deadline: Instant,
}

impl SessionEnd {
    /// Completes when the session ends.
    pub(crate) async fn ended(self) {
        tokio::select! {
            () = self.ended.cancelled() => {}
            () = tokio::time::sleep_until(self.deadline.into()) => {}
        }
    }
}

/// The live sessions, by token digest.
#[derive(Debug, Default)]
pub(super) struct Sessions {
    by_digest: HashMap<[u8; 32], Session>,
}

impl Sessions {
    /// Starts a session at `now`, ending the least recently used one if
    /// [`MAX_SESSIONS`] are live.
    pub(super) fn create(&mut self, now: Instant) -> io::Result<Issued> {
        let issued = Issued {
            token: secret::generate()?,
            csrf: secret::generate()?,
        };
        self.by_digest.retain(|_, session| {
            let live = session.live(now);
            if !live {
                session.ended.cancel();
            }
            live
        });
        while self.by_digest.len() >= MAX_SESSIONS {
            let oldest = self
                .by_digest
                .iter()
                .min_by_key(|(_, session)| session.last_used)
                .map(|(digest, _)| *digest)
                .expect("the map is full, so not empty");
            self.remove(&oldest);
        }
        self.by_digest.insert(
            secret::digest(&issued.token),
            Session {
                csrf: issued.csrf.clone(),
                created: now,
                last_used: now,
                ended: CancellationToken::new(),
            },
        );
        Ok(issued)
    }

    /// The live session of `token`, marked used at `now`. An expired one
    /// is removed.
    pub(super) fn get(&mut self, token: &str, now: Instant) -> Option<&Session> {
        let digest = secret::digest(token);
        if !self.by_digest.get(&digest)?.live(now) {
            self.remove(&digest);
            return None;
        }
        let session = self.by_digest.get_mut(&digest)?;
        session.last_used = now;
        Some(session)
    }

    /// Ends the session of `token`, if any.
    pub(super) fn revoke(&mut self, token: &str) {
        self.remove(&secret::digest(token));
    }

    /// Removes a session and ends everything opened with it.
    fn remove(&mut self, digest: &[u8; 32]) {
        if let Some(session) = self.by_digest.remove(digest) {
            session.ended.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_end_when_idle_old_or_revoked() {
        let start = Instant::now();
        let mut sessions = Sessions::default();
        let a = sessions.create(start).unwrap();
        assert_eq!(a.token.len(), 64);
        assert_ne!(a.token, a.csrf);
        assert_eq!(sessions.get(&a.token, start).unwrap().csrf, a.csrf);
        assert!(sessions.get(&a.csrf, start).is_none());

        // Use keeps it alive until its lifetime ends.
        let mut now = start;
        while now + IDLE / 2 < start + LIFETIME {
            now += IDLE / 2;
            assert!(sessions.get(&a.token, now).is_some());
        }
        assert!(sessions.get(&a.token, start + LIFETIME).is_none());
        assert!(
            sessions.by_digest.is_empty(),
            "an expired session is removed"
        );

        let b = sessions.create(start).unwrap();
        assert!(sessions.get(&b.token, start + IDLE).is_none());

        let c = sessions.create(start).unwrap();
        sessions.revoke(&c.token);
        assert!(sessions.get(&c.token, start).is_none());
    }

    /// Whether `end` has been signalled, short of its deadline.
    fn signalled(end: &SessionEnd) -> bool {
        end.ended.is_cancelled()
    }

    #[test]
    fn whatever_ends_a_session_ends_its_streams() {
        let start = Instant::now();
        let mut sessions = Sessions::default();

        let revoked = sessions.create(start).unwrap();
        let end = sessions.get(&revoked.token, start).unwrap().end(start);
        assert!(!signalled(&end));
        sessions.revoke(&revoked.token);
        assert!(signalled(&end));

        let expired = sessions.create(start).unwrap();
        let end = sessions.get(&expired.token, start).unwrap().end(start);
        assert!(sessions.get(&expired.token, start + IDLE).is_none());
        assert!(signalled(&end));

        // Expired sessions swept out by a new one end too.
        let swept = sessions.create(start).unwrap();
        let end = sessions.get(&swept.token, start).unwrap().end(start);
        sessions.create(start + IDLE).unwrap();
        assert!(signalled(&end));

        // So does the least recently used one a new session replaces.
        let mut sessions = Sessions::default();
        let first = sessions.create(start).unwrap();
        let end = sessions.get(&first.token, start).unwrap().end(start);
        for _ in 1..MAX_SESSIONS {
            sessions.create(start + Duration::from_secs(1)).unwrap();
        }
        assert!(!signalled(&end));
        sessions.create(start + Duration::from_secs(2)).unwrap();
        assert!(signalled(&end));
    }

    #[test]
    fn a_stream_ends_by_when_its_session_would_expire_unused() {
        let start = Instant::now();
        let mut sessions = Sessions::default();
        let issued = sessions.create(start).unwrap();
        let session = sessions.get(&issued.token, start).unwrap();
        assert_eq!(session.end(start).deadline, start + IDLE);
        let late = start + LIFETIME - IDLE / 2;
        assert_eq!(session.end(late).deadline, start + LIFETIME);
    }

    #[tokio::test]
    async fn the_end_completes_on_revocation_or_at_its_deadline() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        let issued = sessions.create(now).unwrap();
        let end = sessions.get(&issued.token, now).unwrap().end(now);
        let waiting = tokio::spawn(end.ended());
        sessions.revoke(&issued.token);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();

        let past = SessionEnd {
            ended: CancellationToken::new(),
            deadline: Instant::now(),
        };
        tokio::time::timeout(Duration::from_secs(5), past.ended())
            .await
            .unwrap();
    }

    #[test]
    fn storage_is_bounded_by_ending_the_least_recently_used() {
        let start = Instant::now();
        let mut sessions = Sessions::default();
        let issued = (0..MAX_SESSIONS)
            .map(|i| {
                sessions
                    .create(start + Duration::from_secs(i as u64))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let later = start + Duration::from_secs(MAX_SESSIONS as u64);
        // The first is used again, so the second is now the oldest.
        assert!(sessions.get(&issued[0].token, later).is_some());
        let newest = sessions.create(later).unwrap();
        assert_eq!(sessions.by_digest.len(), MAX_SESSIONS);
        assert!(sessions.get(&issued[1].token, later).is_none());
        assert!(sessions.get(&issued[0].token, later).is_some());
        assert!(sessions.get(&newest.token, later).is_some());
    }
}
