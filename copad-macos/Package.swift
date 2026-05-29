// swift-tools-version: 6.0
import PackageDescription

// MARK: - copad-ffi linkage

//
// The Copad executable links a Rust staticlib (`libcopad_ffi.a`) produced by the
// copad-ffi crate at the workspace root. SwiftPM has no first-class way to
// invoke cargo as a prebuild step from this manifest shape, so the build
// pipeline is split:
//
//   1. `cargo build --release -p copad-ffi`   → workspace_root/target/release/libcopad_ffi.a
//   2. `swift build`                          → links libcopad_ffi.a via the linker flags below
//
// scripts/install-macos.sh + copad-macos/run.sh wrap both steps. Running
// `swift build` alone after a clean target/ directory will fail with an
// undefined-symbol link error — the build script is the source of truth.
//
// The `-L../target/release` is a relative path interpreted at link time from
// the package root (`copad-macos/`), resolving to the cargo workspace target
// directory. `linkedLibrary("copad_ffi")` adds `-lcopad_ffi` to find the
// staticlib by its base name.

let package = Package(
    name: "copad-macos",
    platforms: [
        .macOS(.v14),
    ],
    dependencies: [
        .package(url: "https://github.com/migueldeicaza/SwiftTerm", from: "1.2.0"),
        .package(url: "https://github.com/LebJe/TOMLKit", from: "0.6.0"),
    ],
    targets: [
        // C wrapper that exposes copad-ffi's C symbols to Swift via a clang
        // module. The header + module.modulemap live under include/, the
        // dummy.c forces SwiftPM to actually emit a target object so the
        // linker settings flow through to the final executable.
        .target(
            name: "CCopadFFI",
            path: "Sources/CCopadFFI",
            publicHeadersPath: "include",
        ),
        // Sibling C module for the renderer-migration Rust staticlib
        // (`libcopad_term.a` — see copad-term/ + docs/macos-renderer-migration-plan.md
        // Phase 1). Same dummy.c trick as CCopadFFI. Both staticlibs
        // get linked into Copad.app; the Phase 0 spike proved no
        // symbol collision.
        .target(
            name: "CCopadTerm",
            path: "Sources/CCopadTerm",
            publicHeadersPath: "include",
        ),
        // Pure-Swift library holding the executable-independent mirror
        // types (Swift counterparts of `copad-core` Rust structs:
        // ContextService, PaneContext). Extracted from the executable
        // target so the test bundle below can be built without
        // recompiling the GUI layer — that layer has pre-existing
        // strict-concurrency issues under Xcode's Swift 6 (warnings
        // under CLT's compiler, errors under Xcode's), and isolating
        // these types here keeps unit-test iteration unblocked. New
        // pure-logic types should land here, not in the executable.
        .target(
            name: "CopadCore",
            path: "Sources/CopadCore",
        ),
        .executableTarget(
            name: "Copad",
            dependencies: [
                .product(name: "SwiftTerm", package: "SwiftTerm"),
                .product(name: "TOMLKit", package: "TOMLKit"),
                "CCopadFFI",
                "CCopadTerm",
                "CopadCore",
            ],
            path: "Sources/Copad",
            // Pin Swift 5 language mode for the GUI layer. Under CLT's
            // 6.2.4 compiler the Sendable errors here are warnings, but
            // Xcode 16's 6.3.2 escalates them to hard errors during
            // `swift test`. Pinning swift 5 keeps the test bundle
            // buildable without scope-creeping into a strict-concurrency
            // refactor of AppDelegate / TerminalViewController. The
            // pure-logic mirror lives in CopadCore which stays on the
            // package-default Swift 6 mode.
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                .unsafeFlags(["-L../target/release"]),
                .linkedLibrary("copad_ffi"),
                .linkedLibrary("copad_term"),
            ],
        ),
        // Swift-side unit tests. Depends on `CopadCore` (not the
        // executable) so the build does not pull in the GUI layer.
        // `@testable import CopadCore` exposes internal helpers when
        // we add tests for non-public APIs later. Run with
        // `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift test`
        // — Command Line Tools alone do not ship XCTest.
        .testTarget(
            name: "CopadCoreTests",
            dependencies: ["CopadCore"],
            path: "Tests/CopadCoreTests",
        ),
    ],
)
