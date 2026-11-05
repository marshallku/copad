// swift-tools-version: 6.0
import PackageDescription

// MARK: - copad-ffi linkage

// The Copad executable links the Rust staticlib `libcopad_ffi.a`. SwiftPM cannot run cargo, so
// build with `cargo build --release -p copad-ffi` first (scripts/install-macos.sh and
// copad-macos/run.sh do both steps); `swift build` alone fails with an undefined-symbol link
// error. `-L../target/release` is relative to the package root.

let package = Package(
    name: "copad-macos",
    platforms: [
        .macOS(.v14),
    ],
    dependencies: [
        .package(url: "https://github.com/LebJe/TOMLKit", from: "0.6.0"),
    ],
    targets: [
        // C wrapper exposing copad-ffi to Swift; dummy.c forces an object so the linker settings
        // reach the executable.
        .target(
            name: "CCopadFFI",
            path: "Sources/CCopadFFI",
            publicHeadersPath: "include",
        ),
        // Same dummy.c trick for the `libcopad_term.a` staticlib (copad-term/).
        .target(
            name: "CCopadTerm",
            path: "Sources/CCopadTerm",
            publicHeadersPath: "include",
        ),
        // Pure-Swift mirror types of `copad-core` structs, split out so tests build without the
        // GUI layer. New pure-logic types belong here.
        .target(
            name: "CopadCore",
            path: "Sources/CopadCore",
        ),
        .executableTarget(
            name: "Copad",
            dependencies: [
                .product(name: "TOMLKit", package: "TOMLKit"),
                "CCopadFFI",
                "CCopadTerm",
                "CopadCore",
            ],
            path: "Sources/Copad",
            // Swift 5 mode: Xcode's compiler turns the GUI layer's Sendable warnings into errors
            // during `swift test`. CopadCore stays on the package-default Swift 6 mode.
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                .unsafeFlags(["-L../target/release"]),
                .linkedLibrary("copad_ffi"),
                .linkedLibrary("copad_term"),
            ],
        ),
        // Depends on `CopadCore`, not the executable. Run with
        // `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift test`; Command Line
        // Tools do not ship XCTest.
        .testTarget(
            name: "CopadCoreTests",
            dependencies: ["CopadCore"],
            path: "Tests/CopadCoreTests",
        ),
    ],
)
