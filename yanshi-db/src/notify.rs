//! Postgres LISTEN/NOTIFY primitive (W4.2b).
//!
//! `yanshi-server` needs a way for any gateway replica's write to
//! become visible to every other replica's in-memory state (SSE
//! subscribers, parked approval/cancellation intents) without
//! introducing a second piece of infra. Postgres already ships this via
//! `LISTEN`/`NOTIFY`, and `sqlx` (already a dependency) exposes it as
//! [`sqlx::postgres::PgListener`] — no new dependency, no new service to
//! run in `docker-compose.yml` or a Helm subchart. See `TODOs.md` W4.2
//! for the fuller design rationale (Postgres NOTIFY chosen over
//! Redis/NATS).
//!
//! Callers should keep `payload` small — Postgres caps a `NOTIFY`
//! payload at ~8000 bytes. Send identifiers (e.g. `"{tenant_id}:
//! {run_id}:{seq}"`), never full event bodies; the receiving replica
//! re-fetches details from the DB, mirroring how the SSE backfill path
//! already works.

use sqlx::PgPool;
use sqlx::postgres::PgListener;

use crate::error::DbError;

/// Publish a notification on `channel`.
pub async fn notify(pool: &PgPool, channel: &str, payload: &str) -> Result<(), DbError> {
  sqlx::query("SELECT pg_notify($1, $2)")
    .bind(channel)
    .bind(payload)
    .execute(pool)
    .await?;
  Ok(())
}

/// Subscribes to one or more channels and yields `(channel, payload)`
/// pairs as they arrive. A thin wrapper around [`PgListener`] so callers
/// have a single `recv`-style loop point.
pub struct NotifyListener {
  inner: PgListener,
}

impl NotifyListener {
  /// Connect a dedicated listener and subscribe to every channel in
  /// `channels`. Uses its own connection (not drawn from `pool`'s
  /// regular acquire/release cycle) since a `LISTEN` session must stay
  /// open for the listener's lifetime.
  pub async fn connect(pool: &PgPool, channels: &[&str]) -> Result<Self, DbError> {
    let mut inner = PgListener::connect_with(pool).await?;
    for channel in channels {
      inner.listen(channel).await?;
    }
    Ok(Self { inner })
  }

  /// Wait for the next notification, or report that the connection
  /// dropped. After a [`Notice::Gap`] the next call reconnects and
  /// re-subscribes; anything sent in between is gone, so the caller must
  /// resync its state from the database.
  pub async fn recv(&mut self) -> Result<Notice, DbError> {
    Ok(match self.inner.try_recv().await? {
      Some(notification) => Notice::Message {
        channel: notification.channel().to_string(),
        payload: notification.payload().to_string(),
      },
      None => Notice::Gap,
    })
  }
}

/// What [`NotifyListener::recv`] yields. NOTIFY is at-most-once, so a
/// consumer treats the DB as the source of truth and a `Gap` as "resync".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
  Message {
    channel: String,
    payload: String,
  },
  /// The LISTEN connection dropped; notifications may have been missed.
  Gap,
}

#[cfg(test)]
mod tests {
  use super::*;

  fn live_url() -> Option<String> {
    std::env::var("YANSHI_DATABASE_TEST_URL").ok()
  }

  /// W4.2b: two independent listeners subscribed to the same channel
  /// must both observe one `notify()` call — proves the primitive
  /// actually fans out to multiple "replicas" (each listener stands in
  /// for one gateway process), which every later W4.2 phase depends on.
  #[tokio::test]
  async fn two_listeners_both_receive_a_published_notification() {
    let Some(url) = live_url() else {
      eprintln!("skipping two_listeners_both_receive_a_published_notification: no DB url");
      return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
      .max_connections(4)
      .connect(&url)
      .await
      .expect("connect");

    let channel = format!("yanshi_test_{}", uuid::Uuid::new_v4().simple());
    let mut listener_a = NotifyListener::connect(&pool, &[&channel])
      .await
      .expect("listener a");
    let mut listener_b = NotifyListener::connect(&pool, &[&channel])
      .await
      .expect("listener b");

    notify(&pool, &channel, "hello").await.expect("notify");

    let expected = Notice::Message {
      channel: channel.clone(),
      payload: "hello".to_string(),
    };
    for listener in [&mut listener_a, &mut listener_b] {
      let notice = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
        .await
        .expect("listener timed out")
        .expect("listener recv");
      assert_eq!(notice, expected);
    }
  }

  /// A listener subscribed to a *different* channel must not see a
  /// notification meant for another channel.
  #[tokio::test]
  async fn listener_does_not_receive_notifications_for_other_channels() {
    let Some(url) = live_url() else {
      eprintln!("skipping listener_does_not_receive_notifications_for_other_channels: no DB url");
      return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
      .max_connections(4)
      .connect(&url)
      .await
      .expect("connect");

    let channel_a = format!("yanshi_test_{}", uuid::Uuid::new_v4().simple());
    let channel_b = format!("yanshi_test_{}", uuid::Uuid::new_v4().simple());
    let mut listener = NotifyListener::connect(&pool, &[&channel_a])
      .await
      .expect("listener");

    notify(&pool, &channel_b, "irrelevant")
      .await
      .expect("notify other channel");
    notify(&pool, &channel_a, "expected")
      .await
      .expect("notify subscribed channel");

    let notice = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
      .await
      .expect("listener timed out")
      .expect("listener recv");
    assert_eq!(
      notice,
      Notice::Message {
        channel: channel_a,
        payload: "expected".to_string(),
      }
    );
  }

  /// Eval 2026-10-07 P3: a dropped LISTEN connection must surface as a
  /// gap. `PgListener::recv` reconnects silently and loses whatever was
  /// sent in between, so callers could never resync.
  #[tokio::test]
  async fn dropped_connection_surfaces_as_a_gap() {
    let Some(url) = live_url() else {
      eprintln!("skipping dropped_connection_surfaces_as_a_gap: no DB url");
      return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
      .max_connections(2)
      .connect(&url)
      .await
      .expect("connect");
    let channel = format!("yanshi_test_{}", uuid::Uuid::new_v4().simple());
    let mut listener = NotifyListener::connect(&pool, &[&channel])
      .await
      .expect("listener");

    let killed: i64 = sqlx::query_scalar(
      "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE query = $1",
    )
    .bind(format!("LISTEN \"{channel}\""))
    .fetch_one(&pool)
    .await
    .expect("terminate listener backend");
    assert_eq!(killed, 1, "exactly the listener's backend");

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
      .await
      .expect("a gap must be reported, not swallowed")
      .expect("recv");
    assert!(matches!(first, Notice::Gap), "got {first:?}");

    // The next call reconnects and re-subscribes.
    notify(&pool, &channel, "after").await.expect("notify");
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
      .await
      .expect("listener timed out after reconnect")
      .expect("recv");
    assert!(
      matches!(&next, Notice::Message { payload, .. } if payload == "after"),
      "got {next:?}"
    );
  }
}
