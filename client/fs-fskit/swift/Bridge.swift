// Bridge.swift — Swift ↔ Rust FFI shims for the FSKit extension.
//
// STUB / TODO(dev-account): this file does NOT compile without the FSKit SDK (it imports
// `FSKit`). It declares the Rust C ABI from `fs-fskit/src/ffi.rs` and wraps each call so the
// FSKit conformances in MakerFSExtension.swift can stay clean. The Rust side is already real
// + unit-tested; this is the marshalling layer that the SDK headers + signing unlock.
//
// How the handle flows: the engine (Rust) builds an `FsKitBridge`, `into_raw`s it to a
// `*mut c_void`, and passes that opaque pointer to the extension at startup. Swift stores it
// as `OpaquePointer` and threads it through every call. On teardown, `fskit_free(handle)`.

import Foundation

// MARK: - Rust C ABI (declared from fs-fskit/src/ffi.rs)
//
// TODO(dev-account): these `@_silgen_name` decls must match the FINAL FSKit-struct-typed
// signatures once `src/ffi.rs`'s shims are upgraded from the scalar/buffer sketches. For now
// they mirror the sketched scalar ABI so the shape is concrete.

/// fskit_lookup(bridge, parent_id, name_utf8, name_len, *out_item_id) -> errno
@_silgen_name("fskit_lookup")
func rust_fskit_lookup(
    _ bridge: OpaquePointer?,
    _ parentId: UInt64,
    _ name: UnsafePointer<UInt8>?,
    _ nameLen: Int,
    _ outItemId: UnsafeMutablePointer<UInt64>?
) -> Int32

/// fskit_read(bridge, item_id, offset, size, *out_buf, *out_len) -> errno
@_silgen_name("fskit_read")
func rust_fskit_read(
    _ bridge: OpaquePointer?,
    _ itemId: UInt64,
    _ offset: UInt64,
    _ size: UInt32,
    _ outBuf: UnsafeMutablePointer<UInt8>?,
    _ outLen: UnsafeMutablePointer<Int>?
) -> Int32

/// fskit_free(bridge)
@_silgen_name("fskit_free")
func rust_fskit_free(_ bridge: OpaquePointer?)

// TODO(dev-account): declare the rest as `src/ffi.rs` grows them:
//   fskit_getattr, fskit_enumerate, fskit_create, fskit_mkdir, fskit_write, fskit_flush,
//   fskit_truncate, fskit_unlink, fskit_rmdir, fskit_rename.

// MARK: - Swift-friendly wrappers

/// Holds the opaque Rust bridge pointer for the volume's lifetime. The Rust side owns the
/// allocation; `deinit` returns it via `fskit_free`.
final class RustBridge {
    private let handle: OpaquePointer

    /// `handle` is the `*mut c_void` the engine produced via `fs_fskit::ffi::into_raw`.
    init(handle: OpaquePointer) {
        self.handle = handle
    }

    deinit {
        rust_fskit_free(handle)
    }

    /// Look up `name` under `parentId`; returns the child item id or throws an `NSError`
    /// carrying the POSIX errno the Rust side returned.
    func lookup(parentId: UInt64, name: String) throws -> UInt64 {
        var out: UInt64 = 0
        let bytes = Array(name.utf8)
        let rc = bytes.withUnsafeBufferPointer { buf in
            rust_fskit_lookup(handle, parentId, buf.baseAddress, buf.count, &out)
        }
        if rc != 0 { throw posixError(rc) }
        return out
    }

    /// Read up to `size` bytes at `offset` from `itemId`.
    func read(itemId: UInt64, offset: UInt64, size: UInt32) throws -> Data {
        var buf = [UInt8](repeating: 0, count: Int(size))
        var len = 0
        let rc = buf.withUnsafeMutableBufferPointer { p in
            rust_fskit_read(handle, itemId, offset, size, p.baseAddress, &len)
        }
        if rc != 0 { throw posixError(rc) }
        return Data(buf.prefix(len))
    }

    private func posixError(_ errno: Int32) -> NSError {
        NSError(domain: NSPOSIXErrorDomain, code: Int(errno))
    }
}
