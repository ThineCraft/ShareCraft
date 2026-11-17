//! The shared collaboration library for live sessions: each editor builds it in to join a
//! session, send a person's changes and apply everyone's changes in the server's order.
//!
//! It does no input or output of its own: the host hands it each frame received and the
//! current time, and it returns the frames to send and what happened. It has no clocks,
//! threads, sockets or randomness, so the same code serves the browser and the desktop.
#![forbid(unsafe_code)]

pub mod client;
pub mod ids;
#[cfg(feature = "stand-in")]
pub mod stand_in;
