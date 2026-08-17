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
    // busy_timeout：PriceDb 长连接与各 ArbEngine 恢复连接并发写同一库，
    // WAL 下写者互斥；默认 0 会立即 SQLITE_BUSY，把瞬时锁冲突误升级为 RecoveryRequired
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")?;
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
    // id 单调递增：用 O(1) 水位线删除旧行，避免 COUNT(*) 全表扫描持有写锁。
    // 历史裁剪留下的 id 空洞只会让保留行数 ≤ max_rows，方向安全。
    Ok(conn.execute(
        "DELETE FROM prices WHERE id <= (SELECT COALESCE(MAX(id), 0) FROM prices) - ?1",
        params![max_rows as i64],
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

/// 打开并初始化连接（WAL + busy_timeout + 建表）。ArbEngine 启动时调用一次并长期持有，
/// 避免每次恢复持久化都重开连接、重跑 DDL。
pub fn open_initialized(path: &str) -> Result<Connection> {
    let connection = Connection::open(path)?;
    init(&connection)?;
    Ok(connection)
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
