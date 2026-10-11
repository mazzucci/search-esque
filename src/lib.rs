//! search-esque: Elasticsearch-compatible query DSL and aggregations over SQLite.
//!
//! The crate is a pure function from a request and a mapping to an answer. Nothing in it
//! does I/O, so it compiles to a native binary, to WebAssembly and to a Python extension
//! alike. The pieces, in the order a request passes through them:
//!
//! - [`mapping`]: the fields, their types, and what each one allows.
//! - [`dsl`]: the walk over Elasticsearch JSON into the [`ast`], refusing everything outside
//!   the profile with an error that names the path in the request.
//! - [`eval`]: the reference evaluator, the same semantics written plainly over JSON records.
//!   Slow and obviously correct; the SQL compiler must agree with it on every query.
//! - [`sql`]: the schema a mapping needs and the statements a request compiles to, for SQLite.
//! - [`store`] (feature `sqlite`): the executor that runs them and answers like the evaluator.
//! - [`shape`]: Elasticsearch's response shape, shared by both engines.
//!
//! Semantics the profile commits to, and the places where it deliberately differs from
//! Elasticsearch, are documented on the items themselves.

pub mod ast;
pub mod dsl;
pub mod error;
pub mod eval;
pub mod mapping;
pub mod shape;
pub mod sql;
#[cfg(feature = "sqlite")]
pub mod store;
pub mod timeutil;
pub mod tokenize;

pub use ast::Request;
pub use error::{Error, Result};
pub use mapping::Mapping;
