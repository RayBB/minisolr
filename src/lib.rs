//! Rust port of minisolr's `/select`, in progress (see `PORTING.md`).
//!
//! Fields come from Solr's `managed-schema.xml`; the analyzer chains reproduce the
//! Python implementation token for token.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod error;
pub mod function;
pub mod porter;
pub mod query;
pub mod schema;
pub mod server;
pub mod store;

pub use error::Error;
