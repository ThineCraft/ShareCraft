#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! A client that stops reading must not hold its connection, and so its place in the room,
//! forever. This file has its own test binary because it shortens the socket write timeout for
//! every connection in the process.

mod common;

use std::time::Duration;

use common::{TestClient, new_doc, spawn_server_config};

/// Shorter than the 10 seconds a real connection gets, so the test does not wait that long.
const WRITE_TIMEOUT: Duration = Duration::from_millis(300);

/// Typed in one change each time; large enough that the socket buffers between the server and
/// a client that never reads fill up within the volume below.
const CHUNK: usize = 400 * 1024;
/// Changes typed. Fewer than the 256 frames a connection's outbound queue holds (with the
/// handful of other frames the room sends), so the queue cannot fill and drop the client: only
/// the write timeout can.
const CHANGES: usize = 80;

#[tokio::test]
async fn stalled_reader_is_dropped() {
    server::config::set_write_timeout_for_test(WRITE_TIMEOUT);
    // Nothing is saved while the test types: saving megabytes of text is not what is tested.
    let server = spawn_server_config(|c| {
        c.checkpoint_idle = Duration::from_secs(600);
        c.checkpoint_max = Duration::from_secs(600);
    })
    .await;
    let doc = new_doc();
    let mut stalled = TestClient::connect(&server, doc, "Stalled").await;
    let mut typist = TestClient::connect(&server, doc, "Typist").await;
    typist.pump_until(|c| c.people().len() == 2).await;

    // `stalled` is never read from again. The typist's changes are all sent to it; once its
    // socket buffers are full the server's write blocks and times out.
    let chunk = "x".repeat(CHUNK);
    for _ in 0..CHANGES {
        let at = typist.text().chars().count();
        typist.type_at(at, &chunk).await;
        typist
            .pump_until(|c| c.pending() == 0 || c.people().len() == 1)
            .await;
        if typist.people().len() == 1 {
            break;
        }
    }

    // The room was told the stalled client left.
    typist.pump_until(|c| c.people().len() == 1).await;
    assert_eq!(typist.people(), vec!["Typist".to_owned()]);

    // The room carries on for those who remain.
    let at = typist.text().chars().count();
    typist.type_at(at, "still here").await;
    typist.pump_until(|c| c.pending() == 0).await;
    assert!(typist.text().ends_with("still here"));
    // Reading it is allowed, but it is on a connection the server has let go of.
    stalled.disconnect();
}
