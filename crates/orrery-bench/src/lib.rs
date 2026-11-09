//! Benchmarks that drive a served world the way its consumers do: real `git`
//! and real HTTP against `orrery serve`. Each is a binary in `src/bin/`, run by
//! hand through its `scripts/*-bench.sh` and never by the test suite;
//! `docs/contributors/benchmarks/` has each one's scenarios and recorded
//! results.

pub mod git;
pub mod payload;
pub mod serve;
pub mod stats;
