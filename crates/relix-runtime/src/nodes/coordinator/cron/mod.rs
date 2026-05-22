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

pub mod handlers;
pub mod schedule;
pub mod scheduler;
pub mod store;

pub use schedule::{CronField, Schedule, ScheduleError};
pub use scheduler::{
    CronAiDispatcher, CronAiDispatcherCell, CronAiMeshDispatcher, CronAiPeerConfig,
    CronSchedulerConfig, FireOutcome, fire_job, register_trigger, run_one_tick,
    spawn_cron_scheduler,
};
pub use store::{CronJob, CronJobSummary, CronStore, CronStoreError};

use std::sync::Arc;

use crate::dispatch::{DispatchBridge, FnHandler, InvocationCtx};

/// Register the `cron.create / list / get / update / delete`
/// capabilities on the coordinator's dispatch bridge. The
/// `cron.trigger` handler is registered separately alongside
/// the scheduler since it needs both stores + the AI cell.
pub fn register(bridge: &mut DispatchBridge, store: Arc<CronStore>) {
    {
        let s = store.clone();
        bridge.register(
            "cron.create",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_create(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "cron.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_list(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "cron.get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "cron.update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_update(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "cron.delete",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_delete(&s, &ctx) }
            })),
        );
    }
}
