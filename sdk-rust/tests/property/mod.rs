//! Test support of the property test of `use_meter` (`use_property.rs`): a case
//! and its generator, the server, the machine the real `License` runs on, and
//! the world that steps a case and judges it. Never part of the crate.

pub mod case;
pub mod cases;
pub mod net;
pub mod server;
pub mod world;

use std::panic::{catch_unwind, AssertUnwindSafe};

use case::Case;
use net::PropertyFailure;
use world::World;

/// Run a case: none when every property held. With `trace` the trace of what happened is given too.
pub fn run_case(case: &Case, trace: bool) -> (Option<PropertyFailure>, Vec<String>) {
    let mut world = World::new(case.clone(), trace);
    let ran = catch_unwind(AssertUnwindSafe(|| world.run()));
    let failure = match ran {
        Ok(Ok(())) => None,
        Ok(Err(failure)) => Some(failure),
        Err(panic) => {
            let text = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                .unwrap_or_else(|| "a panic".into());
            Some(PropertyFailure::new("exception", text))
        }
    };
    (failure, world.trace())
}
