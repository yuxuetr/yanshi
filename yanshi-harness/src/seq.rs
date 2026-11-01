//! Monotonic event sequencing — [`Seq`] + [`SeqAllocator`] (P-A3.4).
//!
//! Every [`HarnessEvent`] carries a `seq`: a
//! per-session monotonic ordinal that lets a sink (JSONL replay, the SSE
//! broker, the Postgres event log) reconstruct the exact order events were
//! produced. The wire field stays a plain `u64` so the frozen beta contract is
//! unchanged; [`Seq`] is the in-process newtype that keeps a raw counter value
//! from being confused with the many other `u64`s in this crate.
//!
//! ## The seq-vs-write race this fixes
//!
//! Before P-A3.4 every emit site did the same three steps independently:
//!
//! ```text
//! let seq = counter.fetch_add(1, SeqCst);   // 1. allocate
//! let event = HarnessEvent { seq, .. };      // 2. build
//! sinks.dispatch(&event).await;              // 3. write (awaits I/O)
//! ```
//!
//! `fetch_add` makes the *numbers* monotonic, but step 3 is an `.await`. When
//! two emitters run concurrently — parallel tool calls through the hook layer,
//! the live `AgentEvent` bridge racing those hooks, or several background tasks
//! — emitter A can win step 1 (`seq = N`) yet lose step 3, so the sink observes
//! `seq = N+1` on the wire *before* `seq = N`. [`SinkChain::dispatch`] does not
//! serialize across callers, so the out-of-order write reaches every sink.
//!
//! [`SeqAllocator::stamp`] closes the window by holding an emit lock across all
//! three steps, so the order events reach the sink is exactly their seq order.
//! The lock is shared by [`Clone`], so every writer that shares one allocator
//! (the runtime, its hook config, its background tasks) is serialized together.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};

use crate::error::HarnessError;
use crate::event::{HarnessEvent, HarnessEventBody};
use crate::persistence::SinkChain;

/// A per-session monotonic event ordinal.
///
/// A transparent newtype over `u64`: it serializes and compares exactly like
/// the raw number (so it crosses the [`HarnessEvent`] wire boundary as a plain
/// integer), but in Rust code it cannot be silently swapped with a step index,
/// a token count, or any other `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
  /// The underlying ordinal, for the wire `HarnessEvent.seq` field.
  pub fn get(self) -> u64 {
    self.0
  }
}

impl From<u64> for Seq {
  fn from(value: u64) -> Self {
    Seq(value)
  }
}

impl From<Seq> for u64 {
  fn from(seq: Seq) -> Self {
    seq.0
  }
}

impl std::fmt::Display for Seq {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.0)
  }
}

/// Allocates monotonic [`Seq`] values and emits events under an emit lock so
/// that wire order matches seq order even under concurrent emitters.
///
/// Cloning shares both the counter and the lock, so all writers that should
/// share one monotonic series (the runtime's bracket/bridge events, the hook
/// layer's approval/tool events, background-task events) must be wired from
/// clones of a single allocator — see
/// [`HarnessRuntime::seq_allocator`](crate::runtime::HarnessRuntime::seq_allocator).
#[derive(Clone, Debug)]
pub struct SeqAllocator {
  next: Arc<AtomicU64>,
  /// Serializes the (allocate-seq, build, dispatch) critical section. An async
  /// mutex because [`SinkChain::dispatch`] is `.await`ed while it is held.
  emit_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for SeqAllocator {
  fn default() -> Self {
    Self::new()
  }
}

impl SeqAllocator {
  /// A fresh allocator whose first allocated seq is `0`.
  pub fn new() -> Self {
    Self::with_initial(0)
  }

  /// An allocator whose first allocated seq is `initial` — used when resuming
  /// a session in `append` mode so the new events continue the prior series.
  pub fn with_initial(initial: u64) -> Self {
    Self {
      next: Arc::new(AtomicU64::new(initial)),
      emit_lock: Arc::new(tokio::sync::Mutex::new(())),
    }
  }

  /// Wrap an existing shared counter with a fresh emit lock.
  ///
  /// Backward-compatibility bridge for the legacy `with_seq_counter`
  /// (`Arc<AtomicU64>`) API: callers that only share the raw counter still get
  /// the race fixed *among writers that share this one allocator*. For the full
  /// cross-writer guarantee, share a single [`SeqAllocator`] (which carries the
  /// lock) via the `with_seq_allocator` setters instead.
  pub fn from_counter(counter: Arc<AtomicU64>) -> Self {
    Self {
      next: counter,
      emit_lock: Arc::new(tokio::sync::Mutex::new(())),
    }
  }

  /// The shared raw counter, for the legacy `seq_counter()` accessor and for
  /// sequential, non-dispatching builders (e.g. post-loop event translation,
  /// which allocates and is dispatched in the same order, so it cannot race).
  pub fn counter(&self) -> Arc<AtomicU64> {
    self.next.clone()
  }

  /// Allocate the next seq without locking or dispatching.
  ///
  /// Only for sequential builders that allocate and emit in the same order
  /// (where step 3 cannot reorder relative to step 1). Concurrent dispatch
  /// sites must use [`stamp`](Self::stamp) / [`stamp_lossy`](Self::stamp_lossy).
  pub fn next_raw(&self) -> Seq {
    Seq(self.next.fetch_add(1, Ordering::SeqCst))
  }

  /// Reset the next seq to `initial`. Used by `with_initial_seq` before any
  /// event has been emitted.
  pub fn reset_to(&self, initial: u64) {
    self.next.store(initial, Ordering::SeqCst);
  }

  /// Allocate, build, and dispatch an event as one atomic critical section,
  /// returning the seq that was stamped onto it. Wire order is guaranteed to
  /// match seq order for every writer sharing this allocator.
  pub async fn stamp(
    &self,
    sinks: &SinkChain,
    session_id: &str,
    ts: DateTime<Utc>,
    body: HarnessEventBody,
  ) -> Result<Seq, HarnessError> {
    let _guard = self.emit_lock.lock().await;
    let seq = Seq(self.next.fetch_add(1, Ordering::SeqCst));
    let event = HarnessEvent {
      seq: seq.0,
      session_id: session_id.to_owned(),
      ts,
      body,
    };
    sinks.dispatch(&event).await?;
    Ok(seq)
  }

  /// Like [`stamp`](Self::stamp) but swallows sink errors — for paths where
  /// observability must never break execution (the live agent-event bridge,
  /// background-task lifecycle events). The seq is still consumed so the series
  /// stays gap-free and ordered.
  pub async fn stamp_lossy(
    &self,
    sinks: &SinkChain,
    session_id: &str,
    ts: DateTime<Utc>,
    body: HarnessEventBody,
  ) -> Seq {
    let _guard = self.emit_lock.lock().await;
    let seq = Seq(self.next.fetch_add(1, Ordering::SeqCst));
    let event = HarnessEvent {
      seq: seq.0,
      session_id: session_id.to_owned(),
      ts,
      body,
    };
    let _ = sinks.dispatch(&event).await;
    seq
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::{HarnessEventBody, StoppedPayload};
  use crate::persistence::{HarnessEventSink, SinkChain};
  use async_trait::async_trait;
  use std::sync::Mutex;

  fn stopped_body() -> HarnessEventBody {
    HarnessEventBody::Stopped(StoppedPayload {
      reason: crate::event::StopReason::Completed,
      final_answer: None,
      error: None,
    })
  }

  /// Records the order events arrive at the sink, after a write delay that is
  /// *longer for lower seqs*. Without the emit lock this inverts the order;
  /// with it, arrival order must still equal seq order.
  struct OrderRecordingSink {
    arrivals: Arc<Mutex<Vec<u64>>>,
  }

  #[async_trait]
  impl HarnessEventSink for OrderRecordingSink {
    async fn write(&self, event: &HarnessEvent) -> Result<(), HarnessError> {
      // Lower seqs sleep longer: if (allocate, dispatch) were not serialized a
      // late-allocated event would overtake an early one and land first.
      let delay = 20u64.saturating_sub(event.seq.min(20));
      tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
      self
        .arrivals
        .lock()
        .map_err(|_| HarnessError::Other("poisoned".into()))?
        .push(event.seq);
      Ok(())
    }
    fn name(&self) -> &str {
      "order-recording"
    }
  }

  #[test]
  fn seq_newtype_round_trips_and_orders() {
    let a = Seq::from(1u64);
    let b = Seq::from(2u64);
    assert!(a < b);
    assert_eq!(a.get(), 1);
    assert_eq!(u64::from(b), 2);
    assert_eq!(a.to_string(), "1");
  }

  #[tokio::test]
  async fn next_raw_is_monotonic_and_shared_across_clones() {
    let alloc = SeqAllocator::with_initial(5);
    let clone = alloc.clone();
    assert_eq!(alloc.next_raw().get(), 5);
    // The clone shares the counter, so it continues the series.
    assert_eq!(clone.next_raw().get(), 6);
    assert_eq!(alloc.next_raw().get(), 7);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn stamp_keeps_wire_order_equal_to_seq_order_under_concurrency() {
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let sinks = SinkChain::new().push(Arc::new(OrderRecordingSink {
      arrivals: arrivals.clone(),
    }));
    let alloc = SeqAllocator::new();

    // Fire many emitters concurrently. Each clones the shared allocator, so the
    // emit lock serializes the (allocate, dispatch) pair across all of them.
    let mut handles = Vec::new();
    for _ in 0..16 {
      let alloc = alloc.clone();
      let sinks = sinks.clone();
      handles.push(tokio::spawn(async move {
        alloc.stamp(&sinks, "s", Utc::now(), stopped_body()).await
      }));
    }
    for h in handles {
      h.await.expect("join").expect("stamp");
    }

    let recorded = arrivals.lock().expect("lock").clone();
    assert_eq!(recorded.len(), 16);
    // The decisive assertion: arrival order is strictly increasing. The
    // inverted per-seq delay would scramble this if stamp did not hold the lock
    // across dispatch.
    let mut sorted = recorded.clone();
    sorted.sort_unstable();
    assert_eq!(
      recorded, sorted,
      "events must reach the sink in seq order, not allocation-then-race order"
    );
  }
}
