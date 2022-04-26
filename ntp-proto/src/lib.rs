#![forbid(unsafe_code)]

mod clock;
mod clock_select;
mod filter;
mod identifiers;
mod packet;
mod peer;
mod time_types;

pub use clock::NtpClock;
#[cfg(feature = "fuzz")]
pub use clock_select::fuzz_find_interval;
pub use identifiers::ReferenceId;
pub use packet::NtpHeader;
pub use time_types::{NtpDuration, NtpTimestamp};