// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "RSD",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(name: "RSD", path: "Sources/RSD")
    ]
)
