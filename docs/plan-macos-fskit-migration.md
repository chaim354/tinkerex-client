# macOS FSKit migration — sketch

> **Kept for the FINDINGS, not as a build plan.** The implementation this sketch describes was
> superseded: `main` carries a different `fs-fskit` (`src/ffi.rs`, `src/mount.rs`,
> `swift/MakerFSExtension.swift`) where the branch this came from had `src/bridge.rs` and
> `swift/MakerFSModule.swift`. What survives is measured: **Phase 0** (§ below) and the
> **Personal Team signing attempt**, which together establish that FSKit's `fsmodule`
> entitlement is a restricted capability a free Personal Team cannot provision — so a paid
> Apple Developer membership is required even for local development. `CLAUDE.md` and
> `client/fs-fskit/BUILD.md` state that conclusion; this is the evidence behind it.

A forward-looking design sketch (NOT an implementation) for moving the macOS mount path from
**WebDAV** (`fs-webdav`, native `mount_webdav`) to **FSKit**, Apple's native, kext-free
filesystem-extension framework. Scoped per `docs/superpowers/specs/2026-06-12-phase2b-ci-fskit-design.md`.
This ends as a committed doc; no code lands from it.

---

## 1. Why move off WebDAV

WebDAV got us a *native, no-kext* macOS mount when FUSE-T's FSKit build couldn't mount on
macOS 26 (silent fail; the System-Settings extension toggle greyed — see `CLAUDE.md` and
`docs/STATUS.md`). It works and is verified live, but it has structural ceilings:

- **HTTP semantics, not filesystem semantics.** `mount_webdav` speaks a chatty HTTP protocol to
  our in-process `dav-server`; metadata timeouts (not active invalidation) drive freshness, so
  the Option-B `invalidate(ino)` seam is a no-op on macOS today (we lean on cache timeouts).
- **No POSIX-mode / xattr fidelity, weaker mtime control** vs a real FS surface (the conformance
  runs already flagged POSIX-perms gaps as deliberate on WebDAV).
- **Not the principled endpoint.** Linux has FUSE, Windows has WinFsp→CfAPI; macOS should have a
  native in-process FS too. FSKit is that — permissive (no GPL kext, satisfies §9), Apple-shipped,
  and the peer of the other two mounts over the *same* `Provider`.

## 2. The key architectural call: FSKit ≈ FUSE, not ≈ CfAPI

This is the most important framing for whoever builds it. macOS offers **two** native models:

| Model | Shape | Our analogue | Used by |
|---|---|---|---|
| **FSKit** (`FSUnaryFileSystem`/`FSVolume`) | A full **userspace filesystem** the kernel mounts; you answer `lookup`/`enumerate`/`read`/`write`/`create`/`remove`/`rename` callbacks | **`fs-fuse`** (in-process FS over `Provider`, hydrate-on-read) | replacing FUSE/NFS-style mounts |
| **File Provider** (`NSFileProviderExtension`) | A **placeholder / on-demand sync** surface (materialize-on-demand, dehydrate) | **`fs-cfapi`** (placeholders, FETCH_DATA, pin/unpin) | iCloud Drive, Dropbox, OneDrive |

The user asked for **FSKit**, so this sketch targets the FUSE-shaped model: a full FS over the
`Provider` with **hydrate-on-read**, NOT a placeholder model. Practically that means
**`fs-fskit` mirrors `fs-fuse` far more than `fs-cfapi`** — the write path is *direct* callbacks
(`createItem`/`removeItem`/`renameItem` → `Provider`), with none of CfAPI's NOTIFY/pre-op-ACK
dance or `delete_by_node_id`/placeholder bookkeeping. The Phase 2b B1–B4 work informs it only by
analogy (the same operations, simpler plumbing). If we later want true on-demand
dehydration/"free up space" on macOS, that's a **File Provider** effort, tracked separately — call
that out so the two don't get conflated.

## 3. Module shape

```
client/fs-fskit/                 # parallels client/fs-fuse
  Cargo.toml                     # cfg(target_os = "macos"); inert empty crate elsewhere
  src/lib.rs                     # Rust side: the FsKitBridge over Arc<SqliteProvider> + Hydrate
  swift/                         # the FSKit extension (Swift) — an .appex bundled in maker-app
    MakerFSExtension.swift       # FSModule / FSUnaryFileSystem / FSVolume conformances
    Bridge.swift                 # @_cdecl Swift ↔ Rust FFI shims
```

- A `sync_engine::FsKitMount` (in `client/sync-engine/src/daemon.rs`, `#[cfg(target_os = "macos")]`)
  wired behind the per-OS `Mount` alias in `client/maker-app/src-tauri/src/lib.rs` — exactly how
  `WebdavMount`/`FuseMount`/`CfApiMount` are selected today. WebDAV stays reachable behind a
  cargo feature for one release (same pattern as the Windows WinFsp→CfAPI flip, Task B5).
- It reuses the **same shared seams** already built: `Arc<SqliteProvider>` (the `impl Provider for
  Arc<P>` from the daemon work), the `Hydrate` hook (chunk-miss → presigned GET → cache → retry),
  the Option-B **real-nanos mtime** (`set_mtime`/`apply_remote` write the source mtime verbatim),
  and the `Invalidate` seam — which on FSKit becomes a **real** active invalidation (see §5),
  unlike the WebDAV no-op.

## 4. The Swift/Rust FFI boundary

FSKit is Swift/ObjC-only; the FS logic stays in Rust (`Provider`). Mirror how `fs-cfapi` bridges
the `windows` crate, but in the opposite direction (Swift calls *into* Rust):

- **Rust exposes a C ABI** (`#[no_mangle] pub extern "C"`) over the `Provider`: `fskit_lookup`,
  `fskit_enumerate`, `fskit_read(ino, off, len, *buf)`, `fskit_write`, `fskit_create`,
  `fskit_remove`, `fskit_rename`, `fskit_getattr`, `fskit_setattr` — each translating FSKit's
  item-identifier model to our `ino`/`node_id`. A leaked `Box<FsKitBridge { provider, hydrate }>`
  handle is passed back and forth as an opaque pointer (the `CfState`/`Box::into_raw` pattern from
  `fs-cfapi`).
- **Swift calls them** from the `FSVolume.Operations` conformances; the extension is packaged as an
  `.appex` with the FSKit entitlement, bundled in the Tauri app and registered with the system.
- Item identity: FSKit uses `FSItem` + 64-bit identifiers — map directly to our `ino` (stable
  per-device), with `node_id` carried for cross-device identity exactly as the other mounts do.

## 5. Hydration + write-capture mapping

- **Read / hydrate:** FSKit `read(_:from:into:)` → `fskit_read` → `Provider::read`; on a
  `ChunkMissing` the bridge runs the **same** `EngineHydrator` (presigned GET) then retries — the
  exact mechanism `fs-fuse` uses. No placeholder state machine.
- **Active invalidation:** a pulled remote change calls the `Invalidate` seam. FSKit can purge the
  kernel cache for an item (the FSKit analogue of FUSE's `notify_inval_inode`, abi-7-12) — so the
  macOS `invalidate(ino)` becomes a real call instead of the WebDAV timeout no-op. This is the
  same primitive slot the Option-B plan reserved (`docs/plan-real-mtime-and-invalidation.md`).
- **Write capture:** direct — FSKit `createItem`/`removeItem`/`renameItem`/`write` → the
  corresponding `Provider` mutation (`create`+`ingest`/`flush`, `unlink`, `rename`) → outbox →
  push. This is the FUSE write path, *not* the CfAPI NOTIFY/ACK path; the B0 primitives
  (`delete_by_node_id` etc.) aren't needed here since FSKit hands us parent+name like FUSE does.

## 6. Verification posture

Like CfAPI, **live FSKit behavior is macOS-only and can't be exercised in a Linux/MSVC CI job**:
- Build/typecheck: the Rust `fs-fskit` crate builds in the existing matrix; the Swift extension
  needs Xcode on the **self-hosted macOS runner** (now available via Track A's `macos` runner).
- A `macos-fskit-smoke` job (the macOS analogue of `windows-smoke`/`mount-conformance`): mount the
  FSKit volume, run the rclone⇄FS + fsx data-integrity suites already in
  `client/test-conformance/` against it, assert a write round-trips to the server. Until that
  exists, the manual smoke (mount in Finder via the app, push/read-back hash-match) is the gate —
  the same bring-up loop CfAPI used.

## 7. Open questions / risks (resolve before committing to build)

1. **The greyed-toggle root cause.** Why FUSE-T's FSKit build couldn't mount on macOS 26 (silent
   fail, greyed System-Settings toggle) must be understood first — it may be an entitlement /
   signing / provisioning issue that bites our own extension identically. **De-risk with a minimal
   "hello-world" FSKit extension that mounts an empty volume before investing in the bridge.**
2. **macOS version floor.** FSKit is recent (Sequoia 15.4+, mature in macOS 26). Setting the floor
   drops older macOS users — keep **WebDAV as the fallback** for them (the cargo-feature gate makes
   this a config choice, not a fork).
3. **Packaging & signing.** The `.appex` needs the FSKit entitlement, a Developer ID, notarization,
   and system registration from inside a Tauri app bundle — non-trivial and not yet proven in our
   build. Spike this early.
4. **FSKit vs File Provider, deliberately.** If "Always keep on this device / Free up space"
   (true dehydration) becomes a macOS requirement, FSKit is the wrong tool — that's a File Provider
   project. Decide up front that FSKit = "native full mount" and dehydration-on-macOS is out of
   scope (or a separate later track), so the goalposts don't move mid-build.
5. **Stability to ship.** Confirm FSKit is production-stable for a third-party FS (Apple positioned
   it as the FUSE successor, but real-world third-party deployments are still young). If not, the
   WebDAV fallback stays the shipping default until it is.

**Bottom line:** FSKit is the right principled macOS endpoint and slots cleanly over the existing
`Provider` + hydrate + invalidate + Option-B-mtime seams as a FUSE-shaped mount. The work is
mostly the **Swift extension + FFI bridge + packaging/signing**, not new sync logic. Gate the
build on de-risking the greyed-toggle/signing issues (#1, #3) with a hello-world extension first,
and keep WebDAV behind a feature flag as the fallback.

---

## Phase 0 findings (2026-06-14) — DE-RISK RESULT: BLOCKED on Apple code-signing/provisioning

A hello-world FSKit extension was built, signed, and registered on the real build Mac
(macOS 26.3.1, build 25D2128; Xcode 26.5 / Swift 6.2.1). **The de-risk reproduced the plan's
open-question #1 (the "greyed toggle") and traced it to a hard root cause: the FSKit module
entitlement is provisioning-gated and the machine has no Apple Developer signing identity.**
The Swift+Info.plist+entitlement recipe is fully worked out and the Rust/Swift bridge is
authored and compiles; what's missing is *signing*, which is an account/infra problem, not a
code problem.

### What was verified live (commands + output)

Environment:
- `sw_vers` → macOS **26.3.1** (25D2128). `swift --version` → **Apple Swift 6.2.1**.
  `xcodebuild -version` → **Xcode 26.5** (after pointing `DEVELOPER_DIR` at
  `/Applications/Xcode.app`; the active dir defaulted to CommandLineTools).
- FSKit SDK present with full headers (`…/MacOSX.sdk/System/Library/Frameworks/FSKit.framework/Headers/`:
  `FSUnaryFileSystem.h`, `FSVolume.h`, `FSItem.h`, …). `/usr/libexec/fskitd` present.
- **`security find-identity -v -p codesigning` → `0 valid identities found`.** No Apple
  Developer account is configured in Xcode (`IDEProvisioningTeams` does not exist; no
  `~/Library/Developer/Xcode/UserData/Provisioning Profiles/`).

A reference third-party FSKit module is already installed on this box — **FUSE-T's**
`org.fuset.fskit-srv.module` at `/Applications/fuse-t.app/Contents/Extensions/FskitSrvModule.appex`.
Dissecting it gave the exact production recipe (and the contrast that proves the blocker):
- `CFBundlePackageType = XPC!`, `EXAppExtensionAttributes.EXExtensionPointIdentifier =
  com.apple.fskit.fsmodule`, an `FSShortName` (the `-t` mount type), `NSExtensionPrincipalClass`.
- Signed with a **real Developer-ID team `6DY7Z4SVDZ`**, hardened runtime (`flags=0x10000(runtime)`),
  app-sandbox, an app-group, and it carries an **`embedded.provisionprofile`** (22 KB).
- Entitlements include the restricted **`com.apple.developer.fskit.fsmodule = true`** *plus*
  `com.apple.application-identifier = 6DY7Z4SVDZ.org.fuset.fskit-srv.module` and
  `com.apple.developer.team-identifier = 6DY7Z4SVDZ`.

The hello-world module (`client/fs-fskit/swift/hello-world/`, built by `build-helloapp.sh`):
- Compiles + links clean against the macOS 26.5 SDK (`swiftc … -framework FSKit`) — the blind-
  authored `FSUnaryFileSystem`/`FSVolume`/`FSVolume.Operations`/`ReadWriteOperations`/
  `OpenCloseOperations`/`PathConfOperations` conformances are **API-correct** for this SDK.
  (Entry point: a `main.swift` that calls `NSExtensionMain` via `@_silgen_name`, since the Swift
  overlay doesn't surface it; `NSExtensionPrincipalClass` in the appex Info.plist names the class.)
- Packaged as `MakerHelloFS.app/Contents/PlugIns/FskitHello.appex`, **ad-hoc** signed
  (`codesign -s -`) with `com.apple.developer.fskit.fsmodule` + app-sandbox, copied to
  `/Applications`.
- **`codesign` embeds the restricted entitlement without complaint** (it doesn't validate
  restricted entitlements at sign time — `amfi`/`fskitd` validate them at *launch*).
- After `pluginkit -a …` + `lsregister -f …`, **the module IS discovered**:
  `pluginkit -m -p com.apple.fskit.fsmodule` lists `com.maker.fskit.hello.module`, and it appears
  in `~/Library/Group Containers/group.com.apple.fskit.settings/probeOrder.plist`. So **registration
  works with ad-hoc signing** — discovery is not the gate.
- **Mounting fails at the enablement gate.** `mount -F -t makerhello none /tmp/hellomnt` →
  **`Module com.maker.fskit.hello.module is disabled!`** / `mount: Unable to invoke task`.
- The gate is `~/Library/Group Containers/group.com.apple.fskit.settings/**enabledModules.plist**`:
  it lists `org.fuset.fskit-srv.module` (FUSE-T) and the Apple built-ins (`apfs`, `exfat`, `msdos`,
  `ntfs`, `ftp`) but **not** our module. `fskitd` exposes the writer as the private XPC
  `setEnabledStateForIdentifier:newState:` (strings in `/usr/libexec/fskitd`), driven **only** by
  the System Settings UI ("General → Login Items & Extensions → File System Extensions").
- **Direct contrast that isolates the cause:** the *enabled, Developer-ID* FUSE-T module fails a
  bare mount with **`Probing resource: Permission denied`** — i.e. `fskitd` *launched its
  extension* and the extension then declined the empty resource. Our *ad-hoc* module never gets
  that far: `fskitd` refuses to start it at all (`disabled`). The differentiator is the signature/
  provisioning: FUSE-T has a Team ID + an embedded provisioning profile authorizing
  `com.apple.developer.fskit.fsmodule`; the ad-hoc build has neither (`TeamIdentifier=not set`,
  `flags=0x2(adhoc)`, no profile).

(The System Settings toggle itself couldn't be inspected via computer-use this session — the
access-approval dialog timed out, the box being unattended. Writing `enabledModules.plist`
directly to force-enable was deliberately **not** done: it's the OS's filesystem-extension
approval control, and the harness's safety classifier correctly blocks bypassing it. The plist was
left untouched; the test app + its containers were removed and the system returned to baseline.)

### Conclusion

FSKit **can** register and is API-reachable on this machine, but a third-party module **cannot be
enabled/mounted without an Apple-issued provisioning profile** that grants the restricted
`com.apple.developer.fskit.fsmodule` entitlement, signed under a real Team ID. That requires:
1. a **paid Apple Developer account** (the `fskit.fsmodule` entitlement is a *managed/restricted*
   capability — it is not in the free "Personal Team" allowlist), and
2. an App ID with that capability enabled + a matching provisioning profile embedded in the
   `.appex`, with the host app signed Developer-ID + notarized.

This is exactly the same wall FUSE-T's own FSKit build hit ("greyed toggle"): the toggle is greyed
/ the module stays "disabled" unless the extension is properly provisioned. **It is an
account/infra blocker, not a code or API blocker** — and it is unsolvable from this environment as
configured (no signing identity, no account, and the enablement step is GUI-gated + a protected OS
control). Per the de-risk instruction, the build stops here rather than looping against it.

### What is nonetheless delivered (ready the moment a Developer ID + FSKit entitlement exist)

All of this builds + tests on the macOS box and is committed on the worktree branch.

1. **`client/fs-fskit/swift/hello-world/`** — the working hello-world FSKit module (HelloFS.swift,
   HelloFSExtension.swift, main.swift) + `build-helloapp.sh` that compiles, bundles, ad-hoc-signs,
   and stages it. **Proven to compile + register** (it appears in `pluginkit -m -p
   com.apple.fskit.fsmodule` + `probeOrder.plist`); the *only* missing piece to mount is signing.
2. **`client/fs-fskit/` Rust crate (Phase 1)** — the C-ABI bridge over `Arc<SqliteProvider>` + a
   `Hydrate` hook (`fskit_lookup`/`getattr`/`readdir`/`read`/`write`/`flush`/`create`/`mkdir`/
   `remove`/`rename`/`setattr`/`open`, an opaque `Box<FsKitBridge>` handle via `Box::into_raw`),
   plus `include/fs_fskit.h`. `#[cfg(target_os = "macos")]`, inert elsewhere (the
   `fs-winfsp`/`fs-cfapi` pattern). **9 unit tests green** (create/write/read round-trip *through
   the ABI*, lookup ENOENT, mkdir+remove dispatch, truncate-no-stale-tail, readdir, hydrate-on-miss
   fires). `staticlib`+`rlib`. Built against **this branch's** `Provider` surface (no `mode` column;
   `mtime` in seconds; no lock manager) — the richer branches' additions are noted inline as
   forward-compat seams.
3. **`client/fs-fskit/swift/{MakerFSModule,MakerFSVolume,Bridge,main}.swift` + `module.modulemap`
   (Phase 2)** — the production Swift FSKit extension that calls the Rust C-ABI (`import
   FsKitBridge`), mirroring the hello-world plumbing but backed by the real `Provider`: read carries
   chunk-miss→hydrate→retry, `closeItem` flushes (writeback→outbox→push), `enumerateDirectory` packs
   the `Provider`'s `readdir` honoring the FSKit cookie. **All three sources typecheck clean against
   the macOS 26.5 FSKit SDK + the C module.**
4. **`sync_engine::FsKitMount` + `fskit_glue::fskit_bootstrap` (Phase 3)** — `FsKitMount`
   (`cfg(all(macos, feature="fskit"))`) reuses the shared `sync_loop`/`nudge_loop` and has the same
   `start`/`stop`/`is_running`/`mountpoint` shape as `WebdavMount`; it pre-flights module
   registration (`pluginkit`) and **fails fast** with an actionable message instead of letting
   `mount -F` hang. `fskit_bootstrap` is the `#[no_mangle]` C entry the `.appex` calls
   (`@_silgen_name`) to open its own `Provider` + presigned-GET hydrator (it lives in `sync-engine`
   because hydration needs `sync-client`). 2 glue unit tests. Behind the **`fskit` cargo feature**
   (OFF by default → **WebDAV stays the default macOS backend** and the normal build is unchanged).
   `examples/fskit_live.rs` is the FSKit analogue of `webdav_live.rs`.
5. **`client/fs-fskit/BUILD.md`** — the Xcode/codesign/notarize/enable steps, including the exact
   entitlements + Info.plist keys reverse-engineered from FUSE-T, and what an Apple Developer
   account must provision.

**Parity status:** NOT achieved end-to-end — a live write→server→read-back round-trip through an
FSKit mount is impossible here without signing. But the **full path is wired and was driven live**:
against a local memory server, `examples/fskit_live` registers a device, starts the sync loop, then
`FsKitMount::start` reaches exactly the registration/signing gate and fails cleanly (process exits,
no hang) with the BUILD.md pointer. This is the same authored-and-compiles posture `fs-winfsp` /
`fs-cfapi` first landed in (runtime-pending on hardware/credentials they couldn't reach) — here the
missing credential is a paid Apple Developer account + the FSKit provisioning profile.

---

## Signing attempt — Personal Team (2026-06-14) — DEFINITIVE: paid Apple Developer Program REQUIRED

A signing identity has since been installed on the build Mac, so the Phase-0 hypothesis (paid
account required) was tested **directly** against Apple's provisioning portal — not assumed.

### Environment
- macOS **26.3.1** (25D2128), **Xcode 26.5** (Build 17F42), active dev dir `/Applications/Xcode.app`.
- `security find-identity -v -p codesigning` → **1 valid identity**:
  `C0F22849F4AA3241DC23BEEB4464CF577EB00D3F "Apple Development: marvin schwartz (7LA92R64RW)"`.
- Team ID **`ERVD4F8593`** — a **free "Personal Team"** (not a paid Apple Developer Program
  membership). No provisioning profiles staged; the account is signed into Xcode (automatic
  provisioning reached the portal, see below).

### The test (minimal Xcode project, automatic provisioning)
Built a minimal two-target Xcode project (a host `.app` + an `app-extension` `.appex` target). The
appex's `Info.plist` carries the FSKit keys (`EXExtensionPointIdentifier = com.apple.fskit.fsmodule`,
`FSShortName`, `NSExtensionPrincipalClass`) and its entitlements request the restricted
`com.apple.developer.fskit.fsmodule = true` + `com.apple.security.app-sandbox`. Then forced the
provisioning system to mint/fetch a matching profile:

```
xcodebuild -project MakerFSProv.xcodeproj -target MakerFSExt -configuration Debug \
  -allowProvisioningUpdates DEVELOPMENT_TEAM=ERVD4F8593 CODE_SIGN_STYLE=Automatic build
```

**Result — the portal explicitly refused the capability (reproducible across runs):**

```
error: Cannot create a Mac App Development provisioning profile for
"com.maker.fskitprov.host.fsmodule". Personal development teams, including "marvin schwartz",
do not support the FSKit Module capability. (in target 'MakerFSExt')
error: No profiles for 'com.maker.fskitprov.host.fsmodule' were found: Xcode couldn't find any
Mac App Development provisioning profiles matching 'com.maker.fskitprov.host.fsmodule'.
** BUILD FAILED **
```

### Control that isolates the FSKit capability as the *sole* blocker
The **same appex target, same identity, same Personal Team, same automatic-provisioning flow**, with
**only** the `com.apple.developer.fskit.fsmodule` key removed from the entitlements (leaving
`app-sandbox`), **builds and code-signs successfully**:

```
CodeSign .../MakerFSExt.appex
    Signing Identity: "Apple Development: marvin schwartz (7LA92R64RW)"
    /usr/bin/codesign --force --sign C0F22849F4AA3241DC23BEEB4464CF577EB00D3F ...
** BUILD SUCCEEDED **
```

So the signing identity, the connected account, automatic provisioning, and app-extension packaging
all work. The **one and only** thing that fails is the restricted FSKit Module capability, which
Apple's portal will not provision for a Personal Team. (The scratch project was created under
`/tmp`, used only to elicit the error, and deleted; nothing in the repo or the OS enablement state
was touched — `enabledModules.plist` / the System-Settings toggle were left alone, as in Phase 0.)

### Definitive conclusion
**The `com.apple.developer.fskit.fsmodule` entitlement does NOT provision on this free Personal Team
(Team ID `ERVD4F8593`).** Apple's provisioning service refuses it by name: *"Personal development
teams … do not support the FSKit Module capability."* This is a **managed/restricted capability that
requires a paid Apple Developer Program membership** ($99/yr). With a paid membership the path is
unblocked: enroll → create an App ID with the **FSKit Module** capability → automatic provisioning
will then mint the profile → sign the host app + `.appex` (Developer ID, hardened runtime) →
notarize → the System Settings → Login Items & Extensions → File System Extensions toggle becomes
enable-able. **No code change is required** — the bridge, Swift extension, and `FsKitMount` are all
authored-and-compiling and were re-confirmed unchanged; the wall is purely the account tier.
