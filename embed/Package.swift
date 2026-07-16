// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "rsd-embed",
    platforms: [.macOS(.v14)],
    targets: [.executableTarget(name: "rsd-embed", path: "Sources/rsd-embed")]
)
