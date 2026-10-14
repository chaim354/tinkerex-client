# tinkerex-client

The client half of [Tinkerex](https://github.com/chaim354/tinkerex): Rust crates that keep a
local-first copy of a Tinkerex library in sync with its sync server, and the desktop app built on
them.

| path | what it is |
|---|---|
| `client/maker-app` | **The desktop app** (Tauri): signs in to a Tinkerex server and syncs a folder with its library. Start at its [README](client/maker-app/README.md). |
| `client/sync-engine` | The sync daemon (push, pull, hydrate) and the per-OS mount it runs. |
| `client/sync-client` | The HTTP client: the sync server's v1 API, and the Tinkerex server's sign-in and `/v1/maker/*` API. |
| `client/fs-provider` | The local SQLite tree and content-addressed chunk cache every mount reads from, so browsing never waits on the network. |
| `client/fs-webdav`, `client/fs-fuse`, `client/fs-cfapi` | The mounts: macOS (WebDAV), Linux (FUSE), Windows (Cloud Filter API). |
| `client/fs-winfsp`, `client/fs-fskit` | A legacy Windows mount, and a macOS FSKit bridge that does not ship. |
| `client/maker-app-core`, `client/juicefs-backend` | Leftovers from an abandoned design, kept for reference. |
| `client/test-conformance` | External tools (rclone, fsx, pjdfstest) driven against the real mounts. |

## How it fits together

- **[chaim354/tinkerex](https://github.com/chaim354/tinkerex)** is the Tinkerex server: the web app
  for projects, inventory and the model library. The desktop app signs in there, with the account
  you use on the web, and its answer carries a grant for the sync server.
- **[chaim354/tinkerex-sync](https://github.com/chaim354/tinkerex-sync)** is the sync server:
  versions, the changelog and chunk storage. With that grant, the app syncs against it directly.

The wire between them is specified in `docs/sync-protocol.md` in chaim354/tinkerex.

## Build and test

There is no Cargo workspace, so every crate builds on its own: `cd client/<crate> && cargo test`.
[CLAUDE.md](CLAUDE.md) is the cheatsheet: the commands, the per-OS gotchas, and what CI runs.

## License

Apache-2.0. See [LICENSE](LICENSE).
