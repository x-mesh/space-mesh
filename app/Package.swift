// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "SpaceMesh",
    platforms: [.macOS(.v14)],
    targets: [
        // UniFFI가 생성한 C 헤더 모듈 — 심볼 구현은 Rust staticlib에서 온다.
        .target(
            name: "space_ffiFFI",
            path: "Sources/space_ffiFFI"
        ),
        // UniFFI가 생성한 Swift 바인딩.
        .target(
            name: "SpaceMeshCore",
            dependencies: ["space_ffiFFI"],
            path: "Sources/SpaceMeshCore"
        ),
        .executableTarget(
            name: "SpaceMeshApp",
            dependencies: ["SpaceMeshCore"],
            path: "Sources/SpaceMeshApp",
            exclude: ["Resources"],
            linkerSettings: [
                .unsafeFlags(["-L\(Context.packageDirectory)/../core/target/release"]),
                .linkedLibrary("space_ffi"),
            ]
        ),
    ]
)
