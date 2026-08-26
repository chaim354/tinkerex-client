// Minimal hello-world FSKit module — Phase 0 de-risk for the macOS FSKit mount port.
//
// Goal: prove a *third-party* FSKit extension can register, enable, and mount on THIS
// macOS box (26.x) with whatever signing we can do (ad-hoc / personal team), before
// investing in the full Rust-Provider bridge. It presents a static, read-only volume
// with a single file `hello.txt` at the root. No Rust, no Provider — just the FSKit
// plumbing and the question "does it mount at all?".
//
// The module is an `FSUnaryFileSystem` (one resource → one volume), the simplest FSKit
// shape and the one the real `fs-fskit` bridge will also use.

import Foundation
import FSKit
import os

private let log = Logger(subsystem: "com.maker.fskit.hello", category: "hello")

// A trivial in-memory item: either the root directory or the single file.
final class HelloItem: FSItem {
    let id: FSItem.Identifier
    let name: FSFileName
    let isDir: Bool
    let data: Data

    init(id: UInt64, name: String, isDir: Bool, data: Data = Data()) {
        self.id = FSItem.Identifier(rawValue: id)!
        self.name = FSFileName(string: name)
        self.isDir = isDir
        self.data = data
        super.init()
    }
}

// The volume: answers the FSKit operation callbacks against the two static items.
final class HelloVolume: FSVolume {
    static let rootID: UInt64 = FSItem.Identifier.rootDirectory.rawValue
    static let fileID: UInt64 = 2

    let root: HelloItem
    let file: HelloItem

    init(resource: FSResource) {
        let payload = Data("hello from a third-party FSKit volume!\n".utf8)
        self.root = HelloItem(id: HelloVolume.rootID, name: "/", isDir: true)
        self.file = HelloItem(id: HelloVolume.fileID, name: "hello.txt", isDir: false, data: payload)
        super.init(
            volumeID: FSVolume.Identifier(uuid: UUID()),
            volumeName: FSFileName(string: "HelloFSKit")
        )
    }

    private func attrs(for item: HelloItem) -> FSItem.Attributes {
        let a = FSItem.Attributes()
        a.fileID = item.id
        a.parentID = item.isDir ? FSItem.Identifier.parentOfRoot : root.id
        a.type = item.isDir ? .directory : .file
        a.mode = item.isDir ? 0o755 : 0o644
        a.linkCount = 1
        a.uid = 0
        a.gid = 0
        a.size = UInt64(item.data.count)
        a.allocSize = UInt64(item.data.count)
        let now = timespec(tv_sec: time(nil), tv_nsec: 0)
        a.modifyTime = now
        a.accessTime = now
        a.changeTime = now
        a.birthTime = now
        return a
    }
}

// MARK: - FSVolume.Operations
extension HelloVolume: FSVolume.Operations {
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsHardLinks = false
        caps.supportsSymbolicLinks = false
        caps.supportsPersistentObjectIDs = true
        caps.doesNotSupportVolumeSizes = false
        caps.caseFormat = .insensitiveCasePreserving
        return caps
    }

    var volumeStatistics: FSStatFSResult {
        let s = FSStatFSResult(fileSystemTypeName: "HelloFSKit")
        s.blockSize = 4096
        s.ioSize = 4096
        s.totalBlocks = 1024
        s.availableBlocks = 0
        s.freeBlocks = 0
        s.totalFiles = 2
        s.freeFiles = 0
        return s
    }

    func activate(options: FSTaskOptions, replyHandler reply: @escaping (FSItem?, (any Error)?) -> Void) {
        log.info("activate")
        reply(root, nil)
    }

    func deactivate(options: FSDeactivateOptions = [], replyHandler reply: @escaping ((any Error)?) -> Void) {
        log.info("deactivate")
        reply(nil)
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping ((any Error)?) -> Void) {
        log.info("mount")
        reply(nil)
    }

    func unmount(replyHandler reply: @escaping () -> Void) {
        log.info("unmount")
        reply()
    }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(nil)
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest,
                       of item: FSItem,
                       replyHandler reply: @escaping (FSItem.Attributes?, (any Error)?) -> Void) {
        guard let it = item as? HelloItem else { return reply(nil, fs_errorForPOSIXError(POSIXError.EIO.rawValue)) }
        reply(attrs(for: it), nil)
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest,
                       on item: FSItem,
                       replyHandler reply: @escaping (FSItem.Attributes?, (any Error)?) -> Void) {
        guard let it = item as? HelloItem else { return reply(nil, fs_errorForPOSIXError(POSIXError.EIO.rawValue)) }
        reply(attrs(for: it), nil) // read-only: ignore, echo current
    }

    func lookupItem(named name: FSFileName,
                    inDirectory directory: FSItem,
                    replyHandler reply: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        if name.string == "hello.txt" {
            reply(file, file.name, nil)
        } else {
            reply(nil, nil, fs_errorForPOSIXError(POSIXError.ENOENT.rawValue))
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(nil)
    }

    func readSymbolicLink(_ item: FSItem,
                          replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        reply(nil, fs_errorForPOSIXError(POSIXError.EINVAL.rawValue))
    }

    func createItem(named name: FSFileName,
                    type: FSItem.ItemType,
                    inDirectory directory: FSItem,
                    attributes newAttributes: FSItem.SetAttributesRequest,
                    replyHandler reply: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        reply(nil, nil, fs_errorForPOSIXError(POSIXError.EROFS.rawValue))
    }

    func createSymbolicLink(named name: FSFileName,
                            inDirectory directory: FSItem,
                            attributes newAttributes: FSItem.SetAttributesRequest,
                            linkContents contents: FSFileName,
                            replyHandler reply: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        reply(nil, nil, fs_errorForPOSIXError(POSIXError.EROFS.rawValue))
    }

    func createLink(to item: FSItem,
                    named name: FSFileName,
                    inDirectory directory: FSItem,
                    replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        reply(nil, fs_errorForPOSIXError(POSIXError.EROFS.rawValue))
    }

    func removeItem(_ item: FSItem,
                    named name: FSFileName,
                    fromDirectory directory: FSItem,
                    replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(fs_errorForPOSIXError(POSIXError.EROFS.rawValue))
    }

    func renameItem(_ item: FSItem,
                    inDirectory sourceDirectory: FSItem,
                    named sourceName: FSFileName,
                    to destinationName: FSFileName,
                    inDirectory destinationDirectory: FSItem,
                    overItem: FSItem?,
                    replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        reply(nil, fs_errorForPOSIXError(POSIXError.EROFS.rawValue))
    }

    func enumerateDirectory(_ directory: FSItem,
                            startingAt cookie: FSDirectoryCookie,
                            verifier: FSDirectoryVerifier,
                            attributes: FSItem.GetAttributesRequest?,
                            packer: FSDirectoryEntryPacker,
                            replyHandler reply: @escaping (FSDirectoryVerifier, (any Error)?) -> Void) {
        // One entry: hello.txt. Cookie 0 = start; emit then signal done with cookie past it.
        if cookie.rawValue == 0 {
            _ = packer.packEntry(name: file.name,
                                 itemType: .file,
                                 itemID: file.id,
                                 nextCookie: FSDirectoryCookie(rawValue: 1),
                                 attributes: attributes != nil ? attrs(for: file) : nil)
        }
        reply(verifier, nil)
    }
}

// MARK: - FSVolume.PathConfOperations
extension HelloVolume: FSVolume.PathConfOperations {
    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 0 }
    var maximumFileSize: UInt64 { UInt64.max }
}

// MARK: - FSVolume.ReadWriteOperations
extension HelloVolume: FSVolume.ReadWriteOperations {
    func read(from item: FSItem,
              at offset: off_t,
              length: Int,
              into buffer: FSMutableFileDataBuffer,
              replyHandler reply: @escaping (Int, (any Error)?) -> Void) {
        guard let it = item as? HelloItem, !it.isDir else {
            return reply(0, fs_errorForPOSIXError(POSIXError.EISDIR.rawValue))
        }
        let start = min(Int(offset), it.data.count)
        let end = min(start + length, it.data.count)
        let slice = it.data.subdata(in: start..<end)
        let n = slice.withUnsafeBytes { raw in
            buffer.withUnsafeMutableBytes { dst in
                let count = min(slice.count, dst.count)
                dst.copyBytes(from: UnsafeRawBufferPointer(rebasing: raw[0..<count]))
                return count
            }
        }
        reply(n, nil)
    }

    func write(contents: Data,
               to item: FSItem,
               at offset: off_t,
               replyHandler reply: @escaping (Int, (any Error)?) -> Void) {
        reply(0, fs_errorForPOSIXError(POSIXError.EROFS.rawValue))
    }
}

// MARK: - FSVolume.OpenCloseOperations
extension HelloVolume: FSVolume.OpenCloseOperations {
    func openItem(_ item: FSItem, modes: FSVolume.OpenModes,
                  replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(nil)
    }
    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes,
                   replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(nil)
    }
}
