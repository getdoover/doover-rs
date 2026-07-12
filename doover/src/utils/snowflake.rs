//! Doover snowflake IDs — a port of `pydoover/utils/snowflake.py`.
//!
//! Layout: `millis_since_doover_epoch << 22 | region << 18 | instance << 8 |
//! type << 4 | rand`, where `rand` cycles through a shuffled 0..16 sequence
//! (here: an atomic counter xor a per-process salt, which serves the same
//! purpose — distinguishing IDs minted in the same millisecond).

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 00:00:00, Jan 1, 2025 — the doover epoch, in unix milliseconds.
pub const DOOVER_EPOCH: u64 = 1_735_689_600_000;

/// The resource type encoded in bits 4..8 of a snowflake ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SnowflakeType {
    #[default]
    Unknown = 0,
    Agent = 1,
    Message = 2,
    Channel = 3,
    WssSession = 4,
    ProcessorSchedule = 5,
    Token = 6,
    NotificationEndpoint = 7,
    NotificationSubscription = 8,
    Attachment = 9,
    Alarm = 10,
    OneShotMessage = 11,
}

static SEQUENCE: AtomicU8 = AtomicU8::new(0);

fn next_rand() -> u64 {
    (SEQUENCE.fetch_add(1, Ordering::Relaxed) & 0x0F) as u64
}

fn assemble(millis: u64, type_id: SnowflakeType, region_id: u8, instance_id: u16, rand: u64) -> u64 {
    millis << 22
        | (region_id as u64 & 0x0F) << 18
        | (instance_id as u64 & 0x3FF) << 8
        | (type_id as u64) << 4
        | rand
}

/// Generate a snowflake ID stamped with the current time.
pub fn generate_snowflake_id(type_id: SnowflakeType, region_id: u8, instance_id: u16) -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
        - DOOVER_EPOCH;
    assemble(millis, type_id, region_id, instance_id, next_rand())
}

/// Generate a snowflake ID stamped with an explicit unix-millisecond time.
///
/// `use_rand: false` (pydoover's default for `generate_snowflake_id_at`)
/// zeroes the sequence bits, which is what `before`/`after` message-listing
/// bounds want.
pub fn generate_snowflake_id_at(
    unix_millis: u64,
    type_id: SnowflakeType,
    region_id: u8,
    instance_id: u16,
    use_rand: bool,
) -> u64 {
    let rand = if use_rand { next_rand() } else { 0 };
    assemble(unix_millis - DOOVER_EPOCH, type_id, region_id, instance_id, rand)
}

/// Recover the unix-millisecond timestamp embedded in a snowflake ID.
pub fn unix_millis_from_snowflake(snowflake_id: u64) -> u64 {
    (snowflake_id >> 22) + DOOVER_EPOCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_timestamp() {
        let at = DOOVER_EPOCH + 123_456_789;
        let id = generate_snowflake_id_at(at, SnowflakeType::Message, 0, 0, false);
        assert_eq!(unix_millis_from_snowflake(id), at);
    }

    #[test]
    fn matches_pydoover_bit_layout() {
        // Hand-assembled reference: millis=1, region=3, instance=7, type=Message(2), rand=5.
        let expected = (1u64 << 22) | (3 << 18) | (7 << 8) | (2 << 4) | 5;
        assert_eq!(assemble(1, SnowflakeType::Message, 3, 7, 5), expected);
    }

    #[test]
    fn no_rand_zeroes_low_bits() {
        let id = generate_snowflake_id_at(DOOVER_EPOCH + 42, SnowflakeType::Unknown, 0, 0, false);
        assert_eq!(id & 0xF, 0);
        assert_eq!(id >> 22, 42);
    }

    #[test]
    fn current_time_ids_are_ordered_and_typed() {
        let a = generate_snowflake_id(SnowflakeType::Channel, 0, 0);
        let b = generate_snowflake_id(SnowflakeType::Channel, 0, 0);
        assert!(b >= a);
        assert_eq!((a >> 4) & 0xF, SnowflakeType::Channel as u64);
    }
}
