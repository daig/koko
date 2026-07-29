//! Pure planning and optimization over `koko-ir` plan records.

mod builder;
mod cost;
mod optimize;

pub use builder::{plan, plan_regular};
pub use cost::StatsMap;
pub use optimize::{optimize, optimize_regular};
