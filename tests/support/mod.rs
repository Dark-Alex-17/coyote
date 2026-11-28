//! Helpers shared by the integration tests. A test crate declares `mod support;`
//! and reaches the parts it needs through the submodules.

#[allow(dead_code)] // each test crate uses a subset
pub mod repo;

#[allow(dead_code)] // each test crate uses a subset
pub mod bin;

#[allow(dead_code)] // each test crate uses a subset
pub mod pty;

#[allow(dead_code)] // each test crate uses a subset
pub mod script;
