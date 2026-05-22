//! Cron scheduler — agents schedule their own future work.
//!
//! Three pieces:
//!
//! - [`schedule`] — schedule-expression parser. Three formats:
//!   duration (`30m`), 5-field cron (`0 9 * * 1`), one-shot
//!   RFC 3339 timestamp.
//!
//! Storage (`store`), the periodic background loop (`scheduler`),
//! and the `cron.*` capability handlers (`handlers`) land in
//! follow-up commits.

pub mod schedule;

pub use schedule::{CronField, Schedule, ScheduleError};
