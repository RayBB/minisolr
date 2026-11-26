//! Throwaway Rust prototype of minisolr's `/select`.
//!
//! It exists to measure memory and to check analyzer parity against the Python
//! implementation, not to replace it. The field table is hard-coded.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod error;
pub mod porter;
pub mod query;
pub mod server;
pub mod store;

pub use error::Error;
