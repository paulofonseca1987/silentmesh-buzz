// swift-tools-version: 6.0
import PackageDescription

// MeshProtocol — the Silent Mesh client protocol layer (Phase 4, D19).
//
// Deliberately a library with no UI and no platform entitlements: it is the
// half of the macOS client that can be built, tested, and validated against
// a live relay headlessly, which is also the half every other client
// surface (vault, UI, sync) will sit on top of.
let package = Package(
    name: "MeshProtocol",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "MeshProtocol", targets: ["MeshProtocol"]),
        .library(name: "MeshVault", targets: ["MeshVault"]),
    ],
    dependencies: [
        // Nostr events are BIP-340 Schnorr over secp256k1. CryptoKit has no
        // secp256k1 curve, so this is not optional.
        .package(url: "https://github.com/21-DOT-DEV/swift-secp256k1", from: "0.21.0")
    ],
    targets: [
        .target(
            name: "MeshProtocol",
            dependencies: [.product(name: "P256K", package: "swift-secp256k1")]
        ),
        // Split from MeshProtocol because it pulls in Security and
        // LocalAuthentication and only works in a signed process — the
        // protocol layer must stay usable in plain `swift test`.
        .target(name: "MeshVault", dependencies: ["MeshProtocol"]),
        .testTarget(name: "MeshProtocolTests", dependencies: ["MeshProtocol"]),
        .testTarget(name: "MeshVaultTests", dependencies: ["MeshVault", "MeshProtocol"]),
    ]
)
