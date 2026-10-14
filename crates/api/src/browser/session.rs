//! Owner sessions: opaque random tokens held in memory only.
//!
//! The cookie carries a 256-bit random token that means nothing by itself;
//! the server keeps only its SHA-256 digest, with the session's CSRF token
//! and its times. A session ends when it has been idle for [`IDLE`], when
//! it is [`LIFETIME`] old, when its owner signs out, or when the process
//! stops: sessions are never written to disk, so a restart signs every
//! browser out. At most [`MAX_SESSIONS`] exist; a new one beyond that
//! replaces the least recently used.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

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
}

impl Session {
    fn live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) < LIFETIME
            && now.saturating_duration_since(self.last_used) < IDLE
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
        self.by_digest.retain(|_, session| session.live(now));
        while self.by_digest.len() >= MAX_SESSIONS {
            let oldest = self
                .by_digest
                .iter()
                .min_by_key(|(_, session)| session.last_used)
                .map(|(digest, _)| *digest)
                .expect("the map is full, so not empty");
            self.by_digest.remove(&oldest);
        }
        self.by_digest.insert(
            secret::digest(&issued.token),
            Session {
                csrf: issued.csrf.clone(),
                created: now,
                last_used: now,
            },
        );
        Ok(issued)
    }

    /// The live session of `token`, marked used at `now`. An expired one
    /// is removed.
    pub(super) fn get(&mut self, token: &str, now: Instant) -> Option<&Session> {
        let digest = secret::digest(token);
        if !self.by_digest.get(&digest)?.live(now) {
            self.by_digest.remove(&digest);
            return None;
        }
        let session = self.by_digest.get_mut(&digest)?;
        session.last_used = now;
        Some(session)
    }

    /// Ends the session of `token`, if any.
    pub(super) fn revoke(&mut self, token: &str) {
        self.by_digest.remove(&secret::digest(token));
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
