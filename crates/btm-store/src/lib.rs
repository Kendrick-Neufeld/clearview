//! History, kept deliberately small.
//!
//! A monitor that quietly grows a database until it is a problem of its own has
//! traded one annoyance for a worse one. Every decision here is made for size:
//!
//! - **Nothing is stored at full fidelity for long.** Three resolutions, each
//!   kept only as long as it is useful: five seconds for the last two hours, one
//!   minute for two days, ten minutes for a month.
//! - **Per-app figures are never stored raw.** They are averaged in memory over a
//!   whole minute before a single row is written, and only the handful of apps
//!   that actually mattered in that minute are written at all.
//! - **Values are scaled integers, not floats.** SQLite stores a small integer in
//!   one or two bytes and every float in eight, so CPU is kept as per-mille and
//!   memory as whole megabytes. Neither loses anything a person could see on a
//!   graph.
//! - **Names are stored once**, in a dictionary table, and referenced by id.
//!
//! The result is a few megabytes covering a month, rather than the hundreds a
//! naive schema would reach.

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Five-second detail, for "what just happened".
pub const RES_FINE: u32 = 5;
/// One-minute detail, for today.
pub const RES_MINUTE: u32 = 60;
/// Ten-minute detail, for the month.
pub const RES_COARSE: u32 = 600;

/// How long each resolution is kept, in seconds.
const KEEP_FINE: i64 = 2 * 3600;
const KEEP_MINUTE: i64 = 48 * 3600;
const KEEP_COARSE: i64 = 31 * 86_400;

/// Apps recorded per minute, by CPU and again by memory. Beyond a dozen or so
/// nobody is reading the history of a process that used 0.1% of a core.
const APPS_PER_BUCKET: usize = 10;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// Which stored quantity a query is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    Cpu,
    Memory,
    Gpu,
    Network,
    Disk,
}

impl Metric {
    /// The per-app column this metric lives in. Fixed strings only — these are
    /// interpolated into SQL, so they must never come from outside.
    fn column(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu_pm",
            Metric::Memory => "mem_mb",
            Metric::Gpu => "gpu_pm",
            Metric::Network => "net_kbps",
            Metric::Disk => "disk_kbps",
        }
    }

    pub fn parse(name: &str) -> Metric {
        match name {
            "memory" => Metric::Memory,
            "gpu" => Metric::Gpu,
            "network" => Metric::Network,
            "disk" => Metric::Disk,
            _ => Metric::Cpu,
        }
    }
}

/// One machine-wide reading. Integers throughout, scaled on the way in.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SystemPoint {
    /// Unix seconds.
    pub t: i64,
    /// CPU busy, per mille of the whole machine (0–1000).
    pub cpu_pm: u16,
    pub mem_mb: u32,
    pub swap_mb: u32,
    /// PSI "some" over ten seconds, per mille.
    pub psi_cpu_pm: u16,
    pub psi_mem_pm: u16,
    pub psi_io_pm: u16,
    /// Integrated and discrete GPU busy, per mille.
    pub gpu_pm: u16,
    pub gpu2_pm: u16,
    /// Whole degrees: a tenth of a degree is below what anyone acts on, and
    /// storing it would double the width of the column for nothing.
    pub cpu_temp_c: u8,
    pub gpu_temp_c: u8,
    /// Package power draw in whole watts.
    pub power_w: u16,
    /// Whole-machine network throughput in kilobytes per second.
    pub net_rx_kbps: u32,
    pub net_tx_kbps: u32,
    /// Whole-device disk throughput in kilobytes per second.
    pub disk_rd_kbps: u32,
    pub disk_wr_kbps: u32,
    /// Fraction of the interval the busiest device had work in flight, per
    /// mille. A drive can be saturated at modest throughput.
    pub disk_busy_pm: u16,
}

/// One app's reading in one bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPoint {
    /// The model's stable app id.
    pub key: String,
    /// Display name, stored once in the dictionary.
    pub name: String,
    pub cpu_pm: u16,
    pub mem_mb: u32,
    pub gpu_pm: u16,
    pub net_kbps: u32,
    pub disk_kbps: u32,
}

/// A row read back out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSeriesRow {
    pub t: i64,
    pub cpu_pm: u16,
    pub mem_mb: u32,
    pub gpu_pm: u16,
    pub net_kbps: u32,
    pub disk_kbps: u32,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database, applying the schema and the pragmas that
    /// keep it compact.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;

        // WAL keeps the writer from blocking readers, which matters because the
        // application reads this file while the collector writes it. NORMAL
        // synchronous is the right trade for data that is, by definition,
        // disposable history. Incremental auto-vacuum lets pruning actually
        // return space instead of leaving the file permanently at its high-water
        // mark.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;

        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Opens an existing database for reading only.
    ///
    /// The application reads this file while the collector writes it. WAL makes
    /// that safe, but the reader has no business creating tables or changing
    /// pragmas, and opening read-only makes that impossible rather than merely
    /// unlikely.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        Ok(Store { conn })
    }

    /// Picks the finest resolution that still covers the requested span, so a
    /// month-long view does not try to draw half a million points.
    pub fn resolution_for(span_secs: i64) -> u32 {
        if span_secs <= KEEP_FINE {
            RES_FINE
        } else if span_secs <= KEEP_MINUTE {
            RES_MINUTE
        } else {
            RES_COARSE
        }
    }

    fn migrate(&self) -> Result<()> {
        // WITHOUT ROWID throughout: these tables are all keyed by their natural
        // primary key, so the extra hidden rowid and its index would be pure
        // overhead on every row.
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS app (
                id   INTEGER PRIMARY KEY,
                key  TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS system_series (
                res        INTEGER NOT NULL,
                t          INTEGER NOT NULL,
                cpu_pm     INTEGER NOT NULL,
                mem_mb     INTEGER NOT NULL,
                swap_mb    INTEGER NOT NULL,
                psi_cpu_pm INTEGER NOT NULL,
                psi_mem_pm INTEGER NOT NULL,
                psi_io_pm  INTEGER NOT NULL,
                PRIMARY KEY (res, t)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS app_series (
                res    INTEGER NOT NULL,
                t      INTEGER NOT NULL,
                app_id INTEGER NOT NULL,
                cpu_pm INTEGER NOT NULL,
                mem_mb INTEGER NOT NULL,
                PRIMARY KEY (res, t, app_id)
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS app_series_by_app
                ON app_series (app_id, res, t);
            ",
        )?;

        // Columns added after the first release. SQLite's ADD COLUMN is a
        // metadata-only change, so this costs nothing on an existing file and
        // keeps the history that is already recorded.
        self.add_columns(
            "system_series",
            &[
                ("gpu_pm", "INTEGER NOT NULL DEFAULT 0"),
                ("gpu2_pm", "INTEGER NOT NULL DEFAULT 0"),
                ("cpu_temp_c", "INTEGER NOT NULL DEFAULT 0"),
                ("gpu_temp_c", "INTEGER NOT NULL DEFAULT 0"),
                ("power_w", "INTEGER NOT NULL DEFAULT 0"),
                ("net_rx_kbps", "INTEGER NOT NULL DEFAULT 0"),
                ("net_tx_kbps", "INTEGER NOT NULL DEFAULT 0"),
                ("disk_rd_kbps", "INTEGER NOT NULL DEFAULT 0"),
                ("disk_wr_kbps", "INTEGER NOT NULL DEFAULT 0"),
                ("disk_busy_pm", "INTEGER NOT NULL DEFAULT 0"),
            ],
        )?;
        self.add_columns(
            "app_series",
            &[
                ("gpu_pm", "INTEGER NOT NULL DEFAULT 0"),
                ("net_kbps", "INTEGER NOT NULL DEFAULT 0"),
                ("disk_kbps", "INTEGER NOT NULL DEFAULT 0"),
            ],
        )
    }

    fn add_columns(&self, table: &str, columns: &[(&str, &str)]) -> Result<()> {
        let mut existing = std::collections::HashSet::new();
        {
            let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
            for name in names {
                existing.insert(name?);
            }
        }
        for (name, decl) in columns {
            if !existing.contains(*name) {
                self.conn
                    .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl};"))?;
            }
        }
        Ok(())
    }

    /// Records one fine-resolution machine reading.
    pub fn record_system(&self, res: u32, p: &SystemPoint) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO system_series
               (res, t, cpu_pm, mem_mb, swap_mb, psi_cpu_pm, psi_mem_pm, psi_io_pm,
                gpu_pm, gpu2_pm, cpu_temp_c, gpu_temp_c, power_w, net_rx_kbps, net_tx_kbps,
                disk_rd_kbps, disk_wr_kbps, disk_busy_pm)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                res, p.t, p.cpu_pm, p.mem_mb, p.swap_mb, p.psi_cpu_pm, p.psi_mem_pm, p.psi_io_pm,
                p.gpu_pm, p.gpu2_pm, p.cpu_temp_c, p.gpu_temp_c, p.power_w,
                p.net_rx_kbps, p.net_tx_kbps,
                p.disk_rd_kbps, p.disk_wr_kbps, p.disk_busy_pm
            ],
        )?;
        Ok(())
    }

    /// Records a bucket of per-app readings, keeping only the ones worth keeping.
    ///
    /// The caller passes everything; this decides what is worth a row. Taking the
    /// top few by CPU *and* by memory means an app that is quietly holding two
    /// gigabytes is not dropped just because it is using no processor.
    pub fn record_apps(&mut self, res: u32, t: i64, points: &[AppPoint]) -> Result<usize> {
        let mut chosen: Vec<&AppPoint> = Vec::new();

        let mut by_cpu: Vec<&AppPoint> = points.iter().collect();
        by_cpu.sort_unstable_by_key(|p| std::cmp::Reverse(p.cpu_pm));
        chosen.extend(by_cpu.iter().take(APPS_PER_BUCKET).copied());

        let mut by_mem: Vec<&AppPoint> = points.iter().collect();
        by_mem.sort_unstable_by_key(|p| std::cmp::Reverse(p.mem_mb));
        for p in by_mem.iter().take(APPS_PER_BUCKET) {
            if !chosen.iter().any(|c| c.key == p.key) {
                chosen.push(p);
            }
        }

        // Nothing at all is worth a row for an app that did nothing.
        chosen.retain(|p| p.cpu_pm > 0 || p.mem_mb > 0);

        let tx = self.conn.transaction()?;
        {
            let mut lookup = tx.prepare("SELECT id FROM app WHERE key = ?1")?;
            let mut insert_app = tx.prepare("INSERT INTO app (key, name) VALUES (?1, ?2)")?;
            let mut rename = tx.prepare("UPDATE app SET name = ?2 WHERE id = ?1 AND name <> ?2")?;
            let mut insert_row = tx.prepare(
                "INSERT OR REPLACE INTO app_series
                   (res, t, app_id, cpu_pm, mem_mb, gpu_pm, net_kbps, disk_kbps)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;

            for p in &chosen {
                let id: i64 = match lookup.query_row(params![p.key], |r| r.get(0)).optional()? {
                    Some(id) => {
                        // An app can be renamed once its desktop entry is matched;
                        // keep the friendliest name we have seen.
                        rename.execute(params![id, p.name])?;
                        id
                    }
                    None => {
                        insert_app.execute(params![p.key, p.name])?;
                        tx.last_insert_rowid()
                    }
                };
                insert_row.execute(params![
                    res, t, id, p.cpu_pm, p.mem_mb, p.gpu_pm, p.net_kbps, p.disk_kbps
                ])?;
            }
        }
        tx.commit()?;
        Ok(chosen.len())
    }

    /// Folds every complete bucket of `from` resolution into one row of `to`.
    ///
    /// CPU and pressure are averaged because they are rates; memory is averaged
    /// too, since a mean is what a trend line should show. The source rows are
    /// left alone — pruning, not rolling up, is what removes them.
    pub fn rollup_system(&self, from: u32, to: u32, now: i64) -> Result<usize> {
        let bucket = to as i64;
        // Only fold buckets that have closed, so a partial average is never
        // written and then never corrected.
        let cutoff = (now / bucket) * bucket;
        let n = self.conn.execute(
            "INSERT OR REPLACE INTO system_series
               (res, t, cpu_pm, mem_mb, swap_mb, psi_cpu_pm, psi_mem_pm, psi_io_pm,
                gpu_pm, gpu2_pm, cpu_temp_c, gpu_temp_c, power_w, net_rx_kbps, net_tx_kbps,
                disk_rd_kbps, disk_wr_kbps, disk_busy_pm)
             SELECT ?2,
                    (t / ?3) * ?3,
                    CAST(AVG(cpu_pm)     AS INTEGER),
                    CAST(AVG(mem_mb)     AS INTEGER),
                    CAST(AVG(swap_mb)    AS INTEGER),
                    CAST(AVG(psi_cpu_pm) AS INTEGER),
                    CAST(AVG(psi_mem_pm) AS INTEGER),
                    CAST(AVG(psi_io_pm)  AS INTEGER),
                    CAST(AVG(gpu_pm)     AS INTEGER),
                    CAST(AVG(gpu2_pm)    AS INTEGER),
                    -- Temperature and power roll up as the *peak*, not the
                    -- mean: a ten-minute average hides exactly the thermal
                    -- excursion someone opens this view to find.
                    MAX(cpu_temp_c),
                    MAX(gpu_temp_c),
                    MAX(power_w),
                    CAST(AVG(net_rx_kbps) AS INTEGER),
                    CAST(AVG(net_tx_kbps) AS INTEGER),
                    CAST(AVG(disk_rd_kbps) AS INTEGER),
                    CAST(AVG(disk_wr_kbps) AS INTEGER),
                    MAX(disk_busy_pm)
               FROM system_series
              WHERE res = ?1 AND t < ?4
              GROUP BY (t / ?3)",
            params![from, to, bucket, cutoff],
        )?;
        Ok(n)
    }

    /// The same fold for per-app rows.
    pub fn rollup_apps(&self, from: u32, to: u32, now: i64) -> Result<usize> {
        let bucket = to as i64;
        let cutoff = (now / bucket) * bucket;
        let n = self.conn.execute(
            "INSERT OR REPLACE INTO app_series
               (res, t, app_id, cpu_pm, mem_mb, gpu_pm, net_kbps, disk_kbps)
             SELECT ?2, (t / ?3) * ?3, app_id,
                    CAST(AVG(cpu_pm) AS INTEGER),
                    CAST(AVG(mem_mb) AS INTEGER),
                    CAST(AVG(gpu_pm) AS INTEGER),
                    CAST(AVG(net_kbps) AS INTEGER),
                    CAST(AVG(disk_kbps) AS INTEGER)
               FROM app_series
              WHERE res = ?1 AND t < ?4
              GROUP BY (t / ?3), app_id",
            params![from, to, bucket, cutoff],
        )?;
        Ok(n)
    }

    /// Drops everything past its keep-window and hands the freed pages back to
    /// the filesystem. Without the incremental vacuum the file would never
    /// shrink, only stop growing.
    pub fn prune(&self, now: i64) -> Result<usize> {
        let mut removed = 0;
        for (res, keep) in
            [(RES_FINE, KEEP_FINE), (RES_MINUTE, KEEP_MINUTE), (RES_COARSE, KEEP_COARSE)]
        {
            let cutoff = now - keep;
            removed += self.conn.execute(
                "DELETE FROM system_series WHERE res = ?1 AND t < ?2",
                params![res, cutoff],
            )?;
            removed += self.conn.execute(
                "DELETE FROM app_series WHERE res = ?1 AND t < ?2",
                params![res, cutoff],
            )?;
        }

        // Apps nobody has a row for any more are just dead strings.
        removed += self.conn.execute(
            "DELETE FROM app WHERE id NOT IN (SELECT DISTINCT app_id FROM app_series)",
            [],
        )?;

        self.conn.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(removed)
    }

    /// Machine history between two times, oldest first.
    pub fn system_series(&self, res: u32, since: i64, until: i64) -> Result<Vec<SystemPoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT t, cpu_pm, mem_mb, swap_mb, psi_cpu_pm, psi_mem_pm, psi_io_pm,
                    gpu_pm, gpu2_pm, cpu_temp_c, gpu_temp_c, power_w, net_rx_kbps, net_tx_kbps,
                    disk_rd_kbps, disk_wr_kbps, disk_busy_pm
               FROM system_series
              WHERE res = ?1 AND t >= ?2 AND t <= ?3
              ORDER BY t",
        )?;
        let rows = stmt.query_map(params![res, since, until], |r| {
            Ok(SystemPoint {
                t: r.get(0)?,
                cpu_pm: r.get(1)?,
                mem_mb: r.get(2)?,
                swap_mb: r.get(3)?,
                psi_cpu_pm: r.get(4)?,
                psi_mem_pm: r.get(5)?,
                psi_io_pm: r.get(6)?,
                gpu_pm: r.get(7)?,
                gpu2_pm: r.get(8)?,
                cpu_temp_c: r.get(9)?,
                gpu_temp_c: r.get(10)?,
                power_w: r.get(11)?,
                net_rx_kbps: r.get(12)?,
                net_tx_kbps: r.get(13)?,
                disk_rd_kbps: r.get(14)?,
                disk_wr_kbps: r.get(15)?,
                disk_busy_pm: r.get(16)?,
            })
        })?;
        rows.collect()
    }

    /// One app's history, oldest first.
    pub fn app_series(
        &self,
        key: &str,
        res: u32,
        since: i64,
        until: i64,
    ) -> Result<Vec<AppSeriesRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.t, s.cpu_pm, s.mem_mb, s.gpu_pm, s.net_kbps, s.disk_kbps
               FROM app_series s JOIN app a ON a.id = s.app_id
              WHERE a.key = ?1 AND s.res = ?2 AND s.t >= ?3 AND s.t <= ?4
              ORDER BY s.t",
        )?;
        let rows = stmt.query_map(params![key, res, since, until], |r| {
            Ok(AppSeriesRow {
                t: r.get(0)?,
                cpu_pm: r.get(1)?,
                mem_mb: r.get(2)?,
                gpu_pm: r.get(3)?,
                net_kbps: r.get(4)?,
                disk_kbps: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// The app that used the most processor in one bucket.
    ///
    /// This is what turns "something spiked at 17:32" into "Vesktop spiked at
    /// 17:32". Per-app rows exist only at minute resolution and coarser, so the
    /// caller has to round to a bucket that was actually written.
    pub fn top_app_in_bucket(
        &self,
        res: u32,
        t: i64,
        metric: Metric,
    ) -> Result<Option<(String, u32)>> {
        let column = metric.column();
        self.conn
            .query_row(
                &format!(
                    "SELECT a.name, s.{column}
                       FROM app_series s JOIN app a ON a.id = s.app_id
                      WHERE s.res = ?1 AND s.t = ?2
                      ORDER BY s.{column} DESC
                      LIMIT 1"
                ),
                params![res, t],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// The app whose memory grew most between two buckets.
    ///
    /// Memory is a level, not a rate, so an outlier in the *level* only says
    /// "a lot is in use", which the graph already shows. What is worth pointing
    /// at is the moment something started holding much more than it was — so
    /// this compares one bucket against the one before it and returns the
    /// largest riser.
    pub fn biggest_grower(
        &self,
        res: u32,
        previous_t: i64,
        t: i64,
    ) -> Result<Option<(String, u32)>> {
        self.conn
            .query_row(
                "SELECT a.name, (now.mem_mb - was.mem_mb) AS growth
                   FROM app_series now
                   JOIN app_series was
                     ON was.app_id = now.app_id AND was.res = now.res AND was.t = ?3
                   JOIN app a ON a.id = now.app_id
                  WHERE now.res = ?1 AND now.t = ?2 AND growth > 0
                  ORDER BY growth DESC
                  LIMIT 1",
                params![res, t, previous_t],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)),
            )
            .optional()
    }

    /// Row counts per table, for reporting what the history actually costs.
    pub fn stats(&self) -> Result<(i64, i64, i64)> {
        let sys: i64 = self.conn.query_row("SELECT COUNT(*) FROM system_series", [], |r| r.get(0))?;
        let apps: i64 = self.conn.query_row("SELECT COUNT(*) FROM app_series", [], |r| r.get(0))?;
        let names: i64 = self.conn.query_row("SELECT COUNT(*) FROM app", [], |r| r.get(0))?;
        Ok((sys, apps, names))
    }

    /// Bytes the database occupies, counting its write-ahead log.
    pub fn size_bytes(path: &Path) -> u64 {
        let one = |p: std::path::PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let mut total = one(path.to_path_buf());
        for suffix in ["-wal", "-shm"] {
            let mut s = path.as_os_str().to_os_string();
            s.push(suffix);
            total += one(std::path::PathBuf::from(s));
        }
        total
    }
}

/// Where the history lives, honouring `XDG_DATA_HOME`.
pub fn default_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local/share")
        });
    base.join("clearview/history.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests run in parallel inside one process, so the fixture needs a name
    /// of its own or they fight over the same file.
    fn temp_store(name: &str) -> (Store, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("btm-test-{}-{name}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        (Store::open(&path).unwrap(), path)
    }

    #[test]
    fn rollup_averages_a_closed_bucket_only() {
        let (store, path) = temp_store("rollup");
        // Two minutes of fine samples, the second minute still open.
        for i in 0..24 {
            store
                .record_system(
                    RES_FINE,
                    &SystemPoint { t: i * 5, cpu_pm: 100, mem_mb: 1000, ..Default::default() },
                )
                .unwrap();
        }
        // "Now" sits inside the second minute, so only the first may fold.
        store.rollup_system(RES_FINE, RES_MINUTE, 90).unwrap();
        let rows = store.system_series(RES_MINUTE, 0, 10_000).unwrap();
        assert_eq!(rows.len(), 1, "an open bucket must not be written early");
        assert_eq!(rows[0].t, 0);
        assert_eq!(rows[0].cpu_pm, 100);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn only_the_apps_worth_keeping_get_a_row() {
        let (mut store, path) = temp_store("apps");
        let mut points: Vec<AppPoint> = (0..40)
            .map(|i| AppPoint {
                key: format!("app{i}"),
                name: format!("App {i}"),
                cpu_pm: i as u16,
                mem_mb: 0,
                gpu_pm: 0,
                net_kbps: 0,
                disk_kbps: 0,
            })
            .collect();
        // One app using no CPU but a lot of memory must survive on that alone.
        points.push(AppPoint {
            key: "hoarder".into(),
            name: "Hoarder".into(),
            cpu_pm: 0,
            mem_mb: 4096,
            gpu_pm: 0,
            net_kbps: 0,
                        disk_kbps: 0,
        });

        let written = store.record_apps(RES_MINUTE, 60, &points).unwrap();
        assert!(written <= APPS_PER_BUCKET * 2);
        let kept = store.app_series("hoarder", RES_MINUTE, 0, 1000).unwrap();
        assert_eq!(kept.len(), 1, "a memory-heavy app must not be dropped for idling");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pruning_removes_expired_rows_and_orphan_names() {
        let (mut store, path) = temp_store("prune");
        let now = 10_000_000;
        store
            .record_apps(
                RES_FINE,
                now - KEEP_FINE - 100,
                &[AppPoint {
                    key: "old".into(),
                    name: "Old".into(),
                    cpu_pm: 5,
                    mem_mb: 5,
                    gpu_pm: 0,
                    net_kbps: 0,
                        disk_kbps: 0,
                }],
            )
            .unwrap();
        assert_eq!(store.stats().unwrap().2, 1);
        store.prune(now).unwrap();
        let (_, app_rows, names) = store.stats().unwrap();
        assert_eq!(app_rows, 0);
        assert_eq!(names, 0, "a name with no rows left is dead weight");
        let _ = std::fs::remove_file(path);
    }
}

/// Capacity planning, run on demand rather than in the normal suite:
/// `cargo test -p btm-store --release -- --ignored --nocapture`
///
/// Fills a database to the steady state every retention window implies and
/// reports what it actually costs, so the "lightweight" claim is measured
/// rather than asserted.
#[cfg(test)]
mod capacity {
    use super::*;

    #[test]
    #[ignore]
    fn steady_state_size() {
        let path = std::env::temp_dir().join("btm-capacity.db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let mut store = Store::open(&path).unwrap();

        let now: i64 = 1_800_000_000;
        let names: Vec<(String, String)> = (0..40)
            .map(|i| (format!("app.id.number{i}"), format!("Application Number {i}")))
            .collect();

        let mut fill_apps = |res: u32, span: i64, step: i64| {
            let mut t = now - span;
            while t < now {
                let points: Vec<AppPoint> = names
                    .iter()
                    .enumerate()
                    .map(|(i, (key, name))| AppPoint {
                        key: key.clone(),
                        name: name.clone(),
                        cpu_pm: ((t / step + i as i64) % 400) as u16,
                        mem_mb: (200 + (i as u32 * 37) % 3000),
                        gpu_pm: ((t / step) % 300) as u16,
                        net_kbps: ((t / step) % 5000) as u32,
                        disk_kbps: ((t / step) % 900) as u32,
                    })
                    .collect();
                store.record_apps(res, t, &points).unwrap();
                t += step;
            }
        };

        fill_apps(RES_MINUTE, KEEP_MINUTE, 60);
        fill_apps(RES_COARSE, KEEP_COARSE, 600);

        for (res, span, step) in [
            (RES_FINE, KEEP_FINE, 5i64),
            (RES_MINUTE, KEEP_MINUTE, 60),
            (RES_COARSE, KEEP_COARSE, 600),
        ] {
            let mut t = now - span;
            while t < now {
                store
                    .record_system(
                        res,
                        &SystemPoint {
                            t,
                            cpu_pm: ((t / step) % 1000) as u16,
                            mem_mb: 12_000 + ((t / step) % 4000) as u32,
                            swap_mb: 3,
                            psi_cpu_pm: 12,
                            psi_mem_pm: 0,
                            psi_io_pm: 48,
                            gpu_pm: 300,
                            gpu2_pm: 120,
                            cpu_temp_c: 78,
                            gpu_temp_c: 64,
                            power_w: 28,
                            net_rx_kbps: 900,
                            net_tx_kbps: 300,
                            disk_rd_kbps: 400,
                            disk_wr_kbps: 150,
                            disk_busy_pm: 120,
                        },
                    )
                    .unwrap();
                t += step;
            }
        }

        store.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let (sys, apps, app_names) = store.stats().unwrap();
        let bytes = Store::size_bytes(&path);

        println!("\n  === steady state, every retention window full ===");
        println!("  machine rows   {sys}");
        println!("  per-app rows   {apps}");
        println!("  app names      {app_names}");
        println!("  ON DISK        {:.2} MB", bytes as f64 / 1024.0 / 1024.0);
        println!("  per app-row    {:.1} bytes\n", bytes as f64 / (sys + apps).max(1) as f64);

        // The whole point of the schema. If this ever fails, the design has
        // drifted from the promise.
        assert!(
            bytes < 32 * 1024 * 1024,
            "history grew to {:.1} MB, which is no longer lightweight",
            bytes as f64 / 1024.0 / 1024.0
        );
        let _ = std::fs::remove_file(&path);
    }
}
