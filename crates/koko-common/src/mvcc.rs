//! In-memory MVCC version-stamp primitives (P3).
//!
//! Per-row version records (`begin_ts`/`end_ts`) replace the O(db) explicit-
//! transaction snapshot clone: an explicit transaction reads at a timestamp and
//! writes new versions at O(changes), instead of cloning the whole store.
//!
//! A version timestamp is one of three things:
//! - a committed commit-time in `0..START_TX_ID`,
//! - an in-flight writer id in `START_TX_ID..TS_INF` (upgraded to a real
//!   commit time at `COMMIT`), or
//! - the [`TS_INF`] sentinel (`end_ts` = "live"; reused as `begin_ts` = "dead").
//!
//! The default engine remains single-writer, but `debug_enable_multi_writes`
//! allows several in-flight writers. A [`ReadView`] therefore carries the current
//! writer id (if any) instead of a boolean "is the only writer" bit.

/// A monotonic version timestamp (a commit time, or a sentinel below).
pub type Ts = u64;

/// The first in-flight writer id. Real commit timestamps stay below this value;
/// writer ids are allocated from this value upward, so an uncommitted version is
/// visible only to the matching writer and never to ordinary committed reads.
pub const START_TX_ID: Ts = 1 << 63;
/// Back-compatibility alias for the first/legacy single-writer uncommitted tag.
pub const UNCOMMITTED: Ts = START_TX_ID;

/// `end_ts` sentinel meaning the row is live (never deleted). Also reused as a
/// `begin_ts` sentinel for a rolled-back insert (dead forever): `TS_INF` is never
/// `<= read_ts` and is not `UNCOMMITTED`, so such a row is invisible to everyone.
pub const TS_INF: Ts = u64::MAX;

/// What a statement is allowed to observe.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadView {
    /// The largest committed timestamp this statement may observe. A committed
    /// version is visible iff its timestamp is `<= read_ts`.
    pub read_ts: Ts,
    /// The current writer's transaction id. `None` means a read-only statement:
    /// it observes only committed versions at/before `read_ts`.
    pub writer_id: Option<Ts>,
}

impl ReadView {
    #[inline]
    pub const fn reader(read_ts: Ts) -> Self {
        Self {
            read_ts,
            writer_id: None,
        }
    }

    #[inline]
    pub const fn writer(read_ts: Ts, writer_id: Ts) -> Self {
        Self {
            read_ts,
            writer_id: Some(writer_id),
        }
    }

    /// A version stamped `ts` is visible iff it is this writer's own uncommitted
    /// work, or a commit that happened at/before `read_ts`.
    #[inline]
    pub fn ts_visible(self, ts: Ts) -> bool {
        if ts == TS_INF {
            false
        } else if self.writer_id == Some(ts) {
            true
        } else {
            ts < START_TX_ID && ts <= self.read_ts
        }
    }

    /// True iff `ts` is an uncommitted version owned by this read view's writer.
    #[inline]
    pub fn owns(self, ts: Ts) -> bool {
        self.writer_id == Some(ts)
    }

    /// True iff writing a row/cell carrying `ts` would conflict with this writer:
    /// the version belongs to another in-flight transaction or committed after
    /// this transaction's snapshot.
    #[inline]
    pub fn conflicts_with_write(self, ts: Ts) -> bool {
        ts != TS_INF && !self.owns(ts) && ts > self.read_ts
    }

    /// A row inserted at `begin` and deleted at `end` (`TS_INF` = not deleted) is
    /// visible iff its insert is visible and its delete is not.
    #[inline]
    pub fn row_visible(self, begin: Ts, end: Ts) -> bool {
        self.ts_visible(begin) && !(end != TS_INF && self.ts_visible(end))
    }
}
