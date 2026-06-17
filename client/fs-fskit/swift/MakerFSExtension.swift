// MakerFSExtension.swift — the FSKit extension entry point (STUB).
//
// TODO(dev-account): does NOT compile without the FSKit SDK (`import FSKit`) + signing. This
// sketches the FUSE-shaped conformances (FSUnaryFileSystem + FSVolume.Operations) that the
// kernel calls, each delegating to the Rust `RustBridge` (Bridge.swift) → the shared
// `Provider`. The exact API surface (protocol names, async vs reply-handler, the FSItem /
// FSDirectoryEntryPacket types) is finalized against the SDK headers; the structure below is
// the map, with every account-gated spot flagged.

import Foundation
// import FSKit  // TODO(dev-account): the SDK module — unavailable until the account/SDK is installed.

// MARK: - The module (extension principal class)

// TODO(dev-account): conform to `FSModuleProtocol` / `FSUnaryFileSystem` from FSKit. The
// `@main`-annotated principal class is what `Info.plist`'s `FSModuleClass` points at.
//
// final class MakerFSModule: FSUnaryFileSystem, FSUnaryFileSystemOperations {
//     // Called when macOS probes/loads a volume for our resource. Build (or receive from the
//     // host app via XPC) the Rust bridge handle, then return an `FSVolume`.
//     func loadResource(...) async throws -> FSVolume { ... }
// }

/// Placeholder so the file is valid Swift syntax pre-SDK (no FSKit symbols referenced).
/// Replace with the real `FSUnaryFileSystem` subclass above.
final class MakerFSModulePlaceholder {
    let bridge: RustBridge
    init(bridge: RustBridge) { self.bridge = bridge }
}

// MARK: - The volume operations (the FUSE-shaped callbacks)
//
// TODO(dev-account): conform to `FSVolume.Operations` (+ `PathConfOperations`,
// `OpenCloseOperations`, `ReadWriteOperations`, etc. as the SDK splits them). Each method
// maps 1:1 onto a `RustBridge` call; the bridge routes to the shared `Provider`. Item
// identity: FSKit's `FSItem.Identifier` (a 64-bit value) IS our `ino` — pass it straight
// through (see `ItemId` in src/lib.rs).
//
// final class MakerFSVolume: FSVolume, FSVolume.Operations {
//     private let bridge: RustBridge
//
//     func lookupItem(named name: FSFileName, inDirectory dir: FSItem) async throws -> (FSItem, FSFileName) {
//         let childId = try bridge.lookup(parentId: dir.id, name: name.string)
//         return (item(forId: childId), name)
//     }
//
//     func enumerateDirectory(_ dir: FSItem, ...) async throws { /* bridge.enumerate(...) */ }
//
//     func read(from item: FSItem, at offset: off_t, length: Int, into buffer: ...) async throws -> Int {
//         // Hydrate-on-read is INSIDE the Rust bridge: on a chunk miss it runs the engine's
//         // presigned GET and retries, so Swift just calls read() — no placeholder dance.
//         let data = try bridge.read(itemId: item.id, offset: UInt64(offset), size: UInt32(length))
//         buffer.copyBytes(from: data); return data.count
//     }
//
//     func createItem(named: ..., type: ..., inDirectory: ...) async throws -> FSItem { /* bridge.create / mkdir */ }
//     func writeContents(_ data: Data, to item: FSItem, at offset: off_t) async throws -> Int { /* bridge.write */ }
//     func removeItem(_ item: FSItem, named: ..., fromDirectory: ...) async throws { /* bridge.unlink / rmdir */ }
//     func renameItem(_ item: FSItem, ... toName: ..., inDirectory newDir: FSItem) async throws -> FSFileName { /* bridge.rename */ }
//     func setAttributes(_ attrs: ..., on item: FSItem) async throws -> FSItem { /* truncate / set_mtime / set_mode */ }
// }

// MARK: - Active invalidation (the FSKit analogue of FUSE notify_inval_inode)
//
// TODO(dev-account): a pulled remote change calls back into the extension (via XPC from the
// host app, or a shared signal) with the affected item id + a `FsKitChange` (decided by the
// Rust pure logic `decide_change` in src/lib.rs). The extension then purges the kernel cache
// for that item — the FSKit API for this is the slot to fill once confirmed against the SDK.
// This is what makes macOS `invalidate(ino)` a REAL call instead of the WebDAV timeout no-op.
