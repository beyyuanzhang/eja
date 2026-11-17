// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Eja",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "Eja",
            path: "Sources/Eja"
        )
    ]
)
