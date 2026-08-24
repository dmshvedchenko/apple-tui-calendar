// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "tui-calendar-service",
    platforms: [.macOS(.v13)],
    products: [.executable(name: "tui-calendar-service", targets: ["CalendarService"])],
    targets: [
        .executableTarget(
            name: "CalendarService",
            path: "Sources/CalendarService",
            exclude: ["Info.plist"],
            linkerSettings: [
                .unsafeFlags([
                    "-Xlinker", "-sectcreate", "-Xlinker", "__TEXT", "-Xlinker", "__info_plist",
                    "-Xlinker", "Sources/CalendarService/Info.plist"
                ])
            ]
        )
    ]
)
