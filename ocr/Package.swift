// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "rsd-ocr",
    platforms: [.macOS(.v14)],
    targets: [.executableTarget(name: "rsd-ocr", path: "Sources/rsd-ocr")]
)
