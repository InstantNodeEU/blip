#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Pending,
    Up,
    Down,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Up => "up",
            Status::Down => "down",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Change {
    Down,
    Up { downtime: u64 },
}

/// Turns raw check results into a confirmed status. A check has to fail
/// `retries + 1` times in a row before it counts as down, so a single
/// dropped packet doesn't wake anyone up.
pub struct Tracker {
    status: Status,
    fails: u32,
    retries: u32,
    down_since: u64,
}

impl Tracker {
    pub fn new(retries: u32) -> Self {
        Tracker {
            status: Status::Pending,
            fails: 0,
            retries,
            down_since: 0,
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// True while a check is failing but hasn't been declared down yet.
    pub fn retrying(&self) -> bool {
        self.fails > 0 && self.status != Status::Down
    }

    pub fn record(&mut self, ok: bool, now: u64) -> Option<Change> {
        if ok {
            self.fails = 0;
            let was = self.status;
            self.status = Status::Up;
            return (was == Status::Down).then(|| Change::Up {
                downtime: now.saturating_sub(self.down_since),
            });
        }
        self.fails += 1;
        if self.status != Status::Down && self.fails > self.retries {
            self.status = Status::Down;
            self.down_since = now;
            return Some(Change::Down);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_consecutive_failures() {
        let mut t = Tracker::new(2);
        assert_eq!(t.record(true, 0), None);
        assert_eq!(t.status(), Status::Up);

        assert_eq!(t.record(false, 10), None);
        assert_eq!(t.record(false, 20), None);
        assert!(t.retrying());
        assert_eq!(t.status(), Status::Up);

        // a success in between resets the count
        assert_eq!(t.record(true, 30), None);
        assert!(!t.retrying());
        assert_eq!(t.record(false, 40), None);
        assert_eq!(t.record(false, 50), None);
        assert_eq!(t.record(false, 60), Some(Change::Down));
        assert_eq!(t.status(), Status::Down);
        assert!(!t.retrying());
    }

    #[test]
    fn notifies_once_per_outage() {
        let mut t = Tracker::new(0);
        assert_eq!(t.record(false, 100), Some(Change::Down));
        assert_eq!(t.record(false, 160), None);
        assert_eq!(t.record(false, 220), None);
        assert_eq!(t.record(true, 400), Some(Change::Up { downtime: 300 }));
        assert_eq!(t.record(true, 460), None);
    }

    #[test]
    fn down_from_the_start() {
        let mut t = Tracker::new(1);
        assert_eq!(t.status(), Status::Pending);
        assert_eq!(t.record(false, 0), None);
        assert_eq!(t.status(), Status::Pending);
        assert_eq!(t.record(false, 20), Some(Change::Down));
    }
}
