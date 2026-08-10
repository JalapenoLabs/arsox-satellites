// Copyright © 2026 Jalapeno Labs

//! Running a harness process, and the mapping it feeds.
//!
//! The mapping itself moved to `arsox-harness`, which has no I/O and can be
//! depended on by an application that supervises its own processes. What stays
//! here is the part that needs a runtime: spawning, pipes, and lifetime.
//!
//! The mapper types are re-exported so this module reads the same as before to
//! everything above it.

pub mod runner;
pub mod spawn;

pub use arsox_harness::{HarnessResult, MappedEvent, Mapping, claude};
