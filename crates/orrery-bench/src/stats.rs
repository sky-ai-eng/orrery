//! The benches' clock, and the summaries they report.

use std::fmt;
use std::time::{Duration, Instant};

/// Benchmarks measure how long real work takes, so they read real time.
/// Nothing they time is an input to a world.
#[allow(clippy::disallowed_methods)]
pub fn now() -> Instant {
    Instant::now()
}

/// Run `f`; what it returned, and how long it took.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = now();
    let out = f();
    (out, start.elapsed())
}

/// p50, p90, and max of a sample, each the nearest rank.
#[derive(Clone, Copy, Debug)]
pub struct Summary {
    pub n: usize,
    pub p50: Duration,
    pub p90: Duration,
    pub max: Duration,
}

impl Summary {
    pub fn of(sample: &[Duration]) -> Option<Summary> {
        let mut xs = sample.to_vec();
        xs.sort();
        let rank = |q: f64| xs[((q * xs.len() as f64).ceil() as usize).clamp(1, xs.len()) - 1];
        (!xs.is_empty()).then(|| Summary {
            n: xs.len(),
            p50: rank(0.5),
            p90: rank(0.9),
            max: xs[xs.len() - 1],
        })
    }
}

/// `p50 / p90 / max`, in milliseconds.
impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} / {} / {}", ms(self.p50), ms(self.p90), ms(self.max))
    }
}

pub fn ms(d: Duration) -> String {
    format!("{}", d.as_millis())
}

/// Columns padded to their widest cell, for a report read in a terminal and
/// pasted into a document.
pub struct Table {
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(header: &[&str]) -> Table {
        Table {
            rows: vec![header.iter().map(|h| h.to_string()).collect()],
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn print(&self) {
        let widths: Vec<usize> = (0..self.rows[0].len())
            .map(|c| self.rows.iter().map(|r| r[c].len()).max().unwrap_or(0))
            .collect();
        for row in &self.rows {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, w)| format!("{cell:<w$}"))
                .collect();
            println!("  {}", cells.join("  ").trim_end());
        }
        println!();
    }
}
