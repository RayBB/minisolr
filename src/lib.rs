//! Rust port of minisolr, a small Solr stand-in for Open Library development (see
//! `PORTING.md`).
//!
//! Fields come from Solr's `managed-schema.xml`; the analyzer chains reproduce the
//! Python implementation token for token.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod difflib;
pub mod error;
pub mod function;
pub mod index;
pub mod md5;
pub mod ping;
pub mod porter;
pub mod query;
pub mod schema;
pub mod server;
pub mod snowball;
pub mod store;
pub mod unicode;
pub mod update;

pub use error::Error;
