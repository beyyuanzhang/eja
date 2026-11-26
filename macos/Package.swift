// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "ejaMacOS",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .library(name: "ejaClient", targets: ["ejaClient"]),
        .library(name: "ejaUI", type: .dynamic, targets: ["ejaUI"]),
        .executable(name: "ejaApp", targets: ["ejaLauncher"]),
    ],
    targets: [
        .target(
            name: "ejaClient",
            path: "Sources/ejaClient"
        ),
        .target(
            name: "ejaUI",
            dependencies: ["ejaClient"],
            path: "Sources/ejaApp"
        ),
        .executableTarget(
            name: "ejaLauncher",
            path: "Sources/ejaLauncher"
        ),
        .testTarget(
            name: "ejaAppTests",
            dependencies: ["ejaClient"],
            path: "Tests/ejaAppTests"
        ),
    ]
)
