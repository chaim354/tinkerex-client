# Security

The desktop app keeps two device tokens for each account it signs in — one for the Tinkerex
server, one for its sync server — and the mounts put that account's library on the local disk. A
vulnerability here can reach someone's files on every machine they sync. Please report one
privately, not in a public issue or pull request.

## Reporting a vulnerability

Open a private report on GitHub: this repository's **Security** tab, then **Report a
vulnerability**. It reaches the maintainer and nobody else. The other Tinkerex repositories have the
same door — [chaim354/tinkerex](https://github.com/chaim354/tinkerex) (the server and web app),
[chaim354/tinkerex-sync](https://github.com/chaim354/tinkerex-sync) and
[chaim354/tinkerex-addons](https://github.com/chaim354/tinkerex-addons) — so report against the one
that holds the code, and use chaim354/tinkerex when unsure.

Include what you can: the commit, the OS and its version, the mount (WebDAV on macOS, FUSE on
Linux, Cloud Filter API on Windows), the crate, the steps to reproduce, and what an attacker gains.
A proof of concept against a server you run yourself is welcome; please do not test against
`tinkerex.com`, the shared public demo.

Expect an acknowledgement within a week. This is a one-person project with no bug bounty: a fix
lands as an ordinary pull request, the report is credited in it unless you ask otherwise, and the
advisory is published once the fix is out.

## Scope

| in scope | what it is |
|---|---|
| `client/maker-app` | The desktop app (Tauri), including its sign-in and the tokens it stores. |
| `client/sync-engine`, `client/sync-client`, `client/fs-provider` | The daemon, the HTTP client, and the local tree and chunk cache. |
| `client/fs-webdav`, `client/fs-fuse`, `client/fs-cfapi` | The three shipping mounts. |
| `client/fs-winfsp` | The legacy Windows mount, while the app can still be built with it. |

Out of scope, because none of it ships: `client/fs-fskit` (its Swift extension is a stub),
`client/maker-app-core` and `client/juicefs-backend` (reference only), and
`client/test-conformance` (test tooling). A flaw there is an ordinary issue.

One thing is documented behaviour rather than a flaw, and belongs in an ordinary issue if you think
it should change: **an `http://` server address is used exactly as given**, typed or handed to the
app by the server, because the self-hosted LAN deployments answer over plain HTTP. On such a
server the password and the tokens cross the network unencrypted, so use `https://` on any network
you do not trust.

## Supported versions

Only the current `main` branch is supported. There are no releases yet: the app is built from
source.
