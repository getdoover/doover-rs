//! Ports of `pydoover.utils` — the small shared helpers doover apps and the
//! runtime itself rely on.

mod diff;
mod snowflake;

pub use diff::{apply_diff, apply_diff_in_place, generate_diff, merged_diff_is_empty};
pub use snowflake::{
    generate_snowflake_id, generate_snowflake_id_at, unix_millis_from_snowflake, SnowflakeType,
    DOOVER_EPOCH,
};
