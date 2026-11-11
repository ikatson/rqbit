//! I2P through the SAM v3 bridge of a local I2P router.

// The session layer that uses these is not wired up yet.
#![allow(dead_code)]

pub(crate) mod book;
#[cfg(test)]
pub(crate) mod fake_bridge;
#[cfg(test)]
mod fake_bridge_tests;
pub(crate) mod sam;
