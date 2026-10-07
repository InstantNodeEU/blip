use rusqlite::{params, Connection, Result, Row};

// Results are folded into hourly buckets as they come in, so the database
// stays at a few hundred kilobytes no matter how often you check.
const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = -512;
CREATE TABLE IF NOT EXISTS monitors (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS hourly (
    monitor INTEGER NOT NULL,
    hour    INTEGER NOT NULL,
    up      INTEGER NOT NULL,
    total   INTEGER NOT NULL,
    ms      INTEGER NOT NULL,
    PRIMARY KEY (monitor, hour)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS events (
    monitor INTEGER NOT NULL,
    ts      INTEGER NOT NULL,
    up      INTEGER NOT NULL,
    detail  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
";

pub struct Store {
    conn: Connection,
}

pub struct Event {
    pub name: String,
    pub ts: u64,
    pub up: bool,
    pub detail: String,
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    pub fn monitor_id(&self, name: &str) -> Result<i64> {
        self.conn
            .execute("INSERT OR IGNORE INTO monitors (name) VALUES (?1)", [name])?;
        self.conn
            .query_row("SELECT id FROM monitors WHERE name = ?1", [name], |r| {
                r.get(0)
            })
    }

    pub fn record(&self, monitor: i64, ts: u64, ok: bool, ms: u32) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO hourly VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT (monitor, hour) DO UPDATE SET
                up = up + excluded.up, total = total + 1, ms = ms + excluded.ms",
        )?;
        let ms = if ok { ms } else { 0 };
        stmt.execute(params![monitor, (ts / 3600) as i64, ok, ms])?;
        Ok(())
    }

    pub fn event(&self, monitor: i64, ts: u64, up: bool, detail: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events VALUES (?1, ?2, ?3, ?4)",
            params![monitor, ts as i64, up, detail],
        )?;
        Ok(())
    }

    /// Per-day `(day, up, total)` since the given unix day, oldest first.
    pub fn days(&self, monitor: i64, from_day: u64) -> Result<Vec<(u64, u64, u64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT hour / 24, sum(up), sum(total) FROM hourly
             WHERE monitor = ?1 AND hour >= ?2
             GROUP BY hour / 24 ORDER BY 1",
        )?;
        let rows = stmt.query_map(params![monitor, (from_day * 24) as i64], |r| {
            Ok((int(r, 0)?, int(r, 1)?, int(r, 2)?))
        })?;
        rows.collect()
    }

    /// `(up, total, sum of ms)` since the given unix hour.
    pub fn since(&self, monitor: i64, from_hour: u64) -> Result<(u64, u64, u64)> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT coalesce(sum(up), 0), coalesce(sum(total), 0), coalesce(sum(ms), 0)
             FROM hourly WHERE monitor = ?1 AND hour >= ?2",
        )?;
        stmt.query_row(params![monitor, from_hour as i64], |r| {
            Ok((int(r, 0)?, int(r, 1)?, int(r, 2)?))
        })
    }

    pub fn events(&self, monitors: &[i64], limit: u32) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.id, m.name, e.ts, e.up, e.detail
             FROM events e JOIN monitors m ON m.id = e.monitor
             ORDER BY e.ts DESC, e.rowid DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Event {
                    name: r.get(1)?,
                    ts: int(r, 2)?,
                    up: r.get(3)?,
                    detail: r.get(4)?,
                },
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, ev) = row?;
            if monitors.contains(&id) {
                out.push(ev);
            }
            if out.len() == limit as usize {
                break;
            }
        }
        Ok(out)
    }

    pub fn prune(&self, now: u64, keep_days: u32) -> Result<()> {
        let cutoff = now.saturating_sub(keep_days as u64 * 86400);
        self.conn.execute(
            "DELETE FROM hourly WHERE hour < ?1",
            [(cutoff / 3600) as i64],
        )?;
        self.conn
            .execute("DELETE FROM events WHERE ts < ?1", [cutoff as i64])?;
        Ok(())
    }

    pub fn close(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }
}

fn int(r: &Row, i: usize) -> Result<u64> {
    r.get::<_, i64>(i).map(|v| v.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_and_prunes() {
        let s = Store::open(":memory:").unwrap();
        let a = s.monitor_id("a").unwrap();
        assert_eq!(s.monitor_id("a").unwrap(), a);
        let b = s.monitor_id("b").unwrap();

        let day = 20_000 * 86400;
        s.record(a, day + 10, true, 100).unwrap();
        s.record(a, day + 70, false, 999).unwrap();
        s.record(a, day + 86400 + 5, true, 50).unwrap();
        s.record(b, day, true, 1).unwrap();

        assert_eq!(
            s.days(a, 20_000).unwrap(),
            vec![(20_000, 1, 2), (20_001, 1, 1)]
        );
        assert_eq!(s.since(a, (day + 86400) / 3600).unwrap(), (1, 1, 50));

        s.event(a, day + 70, false, "timeout").unwrap();
        s.event(b, day + 80, true, "").unwrap();
        let ev = s.events(&[a], 10).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].detail, "timeout");

        s.prune(day + 86400 * 3, 2).unwrap();
        assert_eq!(s.days(a, 0).unwrap(), vec![(20_001, 1, 1)]);
        assert!(s.events(&[a, b], 10).unwrap().is_empty());
    }
}
