//! Shell support: selection, permission scanning, process service, result text.

pub mod daemon;
pub mod parse;
pub mod result;
pub mod scan;
pub mod select;
pub mod service;
#[cfg(all(test, unix))]
mod daemon_tests;
