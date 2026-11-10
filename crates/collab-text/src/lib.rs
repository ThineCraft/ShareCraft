#![forbid(unsafe_code)]
//! Plain-text documents: retain / insert / delete operations with apply and transform.
//!
//! All counts are Unicode scalar values (`char`), never bytes or UTF-16 units.

mod op;
mod transform;

pub use op::{Component, TextDoc, TextError, TextOp, TextOpBuilder};
