//! Lustre resource observations for the registered filesystem catalog.

mod application;
mod capacity;
mod collector;
mod observation;
mod repository;

use std::time::{SystemTime, UNIX_EPOCH};

pub use application::ResourceObservations;
pub use observation::ResourceSnapshot;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
