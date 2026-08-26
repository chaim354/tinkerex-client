// The FSUnaryFileSystem delegate + the @main extension entry point.

import Foundation
import FSKit
import ExtensionFoundation
import os

private let log = Logger(subsystem: "com.maker.fskit.hello", category: "module")

final class HelloFSModule: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    func probeResource(resource: FSResource,
                       replyHandler reply: @escaping (FSProbeResult?, (any Error)?) -> Void) {
        log.info("probeResource")
        // Always claim the resource as usable (this is a synthetic FS, not backed by a
        // real on-disk format we'd need to recognize).
        reply(FSProbeResult.usable(name: "HelloFSKit",
                                   containerID: FSContainerIdentifier(uuid: UUID())),
              nil)
    }

    func loadResource(resource: FSResource,
                      options: FSTaskOptions,
                      replyHandler reply: @escaping (FSVolume?, (any Error)?) -> Void) {
        log.info("loadResource")
        let volume = HelloVolume(resource: resource)
        reply(volume, nil)
    }

    func unloadResource(resource: FSResource,
                        options: FSTaskOptions,
                        replyHandler reply: @escaping ((any Error)?) -> Void) {
        log.info("unloadResource")
        reply(nil)
    }
}

// The @main extension entry. FSKit modules on macOS 15.4+ are ExtensionFoundation
// `AppExtension`s; `UnaryFileSystemExtension` provides the run-loop `configuration` and
// `@main` synthesizes the non-returning `main()`. `fileSystem` is a COMPUTED property
// returning a freshly-constructed module per Apple's FSKitSample (a stored `let` let the
// instance be torn down and the process exited 0 before vending its XPC endpoint — VM
// bring-up 2026-06-14).
@main
struct HelloFSMain: UnaryFileSystemExtension {
    var fileSystem: HelloFSModule {
        HelloFSModule()
    }
}
