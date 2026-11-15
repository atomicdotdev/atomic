//! Read-only context witness; shared with the feature-gated native validator.
use atomic_repository::Repository;
use atomic_repository::{changestore, repository};
#[cfg(test)]
use atomic_repository::{record, TrackingOptions, UnrecordOptions};
#[path = "../src/context_witness_prototype.rs"]
mod witness;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    witness::run_example()
}
