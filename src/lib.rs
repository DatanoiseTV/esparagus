//! esparagus library entry point.
//!
//! Re-exports the modules that compose the CLI binary. Stable for in-process
//! use only; not yet versioned as a public API.

pub mod chip;
pub mod cli;
pub mod error;
pub mod esptool_compat;
pub mod image;
pub mod imagegen;
pub mod monitor;
pub mod nvs;
pub mod observe;
pub mod ops;
pub mod partition;
pub mod protocol;
pub mod reset;
pub mod runner;
pub mod stub;
pub mod transport;
pub mod tui;
