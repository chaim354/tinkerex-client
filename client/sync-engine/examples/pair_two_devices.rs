//! Register device A + pair device B into the SAME library via the **real pairing flow**, and
//! print both device tokens — the seed step for the two-device, real-mount end-to-end tests
//! (`client/test-conformance/two-device-cfapi.ps1` on Windows, `two-device-fuse.sh` on Linux).
//!
//! This is exactly the Tauri app's secure multi-device join: device A `register`s (creating the
//! account/library), mints a single-use pairing code with `create_pairing` (the app's
//! `create_pairing` command), and a brand-new device B redeems it with `pair` (the app's
//! `sync_pair` command) — B never sees A's token. Both tokens then drive a real kernel mount each
//! (`maker-syncd`/`maker-cfsyncd`) so a file written through one mount can be read through the other.
//!
//!   SERVER_URL=http://localhost:3001 cargo run --example pair_two_devices
//!   # or: cargo run --example pair_two_devices -- <server_url> [email]
//!
//! Output (machine-parseable; the test scripts grep these lines — keep them stable):
//!   PAIR_EMAIL=<email>
//!   DEVICE_A_TOKEN=<token>
//!   DEVICE_B_TOKEN=<token>
//! Exit 0 on success; non-zero (with a diagnostic) on any API failure.

use std::time::{SystemTime, UNIX_EPOCH};

use sync_client::ServerClient;

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .or_else(|| std::env::var("SERVER_URL").ok())
        .unwrap_or_else(|| "http://localhost:3001".to_string());
    let email = args.next().or_else(|| std::env::var("EMAIL").ok()).unwrap_or_else(|| {
        let tag = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("twodev-{tag:x}@example.com")
    });

    // Device A registers — creates the account/library (the app's `sync_start` first-run path).
    let mut ca = ServerClient::new(&server);
    let reg_a = match ca.register(&email, "device-A") {
        Ok(r) => r,
        Err(e) => {
            eprintln!("register A failed (is the server up at {server}? email free?): {e}");
            std::process::exit(1);
        }
    };

    // Device A mints a single-use pairing code; brand-new device B redeems it (never sees A's
    // token) — the secure multi-device join (`DevicePairing`, the Syncthing/Plex model).
    let code = match ca.create_pairing() {
        Ok(p) => p.code,
        Err(e) => {
            eprintln!("A create_pairing failed: {e}");
            std::process::exit(1);
        }
    };
    let mut cb = ServerClient::new(&server);
    let reg_b = match cb.pair(&email, &code, "device-B") {
        Ok(r) => r,
        Err(e) => {
            eprintln!("B pair failed (code consumed/expired?): {e}");
            std::process::exit(1);
        }
    };

    // Machine-parseable lines for the test scripts. Keep these stable.
    println!("PAIR_EMAIL={email}");
    println!("DEVICE_A_TOKEN={}", reg_a.device_token);
    println!("DEVICE_B_TOKEN={}", reg_b.device_token);

    eprintln!(
        "✓ paired two devices into one library ({email}): A + B tokens issued via the real \
         register → create_pairing → pair flow"
    );
}
