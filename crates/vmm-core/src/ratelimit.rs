// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A token bucket for events a guest or a peer can repeat at will.
//!
//! A log line the guest can provoke needs a cap, or the guest drives
//! the host log and evicts every other record from the async drain.
//! One bucket per log site keeps one noisy source from spending the
//! budget of another.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Admits up to `burst` events at once, then one more every `period`.
pub struct TokenBucket {
    burst: u32,
    period: Duration,
    state: Mutex<State>,
}

struct State {
    tokens: u32,
    refilled: Instant,
}

impl TokenBucket {
    pub fn new(burst: u32, period: Duration) -> Self {
        Self {
            burst,
            period,
            state: Mutex::new(State {
                tokens: burst,
                refilled: Instant::now(),
            }),
        }
    }

    /// Take one token, or report that none is left.
    pub fn take(&self) -> bool {
        self.take_at(Instant::now())
    }

    fn take_at(&self, now: Instant) -> bool {
        // The state is two integers a panic cannot leave half written.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.period.is_zero() {
            let elapsed = now.saturating_duration_since(state.refilled);
            let earned = elapsed.as_nanos() / self.period.as_nanos();
            if earned > 0 {
                let earned = u32::try_from(earned).unwrap_or(u32::MAX);
                state.tokens =
                    state.tokens.saturating_add(earned).min(self.burst);
                // Advance by whole periods so the remainder is kept.
                state.refilled += self.period * earned;
            }
        }
        if state.tokens == 0 {
            return false;
        }
        state.tokens -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_admitted_and_then_refused() {
        let bucket = TokenBucket::new(3, Duration::from_secs(60));
        let now = Instant::now();

        assert!(bucket.take_at(now));
        assert!(bucket.take_at(now));
        assert!(bucket.take_at(now));
        assert!(!bucket.take_at(now));
    }

    #[test]
    fn one_token_returns_per_period() {
        let period = Duration::from_secs(10);
        let bucket = TokenBucket::new(1, period);
        let start = Instant::now();

        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + period / 2));
        assert!(bucket.take_at(start + period));
        assert!(!bucket.take_at(start + period));
        // Three periods earn three tokens, but the burst caps them at
        // one.
        assert!(bucket.take_at(start + period * 4));
        assert!(!bucket.take_at(start + period * 4));
    }

    #[test]
    fn the_remainder_of_a_period_is_kept() {
        let period = Duration::from_secs(10);
        let bucket = TokenBucket::new(1, period);
        let start = Instant::now();

        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + Duration::from_secs(7)));
        // 7 s + 3 s is one whole period from the start, not from the
        // last refused take.
        assert!(bucket.take_at(start + Duration::from_secs(10)));
    }

    #[test]
    fn a_zero_period_never_refills() {
        let bucket = TokenBucket::new(1, Duration::ZERO);
        let start = Instant::now();

        assert!(bucket.take_at(start));
        assert!(!bucket.take_at(start + Duration::from_secs(3600)));
    }
}
