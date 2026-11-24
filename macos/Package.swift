// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "EjaMacOS",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .library(name: "EjaClient", targets: ["EjaClient"]),
        .library(name: "EjaUI", type: .dynamic, targets: ["EjaUI"]),
        .executable(name: "EjaApp", targets: ["EjaLauncher"]),
    ],
    targets: [
        .target(
            name: "EjaClient",
            path: "Sources/EjaClient"
        ),
        .target(
            name: "EjaUI",
            dependencies: ["EjaClient"],
            path: "Sources/EjaApp"
        ),
        .executableTarget(
            name: "EjaLauncher",
            path: "Sources/EjaLauncher"
        ),
        .testTarget(
            name: "EjaAppTests",
            dependencies: ["EjaClient"],
            path: "Tests/EjaAppTests"
        ),
    ]
)
