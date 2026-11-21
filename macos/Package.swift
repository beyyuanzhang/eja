// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "EjaMacOS",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .library(name: "EjaClient", targets: ["EjaClient"]),
        .executable(name: "EjaApp", targets: ["EjaApp"]),
    ],
    targets: [
        .target(
            name: "EjaClient",
            path: "Sources/EjaClient"
        ),
        .executableTarget(
            name: "EjaApp",
            dependencies: ["EjaClient"],
            path: "Sources/EjaApp"
        ),
        .testTarget(
            name: "EjaAppTests",
            dependencies: ["EjaClient"],
            path: "Tests/EjaAppTests"
        ),
    ]
)
