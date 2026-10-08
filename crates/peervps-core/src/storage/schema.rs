//! Versioned schema migrations, tracked with `PRAGMA user_version`.

use rusqlite::Connection;

use crate::Result;

/// Each entry upgrades the schema by one version. Never edit a shipped entry;
/// append a new one.
const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE accounts (
        id          TEXT PRIMARY KEY,
        kind        TEXT NOT NULL CHECK (kind IN ('renter', 'provider', 'system')),
        balance     INTEGER NOT NULL DEFAULT 0 CHECK (balance >= 0),
        created_at  INTEGER NOT NULL
    );

    -- Append-only journal; every balance change has exactly one row here.
    CREATE TABLE ledger (
        id             INTEGER PRIMARY KEY,
        account        TEXT NOT NULL REFERENCES accounts(id),
        delta          INTEGER NOT NULL,
        balance_after  INTEGER NOT NULL,
        kind           TEXT NOT NULL,
        reference      TEXT,
        at             INTEGER NOT NULL
    );
    CREATE INDEX ledger_account_at ON ledger(account, at);

    CREATE TABLE collateral (
        provider    TEXT PRIMARY KEY REFERENCES accounts(id),
        locked      INTEGER NOT NULL DEFAULT 0 CHECK (locked >= 0),
        updated_at  INTEGER NOT NULL
    );

    CREATE TABLE pools (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL,
        min_total   INTEGER NOT NULL CHECK (min_total >= 0),
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE pool_members (
        pool      TEXT NOT NULL REFERENCES pools(id),
        provider  TEXT NOT NULL REFERENCES accounts(id),
        stake     INTEGER NOT NULL CHECK (stake >= 0),
        PRIMARY KEY (pool, provider)
    );

    CREATE TABLE presence (
        id     INTEGER PRIMARY KEY,
        node   TEXT NOT NULL,
        event  TEXT NOT NULL,
        at     INTEGER NOT NULL
    );
    CREATE INDEX presence_node_at ON presence(node, at);

    CREATE TABLE instances (
        id              TEXT PRIMARY KEY,
        renter          TEXT NOT NULL REFERENCES accounts(id),
        provider        TEXT NOT NULL REFERENCES accounts(id),
        rate_per_sec    INTEGER NOT NULL CHECK (rate_per_sec >= 0),
        state           TEXT NOT NULL,
        started_at      INTEGER NOT NULL,
        stopped_at      INTEGER,
        billed_seconds  INTEGER NOT NULL DEFAULT 0
    );

    -- Idempotency for payment webhooks.
    CREATE TABLE webhook_events (
        id           TEXT PRIMARY KEY,
        gateway      TEXT NOT NULL,
        received_at  INTEGER NOT NULL
    );
    "#,
];

pub fn migrate(conn: &Connection) -> Result<()> {
    let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
