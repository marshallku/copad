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
        .executableTarget(
            name: "Copad",
            dependencies: [
                .product(name: "SwiftTerm", package: "SwiftTerm"),
                .product(name: "TOMLKit", package: "TOMLKit"),
                "CCopadFFI",
                "CCopadTerm",
            ],
            path: "Sources/Copad",
            linkerSettings: [
                .unsafeFlags(["-L../target/release"]),
                .linkedLibrary("copad_ffi"),
                .linkedLibrary("copad_term"),
            ],
        ),
    ],
)
