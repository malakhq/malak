//! Tests that built-in and external Malak layers can run against their implementations.

pub mod store;

pub mod tasks;

pub mod transport;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix_socket;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod process;
