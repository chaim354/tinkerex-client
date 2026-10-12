# Building / running the FSKit mount (macOS) — and the when-the-account-arrives checklist

`fs-fskit` is the **native, kext-free macOS mount** over the same `Provider` — the
principled peer of `fs-fuse` (Linux FUSE) and `fs-cfapi` (Windows Cloud Filter). It uses
Apple's **FSKit** framework (`FSUnaryFileSystem`/`FSVolume`), the FUSE successor: a full
userspace filesystem the kernel mounts, with **hydrate-on-read** baked into the Rust bridge.
See `docs/plan-macos-fskit-migration.md` for the design and the FSKit-vs-File-Provider call.

> **Status — SKELETON. The live mount is account-gated and does NOT build/run yet.**
> The Rust side that *can* build *does*: the cross-platform pure logic and the FFI bridge
> are written + **unit-tested** (`cargo test -p fs-fskit --features ffi`, 23 tests, run on
> any OS). The Swift `.appex`, its `Info.plist`/entitlements, signing, notarization, and
> system registration need the **FSKit SDK + a paid Apple Developer account** and are
> **stubbed** with `TODO(dev-account)` markers (`swift/`). `FsKitMount::start` returns
> `MountError::NeedsDeveloperAccount` today. **macOS ships WebDAV (`fs-webdav`) until this
> lands** — keep it behind a cargo feature as the fallback (the same WinFsp→CfAPI pattern).

## What compiles + is tested today (no account needed)

```sh
# Pure logic + FFI bridge dispatch, against an in-memory SqliteProvider (no Swift/FSKit):
cargo test --manifest-path client/fs-fskit/Cargo.toml --features ffi

# Default (pure logic only — ItemId<->ino, item_type, decide_change):
cargo test --manifest-path client/fs-fskit/Cargo.toml
```

- **`src/lib.rs`** — `ItemId` ↔ `ino` mapping (root pinned to `ROOT_INO`, `0` = invalid
  sentinel), `item_type(Kind)`, and `decide_change(RemoteNodeEvent) -> FsKitChange` (the
  active-invalidation decision: data vs metadata vs removed vs none — the FSKit analogue of
  FUSE `notify_inval_inode` / CfAPI `PlaceholderAction`).
- **`src/ffi.rs`** (`--features ffi`) — `FsKitBridge` over `Arc<SqliteProvider>` + an
  optional `Hydrate` hook; dispatches every FSKit op to the `Provider`
  (`lookup`/`getattr`/`enumerate`/`read`-with-hydrate-on-miss-retry/`create`/`mkdir`/`write`/
  `flush`/`truncate`/`unlink`/`rmdir`/`rename`), an `errno()` mapping, and the
  `into_raw`/`from_raw`/`free_raw` opaque-handle lifecycle. Sketched `#[no_mangle] extern
  "C"` shims (`fskit_lookup`/`_read`/`_free`) use plain C scalars + byte buffers.
- **`src/mount.rs`** — `FsKitMount` lifecycle shape (`start`/`is_running`/`stop`/`Drop`)
  mirroring `WebdavMount`/`CfApiMount`. `start` builds the shared provider, then refuses.

## What is stubbed (needs the account + SDK)

Everything under **`swift/`**, all marked `TODO(dev-account)`:
- `MakerFSExtension.swift` — `FSUnaryFileSystem` + `FSVolume.Operations` conformances.
- `Bridge.swift` — `@_silgen_name` decls of the Rust `fskit_*` C ABI + the `RustBridge` wrapper.
- `Info.plist` — extension-point + principal-class declaration.
- `MakerFS.entitlements` — FSKit + App Sandbox entitlements.

---

# CHECKLIST — when the paid Apple Developer account arrives

Do these roughly in order. **Step 0 first** — it de-risks the whole effort.

### 0. De-risk the greyed-toggle / signing issue with a hello-world extension
FUSE-T's FSKit build couldn't mount on macOS 26 (silent fail; the System-Settings extension
toggle was greyed — see `docs/plan-macos-fskit-migration.md` §7.1). That is most likely an
**entitlement/signing/provisioning** problem that would bite our extension identically.
**Before touching the bridge**, build the minimal Apple "FSKitSample"/hello-world extension
(or a trivial one of our own) that mounts an **empty** read-only volume, sign it with the new
Developer ID, and confirm it (a) appears + can be enabled in System Settings ▸ General ▸
Login Items & Extensions ▸ File System Extensions, and (b) actually mounts. If it doesn't,
fix signing/provisioning here — not against our full code.

### 1. Install the toolchain
- **Xcode** (the version that ships the **FSKit** SDK — Sequoia 15.4+ / macOS 26 era) on the
  self-hosted macOS runner + your dev machine.
- Confirm `import FSKit` resolves in a scratch Swift file.

### 2. Set your real identifiers + signing
- Replace every `TODO(dev-account)` placeholder in `swift/Info.plist` +
  `swift/MakerFS.entitlements`:
  - `CFBundleIdentifier` → your reverse-DNS id under your Team (e.g. `com.<you>.maker.MakerFS`).
  - the FSKit extension-point id + the `com.apple.developer.fskit.fsmodule` entitlement key —
    **confirm the exact strings against the installed SDK** (the stubs are best-guess).
  - the app group (`group.com.<you>.maker`) for host-app ↔ extension XPC.
- In your provisioning profile, **request/authorize the FSKit entitlement** (it is privileged).
- Set the Developer ID signing identity for the `.appex` and the host app.

### 3. Create the Xcode targets
- A **File System Extension** target (the `.appex`) using `swift/*.swift` + `Info.plist` +
  `MakerFS.entitlements`.
- A host **app** target that embeds the `.appex` (extensions ship inside an app). The Tauri
  `client/maker-app` bundle is the natural host — embed the `.appex` in its `.app`.
- Link the **Rust staticlib**: add a `crate-type = ["staticlib"]` (or `cdylib`) target to
  `client/fs-fskit/Cargo.toml`, build it with `--features ffi` for the mac target(s)
  (`aarch64-apple-darwin`, `x86_64-apple-darwin`), and add the resulting `.a`/`.dylib` +
  a bridging header to the extension target so the `@_silgen_name` `fskit_*` symbols resolve.

### 4. Finish the FFI surface (Rust + Swift together)
- Upgrade `src/ffi.rs`'s sketched scalar/buffer shims to the **FSKit-struct-typed**
  signatures (FSItem identifiers, the reply-buffer/`replyHandler` shapes) now that the SDK
  headers are in hand. Add the missing shims: `fskit_getattr`, `fskit_enumerate`,
  `fskit_create`, `fskit_mkdir`, `fskit_write`, `fskit_flush`, `fskit_truncate`,
  `fskit_unlink`, `fskit_rmdir`, `fskit_rename`. Each just calls the matching `FsKitBridge`
  method (already real + tested) and translates the `Result` via `errno()`.
- Fill in the `swift/Bridge.swift` decls + wrappers to match, and the
  `MakerFSExtension.swift` `FSVolume.Operations` methods (the comment block is the map).

### 5. Wire into the engine (mirror the other mounts)
- In `client/sync-engine/src/daemon.rs`, add an `FsKitMount` (copy `WebdavMount`: build the
  shared `Arc<SqliteProvider>` + `bind_library`, spawn `sync_loop` + `nudge_loop`, build the
  hydrate closure). Replace `fs_fskit::FsKitMount::start`'s `NeedsDeveloperAccount` early
  return with: build `ffi::FsKitBridge` → `into_raw` → hand the pointer to the registered
  `.appex` (via XPC/app-group) → return the live `Self`. On `stop`/`Drop`: signal the
  extension to unmount, then `free_raw` + join the threads.
- Export it from `sync-engine`'s `lib.rs` behind `#[cfg(target_os = "macos")]`, and in
  `client/maker-app/src-tauri/src/lib.rs` flip the macOS `Mount` alias from
  `sync_engine::WebdavMount` to `sync_engine::FsKitMount` **behind a cargo feature**
  (`fskit-backend`), keeping WebDAV as the default/fallback for one release + for macOS
  versions below the FSKit floor.
- Wire **active invalidation**: on a pulled change the engine computes
  `fs_fskit::decide_change(...)` and signals the extension to purge the kernel cache for the
  item (the FSKit analogue of FUSE `notify_inval_inode`) — making macOS `invalidate(ino)` a
  real call instead of the WebDAV timeout no-op.

### 6. Register + run the extension
- Build + sign the host app embedding the `.appex`. Launch it once so macOS discovers the
  extension, then enable it in **System Settings ▸ General ▸ Login Items & Extensions ▸ File
  System Extensions**.
- Notarize for distribution (`notarytool`) once it works locally.

### 7. Smoke-test against the deployed server
You need a running sync server + a device token (register via the API or the Maker app).
Bring up the FSKit volume (via the app, or a `maker-fskitd` binary added in step 5) pointed
at the deployed Rails server, then verify — the **same checks the CfAPI smoke uses**:

1. **Populate.** The library tree appears at the mount point in Finder; folders, filenames,
   sizes, and **modification dates** match the server (real mtime is synced end-to-end).
2. **Hydrate on read.** Open a file; the bytes are fetched on demand (the Rust bridge's
   hydrate-on-miss → presigned GET → chunk cache → retry). Confirm a **hash match** against
   the source on another device.
3. **Write capture.** Create/edit/delete/rename a file in the mount; confirm it pushes and
   appears on another device (the direct `Provider` write path — no NOTIFY/ACK dance).
4. **Remote-change invalidation.** Edit the file on another device; within a sync tick the
   kernel cache is purged and the next read shows the **new** content (the `decide_change`
   active invalidation).
5. **Teardown.** Unmount via the app; the volume disappears and the bridge is freed.

Then run the **external conformance** suites already in `client/test-conformance/` against
the FSKit mount (the macOS analogue of the FUSE/WebDAV runs): `rclone` round-trip + `fsx`
data-integrity. Add a `macos-fskit-smoke` CI job on the self-hosted macOS runner (the peer of
`windows-smoke` / `mount-conformance`) that mounts the volume + asserts a write round-trips to
the server.

### 8. Decide the macOS version floor + keep WebDAV
FSKit is recent (Sequoia 15.4+, mature in macOS 26). Setting the floor drops older users, so
**keep WebDAV as the fallback** behind the `fskit-backend` feature gate (a config choice, not
a fork) until FSKit's third-party stability is proven on the deployment base.

---

## VM verification of the signing gate (2026-06-14) — why the checklist above needs a PAID account

> Recovered from the `fskit-vm-residency-investigation` branch, whose *implementation* was
> superseded by the one in this crate. The finding below was measured, not assumed, and it is
> the evidence behind this file's "when the paid Apple Developer account arrives" framing and
> CLAUDE.md's "stubbed pending a paid Apple Developer membership". The scripts it names live in
> `swift/hello-world/`. The companion portal test — Apple's provisioning portal refusing the
> capability outright for a free Personal Team — is in `docs/plan-macos-fskit-migration.md`.


The Phase-0 finding (§5 above, and `docs/plan-macos-fskit-migration.md`) was that an ad-hoc
module **registers** but can't be **enabled/mounted** without an Apple-provisioned
`com.apple.developer.fskit.fsmodule` entitlement — the System-Settings toggle stays greyed and
`mount -F` reports **"Module … is disabled!"**. To test how far we can get *without* a paid
Apple Developer membership, the hello-world module was driven on a **throwaway Tart macOS 26.5
VM** with **AMFI disabled** (`nvram boot-args=amfi_get_out_of_my_way=1`) and **SIP disabled**.
Scripts: `swift/hello-world/vm-bringup.sh` (install → register → force-enable → mount) and
`vm-restore.sh` (return to baseline). These are **DEV-ONLY on a disposable VM you own** — they
write fskitd's enablement plist directly, which on a real host is the OS's protected
filesystem-extension approval control. Never run them against a host.

**Result — the Phase-0 enablement wall IS defeated with AMFI off, but a second wall appears.**

1. **Registration** (ad-hoc): `pluginkit -m -p com.apple.fskit.fsmodule` lists
   `com.maker.fskit.hello.module` (a `+` = enabled). `codesign -dv` shows `Signature=adhoc`,
   `TeamIdentifier=not set`, with `com.apple.developer.fskit.fsmodule` embedded. ✅
2. **Enablement**: fskitd reads `~/Library/Group Containers/group.com.apple.fskit.settings/
   enabledModules.plist` (a flat array of bundle ids; there's a per-user **and** a
   `/var/root/…` copy — write both). Adding our id + `killall fskitd` flips `mount -F` from
   **"Module … is disabled!"** to actually **launching the extension**. With AMFI off the
   restricted FSKit entitlement claim is **honored ad-hoc** — no provisioning profile needed.
   This is the key result: *the greyed-toggle / "disabled" wall is an AMFI-enforced
   entitlement check, and it is the only thing the paid membership was buying for enablement.* ✅
3. **Launch + FSKit handshake**: fskitd starts the `.appex` process; FSKit accepts the
   delegate — the log shows `Got delegate … isConformantFS 1`, `FSModule initial setup`,
   `-[FSModuleExtension didFinishLaunching]: Created FSMachPort`. So the entitlement, sandbox,
   bundle layout, and `FSUnaryFileSystem`/`FSVolume` conformances are all **accepted live**. ✅
4. **The blocker — the extension process exits 0 before vending its XPC endpoint.**
   Immediately after `didFinishLaunching`, `runningboardd` reports
   `monitor: got an update with info exited, wait4() status=0` /
   `termination reported by launchd (0, 0, 0)` (the process lives <200 ms — too fast to
   `pgrep`), and `fskit_agent`/`fskitd` then log `Got ep (null) error … NSCocoaErrorDomain
   Code=4099` → `mount` prints **"Probing resource: Couldn't communicate with a helper
   application."** There is **no crash** (clean `exit(0)`); the ExtensionFoundation `@main`
   run loop returns instead of blocking to serve the XPC connection. Our own delegate code
   (`probeResource`/`loadResource`, `os.Logger subsystem=com.maker.fskit.hello`) **never
   runs** — the process exits before FSKit drives any operation. The `Have delegate, class …`
   / `didFinishLaunching` lines are FSKit framework-internal, not our code.

5. **ROOT CAUSE NAILED (2026-06-14, second VM session): residency is gated specifically to an
   Apple-issued *Team* identity.** Two controlled re-signs on the *same* AMFI-off VM isolate it:

   - **Self-signed (non-ad-hoc, Team-less) identity → identical failure.** A throwaway
     code-signing cert was generated **inside the VM** (`openssl req -x509` codeSigning EKU →
     `security import` into login.keychain → `security add-trusted-cert -p codeSign` as a trust
     root → `security set-key-partition-list` so `codesign` uses it headless — no host keychain,
     no Apple creds). Re-signing the appex with it produced a valid signature
     (`Authority=MakerFSKit DevSign`, `Signature size=1640`, `TeamIdentifier=not set`, the
     `com.apple.developer.fskit.fsmodule` entitlement honored under AMFI-off). Result: **byte-for-
     byte the same** — `didFinishLaunching` → `exit(0)` → `Got ep (null) error … 4099`. So it is
     **NOT** "ad-hoc vs any real identity": a self-signed Team-less identity behaves exactly like
     ad-hoc.

   - **FUSE-T's shipping Apple-Team module → STAYS RESIDENT and vends its endpoint, on the same
     VM.** The control: install `/Applications/fuse-t.app` (its `Contents/Extensions/
     FskitSrvModule.appex` is signed `TeamIdentifier=6DY7Z4SVDZ`, `application-identifier
     6DY7Z4SVDZ.org.fuset.fskit-srv.module`, with an **`embedded.provisionprofile`**), enable it
     the same way, probe-mount it. Result on the identical AMFI-off VM:
     `fskitd: Got ep <private> error (null)` (**endpoint vended — no 4099**); the delegate's own
     `fuse-t: probeResource resource=…` line **runs**; and `pgrep -lx FskitSrvModule` shows the
     process **still alive** (`RESIDENT`) after the probe. (It returns POSIX 13 only because the
     probe was handed a bogus `none` resource path — that's the delegate executing, not a
     residency failure.)

   **Conclusion — the matrix:**

   | signature on the SAME AMFI-off VM | `didFinishLaunching` | XPC endpoint | stays resident | delegate runs |
   |---|---|---|---|---|
   | ad-hoc (`-s -`) + fsmodule ent | ✅ | ❌ `4099` | ❌ exit 0 | ❌ |
   | self-signed (Team-less) + fsmodule ent | ✅ | ❌ `4099` | ❌ exit 0 | ❌ |
   | **free "Apple Development" Team `ERVD4F8593`** (no profile) + fsmodule ent | ✅ | ❌ `4099` | ❌ exit 0 | ❌ |
   | **paid Apple Team `6DY7Z4SVDZ` + provisionprofile** (FUSE-T) | ✅ | ✅ vended | ✅ | ✅ |

   So the macOS-26 ExtensionFoundation `AppExtensionPoint` binding that keeps
   `AppExtension.main()` alive long enough to vend `NSXPCListenerEndpoint` is **validated against
   an Apple-issued Team identity backed by a provisioning profile** (with the matching
   `application-identifier` / `team-identifier`). AMFI-off buys the restricted *entitlement
   claim* (Phase-0 enablement wall, step 2) — but **not** the extension-point residency binding.
   That binding is the deeper credential gate Phase-0 first flagged, now pinned precisely.

5b. **DECISIVE (2026-06-14, third VM session): the FREE "Apple Development" (Personal-Team) cert
   does NOT satisfy FSKit residency. The PAID FSKit Module provisioning profile is required even
   for local dev.** The one remaining unknown from step 5 is now closed with live evidence on the
   same AMFI-off VM (`maker-fskit-test` @ 192.168.64.2). The host keychain is now authorized for
   headless codesign with the user's real free-team identity
   (`codesign -f -s "Apple Development: marvin schwartz (7LA92R64RW)"` → `TeamIdentifier=ERVD4F8593`,
   a **free** Personal Team that cannot provision the `com.apple.developer.fskit.fsmodule`
   capability in a profile — proven earlier). Re-signed the hello-world appex + host app with it
   (FSKit entitlement kept in the `--entitlements` plist, `--generate-entitlement-der`; honored
   under AMFI-off), deployed, enabled, `mount -F -t makerhello`:

   - `codesign -dv` on the deployed appex: `Identifier=com.maker.fskit.hello.module`,
     `TeamIdentifier=ERVD4F8593`, with `com.apple.developer.fskit.fsmodule` + app-sandbox
     entitlements embedded. ✅ (real Apple Team identity, not ad-hoc/self-signed)
   - **Live log (the extension, pid 3165):** `Launching process ... bundleID:
     com.maker.fskit.hello.module` → `FskitHello[3165]: Have delegate` +
     `-[FSModuleExtension didFinishLaunching]: Created FSMachPort = 5131` (FSKit handshake passes)
     → `fskit_agent: About to fetch endpoint` → `Connection error getting endpoint Error
     Domain=NSCocoaErrorDomain Code=4099` → `runningboardd: monitor: got an update with info
     exited, wait4() status=0` / `termination reported by launchd (0, 0, 0)` → `fskitd: Got ep
     (null) error ... Code=4099`. `pgrep -lx FskitHello` after the mount: **NOT resident.**
     `mount` prints "Probing resource: Couldn't communicate with a helper application" (rc=69).
     **Byte-for-byte identical to ad-hoc and self-signed** — the free Team identity adds nothing.
   - **FUSE-T control re-run in the SAME session, SAME VM** (paid Team `6DY7Z4SVDZ` +
     `embedded.provisionprofile`, `-t fuset`): `FskitSrvModule[3288]: didFinishLaunching ...
     FSMachPort = 11535` → `getModuleListenerEndpoint got listener` → `fskit_agent: endpoint
     fetch got endpoint <private> error (null)` → `fskitd: Got ep <private> error (null)`
     (**endpoint vended, no 4099**); `pgrep -lx FskitSrvModule`: **3288 RESIDENT.** (Probe-mount
     returns POSIX "Permission denied" on the bogus `none` resource — that's the delegate
     *executing*, the opposite of a residency failure.)

   **The differentiator, pinned exactly:** it is **not** ad-hoc vs real identity, and **not**
   merely having a Team ID. The free "Apple Development" cert IS a real Apple Team identity
   (`ERVD4F8593`) and still fails identically. What FUSE-T has and the free-team build cannot
   produce is an **`embedded.provisionprofile`** issued by Apple for an App ID carrying the
   **FSKit Module** capability. The free Personal Team cannot mint such a profile (the capability
   is not in its allowlist — proven earlier), so it cannot satisfy the ExtensionFoundation
   residency binding. **A paid Apple Developer membership with the FSKit Module App-ID capability
   + provisioning profile is mandatory even for local dev** — there is no free path to a live
   FSKit mount on macOS 26.

   | identity tier | can mint FSKit profile | extension stays resident / mounts |
   |---|---|---|
   | ad-hoc / self-signed (Team-less) | n/a | ❌ |
   | **free "Apple Development" Personal Team** | ❌ (capability not allowlisted) | ❌ (proven 2026-06-14) |
   | **paid Apple Developer + FSKit Module capability + profile** | ✅ | ✅ (FUSE-T control) |

   **Decision: FSKit is NOT dev-testable for $0.** `fs-webdav` remains the default + only viable
   macOS backend until a paid Apple Developer membership is available to enable the FSKit Module
   capability and embed a real provisioning profile (the shipping path, §5). The Swift extension,
   appex plist, entitlements, and Rust bridge are all correct (FSKit accepts the delegate live
   under every identity); the sole remaining gate is the paid FSKit provisioning profile. The VM
   was returned to baseline after the session (both modules removed, `enabledModules.plist`
   restored to Apple-only on both the admin + `/var/root` copies, FUSE-T uninstalled; AMFI-off
   boot-arg left as the VM's pre-existing baseline).

   **Plist parity hardened along the way (kept):** `swift/hello-world/build-helloapp.sh` now
   stamps the appex `Info.plist` to match the shipping FUSE-T module's shape —
   `LSMinimumSystemVersion=26.0`, `CFBundleSupportedPlatforms=[MacOSX]`, `DTSDKName`/
   `DTPlatformVersion`/`CFBundleInfoDictionaryVersion`, and the full `EXAppExtensionAttributes`
   set (`FSMediaTypes`/`FSPersonalities`/`FSRequiresSecurityScopedPathURLResources`). This was
   tested in isolation first (Hypothesis 1) and is **necessary-but-not-sufficient**: it did not
   change the exit-0 behavior on its own, but the FUSE-T control proves a correct plist *is* part
   of the working shape, so it stays.

**The entry-point bug this surfaced (fixed):** the blind-authored `main.swift` entered via
`@_silgen_name("NSExtensionMain")` with a zero-arg signature, called from synthesized
top-level code. Under `fskitd` that **recursed infinitely** (`main → NSExtensionMain →
EXExtensionMain → _EXRunningExtension._start → … → main`, stack-guard SIGSEGV at depth ~9000).
The supported macOS 15.4+ entry is a `@main` type conforming to
`FSKit.UnaryFileSystemExtension` (which refines `ExtensionFoundation.AppExtension`); Extension
Foundation synthesizes the run loop and routes FSKit's XPC to `fileSystem`. Both
`swift/hello-world/HelloFSExtension.swift` and the production `swift/main.swift` now use the
`@main UnaryFileSystemExtension` pattern (no `NSExtensionMain`, no `NSExtensionPrincipalClass`
in the Info.plist — matching the shipping FUSE-T module's plist shape). With this fix the
extension launches and is FSKit-accepted (step 3); it was the recursion fix that exposed the
step-4 run-loop blocker underneath.

**Bottom line (fully resolved 2026-06-14):** AMFI-off makes the FSKit *entitlement + enablement*
reachable ad-hoc (the Phase-0 wall falls), and the Swift extension + appex plist are correct
enough that FSKit accepts the delegate live under *every* identity. Full mount/round-trip parity
is gated on `AppExtension.main()` staying resident to vend its XPC endpoint — and that residency
requires an **Apple-issued provisioning profile carrying the FSKit Module capability**, which only
a **paid** Apple Developer membership can mint. The controlled matrix (steps 5 + 5b) proves it
across all four signing tiers on the *same* AMFI-off VM: ad-hoc, self-signed (Team-less), **and
the user's free "Apple Development" Personal Team (`ERVD4F8593`)** all exit 0 / `4099` / non-
resident, while FUSE-T's paid-Team (`6DY7Z4SVDZ`) + `embedded.provisionprofile` module stays
resident and vends its endpoint. **The free personal-team cert is NOT sufficient; the paid FSKit
provisioning profile is mandatory even for local dev — there is no $0 path to a live FSKit mount
on macOS 26.** `fs-webdav` remains the default + only viable macOS backend until a paid membership
is available (§7). The VM was returned to baseline after each session (`vm-restore.sh` / inline
cleanup; both test modules removed + `enabledModules.plist` restored to Apple-only).
