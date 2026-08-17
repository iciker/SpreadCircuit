use anyhow::Result;
use rusqlite::{params, Connection};

use crate::types::market::PriceRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub pair: String,
    pub stage: String,
    pub direction: String,
    pub evm_tx_hash: Option<String>,
    pub liquid_oid: Option<u64>,
    pub reason: String,
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS prices (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            pair          TEXT NOT NULL,
            evm_buy_price REAL,
            evm_sell_price REAL,
            liquid_ask    REAL,
            liquid_bid    REAL,
            sell_diff     REAL,
            buy_diff      REAL,
            created_at    INTEGER DEFAULT (strftime('%s','now'))
        );
        CREATE INDEX IF NOT EXISTS idx_prices_pair_created_at
            ON prices(pair, created_at);
        CREATE TABLE IF NOT EXISTS trade_recovery (
            pair          TEXT PRIMARY KEY,
            stage         TEXT NOT NULL,
            direction     TEXT NOT NULL,
            evm_tx_hash   TEXT,
            liquid_oid    INTEGER,
            reason        TEXT NOT NULL,
            updated_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
    ",
    )?;
    Ok(())
}

pub fn prune_prices(conn: &Connection, max_rows: usize) -> Result<usize> {
    anyhow::ensure!(max_rows > 0, "max_rows 必须大于 0");
    let count: usize = conn.query_row("SELECT COUNT(*) FROM prices", [], |row| row.get(0))?;
    let excess = count.saturating_sub(max_rows);
    if excess == 0 {
        return Ok(0);
    }
    Ok(conn.execute(
        "DELETE FROM prices WHERE id IN (SELECT id FROM prices ORDER BY id ASC LIMIT ?1)",
        params![excess],
    )?)
}

pub fn upsert_recovery(conn: &Connection, record: &RecoveryRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO trade_recovery (pair, stage, direction, evm_tx_hash, liquid_oid, reason, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'))
         ON CONFLICT(pair) DO UPDATE SET
           stage=excluded.stage,
           direction=excluded.direction,
           evm_tx_hash=excluded.evm_tx_hash,
           liquid_oid=excluded.liquid_oid,
           reason=excluded.reason,
           updated_at=excluded.updated_at",
        params![
            record.pair,
            record.stage,
            record.direction,
            record.evm_tx_hash,
            record.liquid_oid,
            record.reason,
        ],
    )?;
    Ok(())
}

pub fn clear_recovery(conn: &Connection, pair: &str) -> Result<()> {
    conn.execute("DELETE FROM trade_recovery WHERE pair = ?1", params![pair])?;
    Ok(())
}

pub fn list_recoveries(conn: &Connection) -> Result<Vec<RecoveryRecord>> {
    let mut statement = conn.prepare(
        "SELECT pair, stage, direction, evm_tx_hash, liquid_oid, reason
         FROM trade_recovery ORDER BY pair",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(RecoveryRecord {
            pair: row.get(0)?,
            stage: row.get(1)?,
            direction: row.get(2)?,
            evm_tx_hash: row.get(3)?,
            liquid_oid: row.get(4)?,
            reason: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn open_initialized(path: &str) -> Result<Connection> {
    let connection = Connection::open(path)?;
    init(&connection)?;
    Ok(connection)
}

pub fn upsert_recovery_at(path: &str, record: &RecoveryRecord) -> Result<()> {
    upsert_recovery(&open_initialized(path)?, record)
}

pub fn clear_recovery_at(path: &str, pair: &str) -> Result<()> {
    clear_recovery(&open_initialized(path)?, pair)
}

pub fn list_recoveries_at(path: &str) -> Result<Vec<RecoveryRecord>> {
    list_recoveries(&open_initialized(path)?)
}

pub fn ensure_startup_safe(dry_run: bool, unresolved: &[RecoveryRecord]) -> Result<()> {
    anyhow::ensure!(
        dry_run || unresolved.is_empty(),
        "检测到 {} 条未解决交易恢复记录，拒绝启动实盘: {:?}",
        unresolved.len(),
        unresolved
    );
    Ok(())
}

pub fn insert_price(conn: &Connection, record: &PriceRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO prices (pair, evm_buy_price, evm_sell_price, liquid_ask, liquid_bid, sell_diff, buy_diff)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.pair,
            record.evm_buy_price, record.evm_sell_price,
            record.liquid_ask, record.liquid_bid,
            record.sell_diff, record.buy_diff,
        ],
    )
    .map(drop)
    .map_err(Into::into)
}
