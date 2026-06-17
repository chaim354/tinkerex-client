# Swift FSKit extension — scaffolding (account-gated)

This directory holds the **macOS-side** of the FSKit mount: the `FSKit` extension (`.appex`)
that the kernel loads, and the Swift↔Rust FFI shims that call into `fs-fskit`'s C ABI
(`src/ffi.rs`). **None of it builds without the FSKit SDK + a paid Apple Developer account**
— every file here is a stub with `TODO(dev-account)` markers showing exactly what to fill in.

The architectural call (see `docs/plan-macos-fskit-migration.md`): FSKit is the **FUSE-shaped**
model (`FSUnaryFileSystem`/`FSVolume`, a full userspace filesystem), NOT the placeholder
File-Provider model. So these conformances answer `lookup`/`enumerate`/`read`/`write`/
`create`/`remove`/`rename` by calling the Rust bridge, which routes to the shared `Provider`.

## Files
- `MakerFSExtension.swift` — the `FSModule` / `FSUnaryFileSystem` / `FSVolume.Operations`
  conformances. The kernel calls these; they delegate to `Bridge.swift`.
- `Bridge.swift` — `@_silgen_name`/`@_cdecl` declarations of the Rust `fskit_*` C functions,
  plus thin Swift wrappers that marshal `FSItem` ↔ the C scalar/buffer ABI.
- `Info.plist` — the extension bundle metadata + the FSKit extension-point declaration.
- `MakerFS.entitlements` — the FSKit + App Sandbox entitlements the `.appex` needs.

See `../BUILD.md` for the full when-the-account-arrives checklist (signing, provisioning,
registration, smoke test).
