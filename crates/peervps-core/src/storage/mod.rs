//! SQLite persistence (WAL mode).
//!
//! One connection guarded by a mutex is plenty for a single node: all writes
//! are tiny and WAL lets readers proceed during a write. Every call runs on
//! Tokio's blocking pool so the async runtime never stalls on disk I/O.

mod schema;

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::Result;

/// Shared handle to the node database.
#[derive(Debug, Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (or create) the database at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::init(conn)
    }

    /// Private in-memory database, for tests and demos.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL: concurrent readers + one writer, crash-safe without fsync per txn.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Run `f` with exclusive access to the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut guard)
        })
        .await?
    }

    /// Synchronous access for callers already off the async runtime.
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }

    pub async fn journal_mode(&self) -> Result<String> {
        self.call(|c| Ok(c.pragma_query_value(None, "journal_mode", |r| r.get(0))?)).await
    }
}

/// Unix seconds.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Node presence history (for Solution C SLA enforcement).
pub mod presence {
    use rusqlite::{Connection, OptionalExtension, params};
    use serde::{Deserialize, Serialize};

    use crate::Result;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PresenceEvent {
        Online,
        HeartbeatLost,
        /// Node finished a full hibernation hand-off before going away.
        Hibernated,
        /// Node vanished without hibernating.
        OfflineDirty,
    }

    impl PresenceEvent {
        fn as_str(self) -> &'static str {
            match self {
                Self::Online => "online",
                Self::HeartbeatLost => "heartbeat_lost",
                Self::Hibernated => "hibernated",
                Self::OfflineDirty => "offline_dirty",
            }
        }

        fn parse(s: &str) -> Option<Self> {
            Some(match s {
                "online" => Self::Online,
                "heartbeat_lost" => Self::HeartbeatLost,
                "hibernated" => Self::Hibernated,
                "offline_dirty" => Self::OfflineDirty,
                _ => return None,
            })
        }
    }

    pub fn record(conn: &Connection, node: &str, event: PresenceEvent, at: i64) -> Result<()> {
        conn.execute("INSERT INTO presence(node, event, at) VALUES (?1, ?2, ?3)", params![node, event.as_str(), at])?;
        Ok(())
    }

    pub fn last(conn: &Connection, node: &str) -> Result<Option<(PresenceEvent, i64)>> {
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT event, at FROM presence WHERE node = ?1 ORDER BY at DESC, id DESC LIMIT 1",
                [node],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(e, at)| PresenceEvent::parse(&e).map(|e| (e, at))))
    }

    /// Fraction of `[since, until)` the node was online, from its event history.
    pub fn uptime_ratio(conn: &Connection, node: &str, since: i64, until: i64) -> Result<f64> {
        if until <= since {
            return Ok(1.0);
        }
        let before: Option<String> = conn
            .query_row(
                "SELECT event FROM presence WHERE node = ?1 AND at < ?2 ORDER BY at DESC, id DESC LIMIT 1",
                params![node, since],
                |r| r.get(0),
            )
            .optional()?;
        let mut online = before.as_deref() == Some("online");
        let mut cursor = since;
        let mut up = 0i64;
        let mut stmt =
            conn.prepare("SELECT event, at FROM presence WHERE node = ?1 AND at >= ?2 AND at < ?3 ORDER BY at, id")?;
        let rows =
            stmt.query_map(params![node, since, until], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (event, at) = row?;
            if online {
                up += at - cursor;
            }
            cursor = at;
            online = event == "online";
        }
        if online {
            up += until - cursor;
        }
        Ok(up as f64 / (until - since) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::presence::{self, PresenceEvent};
    use super::*;

    #[tokio::test]
    async fn file_db_uses_wal() {
        let dir = std::env::temp_dir().join(format!("peervps-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store = Store::open(dir.join("node.db")).expect("open");
        assert_eq!(store.journal_mode().await.expect("pragma"), "wal");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uptime_ratio_follows_events() {
        let s = Store::in_memory().expect("db");
        s.with(|c| {
            presence::record(c, "n1", PresenceEvent::Online, 0)?;
            presence::record(c, "n1", PresenceEvent::HeartbeatLost, 75)?;
            presence::record(c, "n1", PresenceEvent::Online, 100)?;
            let r = presence::uptime_ratio(c, "n1", 0, 200)?;
            assert!((r - 0.875).abs() < 1e-9, "{r}");
            assert_eq!(presence::last(c, "n1")?, Some((PresenceEvent::Online, 100)));
            Ok(())
        })
        .expect("ok");
    }
}
