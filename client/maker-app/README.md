# Tinkerex desktop app

The desktop half of [Tinkerex](https://github.com/chaim354/tinkerex): sign in with the account you
use on the web, and a folder on this computer stays in sync with your Tinkerex library. It is
local-first: browsing the folder never waits on the network, a file's contents download when you
open it, and every save is pushed back to the server.

It is a [Tauri 2](https://tauri.app) app: a plain-HTML frontend (`src/`, no npm build step) over a
Rust backend (`src-tauri/`) that runs the sync engine and the mount in-process.

| OS | mount | before you build |
|---|---|---|
| macOS | WebDAV, through the built-in `mount_webdav` | nothing beyond Xcode's command-line tools, Rust and the Tauri CLI |
| Linux | FUSE | Tauri's system libraries, libfuse, and `user_allow_other` in `/etc/fuse.conf` |
| Windows | Cloud Filter API: a sync-root folder, no driver | LLVM/libclang; run it from an interactive login |

[SETUP.md](SETUP.md) has the per-OS detail: the Linux package list, the Windows build, and
troubleshooting.

## Run it

You need [Rust](https://rustup.rs) and the Tauri CLI
(`cargo install tauri-cli --version "^2.0" --locked`). Then:

```sh
cd client/maker-app
cargo tauri dev
```

In the window, fill in:

- **Tinkerex server**: the address of the Tinkerex web app you sign in to.
- **Account email** and **password**: the account you use on the web.
- **Mount point**: the folder to sync into, `~/Tinkerex` by default (`%USERPROFILE%\Tinkerex` on
  Windows).

The server answers the sign-in with a grant for its sync server, and the app syncs against that
directly. A server that keeps its files in a shared folder or a storage bucket (its
`LIBRARY_BACKEND` is `share` or `s3`) has nothing for this app to sync: it says so in one
sentence, and nothing is mounted.

## Where credentials live

Your password is sent once and never stored. It travels over whatever the server's address uses,
so use an `https://` address where you can. What the app keeps is its own device tokens, in
`credentials.json` under its data directory:

| OS | directory |
|---|---|
| macOS | `~/Library/Application Support/maker-app/` |
| Linux | `~/.local/share/maker-app/` |
| Windows | `%LOCALAPPDATA%\maker-app\` |

The file is owner-only (0600, in a 0700 directory) and written atomically. Each entry is keyed by
the server address you typed and your email, and holds this device's token for the Tinkerex
server and the sync server's address and token from the grant:

```json
{ "maker_token": "…", "sync_url": "https://sync.example.com", "sync_token": "…" }
```

An older entry that is a bare string still works, unchanged: it is a sync token for the address
typed with it. The same directory holds `prefs.json` and one `acct-…` folder per account (its local
tree and chunk cache). The directory is still called `maker-app`: renaming it would move every
account's state on disk, and that migration has been deferred.

To make this device sign in from scratch, quit the app and delete that account's entry from
`credentials.json`. That forgets the tokens on this machine; it does not revoke them on the
servers.

## Tests

```sh
(cd client/maker-app/src-tauri && cargo test)   # credentials, sign-in decisions, prefs
node --test client/maker-app/test               # from the repo root: account identity, the form
```

No CI job runs either today: `.github/workflows/rust-client.yml` does not build this crate, and no
workflow runs the JavaScript tests.

## Not set up yet

There are no signed builds and no releases: no Apple Developer ID or Windows code-signing
certificate, no notarization, no published installer, and no auto-updater.
`.github/workflows/build-app.yml` can bundle unsigned installers on a self-hosted runner for each
OS; Gatekeeper and SmartScreen will warn about them.
