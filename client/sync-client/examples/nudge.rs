//! Verify the ActionCable nudge end-to-end: subscribe to `LibraryChannel`, perform a
//! commit on the server, and confirm the `{head_seq}` push arrives near-instantly. This
//! is the latency fast-path the daemon's sync loop waits on (alongside the poll).
//!
//!   SERVER_URL=http://localhost:3001 cargo run --example nudge

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sync_client::ServerClient;

fn uniq() -> String {
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

fn main() {
    let server = std::env::var("SERVER_URL").unwrap_or_else(|_| "http://localhost:3001".into());

    let mut client = ServerClient::new(&server);
    client.register(&format!("nudge-{}@example.com", uniq()), "device-A").expect("register");

    // Subscribe in a background thread; forward each head_seq nudge over a channel.
    let (tx, rx) = mpsc::channel::<i64>();
    let watcher = client.clone();
    thread::spawn(move || {
        if let Err(e) = watcher.watch_library(|seq| {
            let _ = tx.send(seq);
        }) {
            eprintln!("watch_library ended: {e}");
        }
    });
    // Give the WebSocket time to connect + confirm the subscription before we commit.
    thread::sleep(Duration::from_millis(800));
    println!("✓ subscribed to LibraryChannel");

    // Trigger a server-side change → the server broadcasts {head_seq}.
    let started = Instant::now();
    let resp = client.mkdir(&uniq(), None, &format!("nudge-{}", uniq()), 0o755, 0).expect("mkdir");
    println!("✓ committed a mkdir (server head_seq {})", resp.seq);

    // The nudge should arrive within a beat (not the 2s poll interval).
    let seq = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("no nudge received within 5s — ActionCable path is broken");
    let latency = started.elapsed();
    assert_eq!(seq, resp.seq, "nudge head_seq matches the commit's seq");
    println!("✓ nudge received: head_seq {seq} in {latency:?} (well under the poll interval)");

    println!("\nNUDGE OK — LibraryChannel push wakes the sync loop on commit.");
}
