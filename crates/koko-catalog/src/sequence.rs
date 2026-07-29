use koko_common::{Error, Result};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A catalog-owned sequence.
#[derive(Debug)]
pub struct Sequence {
    pub(crate) name: String,
    pub(crate) start: i64,
    pub(crate) increment: i64,
    pub(crate) min: i64,
    pub(crate) max: i64,
    pub(crate) cycle: bool,
    pub(crate) curr: AtomicI64,
    pub(crate) usage: AtomicU64,
}

impl Clone for Sequence {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            start: self.start,
            increment: self.increment,
            min: self.min,
            max: self.max,
            cycle: self.cycle,
            curr: AtomicI64::new(self.curr.load(Ordering::Relaxed)),
            usage: AtomicU64::new(self.usage.load(Ordering::Relaxed)),
        }
    }
}

impl Sequence {
    pub fn new(name: String, start: i64, increment: i64, min: i64, max: i64, cycle: bool) -> Self {
        Self {
            name,
            start,
            increment,
            min,
            max,
            cycle,
            curr: AtomicI64::new(start),
            usage: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn start(&self) -> i64 {
        self.start
    }

    pub const fn increment(&self) -> i64 {
        self.increment
    }

    pub const fn min(&self) -> i64 {
        self.min
    }

    pub const fn max(&self) -> i64 {
        self.max
    }

    pub const fn cycle(&self) -> bool {
        self.cycle
    }

    pub const fn display_val(&self) -> i64 {
        self.start
    }

    pub(crate) fn next_val(&self) -> Result<i64> {
        if self.usage.load(Ordering::Relaxed) == 0 {
            self.usage.store(1, Ordering::Relaxed);
            return Ok(self.curr.load(Ordering::Relaxed));
        }
        let min_error = || {
            Error::catalog(format!(
                "nextval: reached minimum value of sequence \"{}\" {}",
                self.name, self.min
            ))
        };
        let max_error = || {
            Error::catalog(format!(
                "nextval: reached maximum value of sequence \"{}\" {}",
                self.name, self.max
            ))
        };
        let checked = self
            .curr
            .load(Ordering::Relaxed)
            .checked_add(self.increment);
        let next = if self.cycle {
            match checked {
                None if self.increment < 0 => self.max,
                None => self.min,
                Some(value) if value < self.min => self.max,
                Some(value) if value > self.max => self.min,
                Some(value) => value,
            }
        } else {
            match checked {
                None if self.increment < 0 => return Err(min_error()),
                None => return Err(max_error()),
                Some(value) if value < self.min => return Err(min_error()),
                Some(value) if value > self.max => return Err(max_error()),
                Some(value) => value,
            }
        };
        self.curr.store(next, Ordering::Relaxed);
        self.usage.fetch_add(1, Ordering::Relaxed);
        Ok(next)
    }

    pub(crate) fn curr_val(&self) -> Result<i64> {
        if self.usage.load(Ordering::Relaxed) == 0 {
            return Err(Error::catalog(format!(
                "currval: sequence \"{}\" is not yet defined. To define the sequence, call nextval first.",
                self.name
            )));
        }
        Ok(self.curr.load(Ordering::Relaxed))
    }
}

/// The implicit sequence name for a `SERIAL` column.
pub fn serial_sequence_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_serial")
}
