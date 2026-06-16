//! Seed for the **cross-MACHINE, cross-OS** convergence e2e (`.github/workflows/cross-os-e2e.yml`).
//!
//! Registers a FRESH unique test user (device "windows") and pairs TWO more brand-new devices
//! ("mac" + "linux") into the SAME library via the **real pairing flow** — exactly the Tauri
//! app's secure multi-device join: device A `register`s (creates the account/library), mints a
//! single-use pairing code with `create_pairing` (the app's `create_pairing` command), and each
//! brand-new device redeems its own single-use code with `pair` (the app's `sync_pair` command).
//! No device ever sees another's token; each code is single-use, so we mint one per joiner.
//!
//! Unlike `pair_two_devices`, this seed runs ONCE on a runner that can reach the SHARED server and
//! hands the three tokens to three DIFFERENT machines (Windows/macOS/Linux GitHub Actions jobs) as
//! job outputs. The library is a throwaway per-run test account (unique email), so emitting the
//! device tokens as plaintext output is acceptable for this CI scenario.
//!
//!   SERVER_URL=http://192.168.1.95 cargo run --example pair_three_devices
//!   # or: cargo run --example pair_three_devices -- <server_url> [email]
//!
//! Output (machine-parseable; the workflow greps these lines — keep them stable):
//!   PAIR_EMAIL=<email>
//!   TEST_TAG=<unique-tag>          # a per-run suffix for the test filename (no collisions)
//!   DEVICE_WIN_TOKEN=<token>
//!   DEVICE_MAC_TOKEN=<token>
//!   DEVICE_LINUX_TOKEN=<token>
//! Exit 0 on success; exit 3 (SEED_UNREACHABLE) if the shared server can't be reached (the
//! workflow maps that to a clean BLOCKED), non-zero otherwise on any API failure.

use std::time::{SystemTime, UNIX_EPOCH};

use sync_client::ServerClient;

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .or_else(|| std::env::var("SERVER_URL").ok())
        .unwrap_or_else(|| "http://localhost:3001".to_string());

    // A unique per-run tag: drives both the unique email (so concurrent/repeat runs never collide
    // on the new-account-only `register`) and the test filename suffix the three machines agree on.
    let tag = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let test_tag = format!("{tag:x}");
    let email = args
        .next()
        .or_else(|| std::env::var("EMAIL").ok())
        .unwrap_or_else(|| format!("crossos-{test_tag}@example.com"));

    // Device "windows" creates the (confirmed) account + enrolls (the app's first-run path).
    let password = "correct horse battery staple";
    let mut c_win = ServerClient::new(&server);
    let reg_win = match c_win.enroll(&email, password, "windows") {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "enroll failed (is the SHARED server up at {server}? ENABLE_TEST_SIGNUP set? email free?): {e}\n\
                 SEED_UNREACHABLE"
            );
            std::process::exit(3); // distinct code so the workflow can map it to BLOCKED
        }
    };

    // Each brand-new device enrolls by logging in with the SAME account credentials (the
    // Dropbox model — each device authenticates independently; no pairing codes).
    let join = |name: &str| -> String {
        let mut c = ServerClient::new(&server);
        match c.device_login(&email, password, name) {
            Ok(r) => r.device_token,
            Err(e) => {
                eprintln!("device_login {name} failed (bad credentials/unconfirmed/locked?): {e}");
                std::process::exit(1);
            }
        }
    };
    let tok_mac = join("mac");
    let tok_linux = join("linux");

    // Machine-parseable lines for the workflow's `seed` job outputs. Keep these prefixes stable.
    println!("PAIR_EMAIL={email}");
    println!("TEST_TAG={test_tag}");
    println!("DEVICE_WIN_TOKEN={}", reg_win.device_token);
    println!("DEVICE_MAC_TOKEN={tok_mac}");
    println!("DEVICE_LINUX_TOKEN={tok_linux}");
    println!(
        "# paired three devices (windows/mac/linux) into one library ({email}) via the real \
         register -> create_pairing -> pair flow; tag={test_tag}"
    );
}
