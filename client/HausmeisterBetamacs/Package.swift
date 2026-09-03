// swift-tools-version:6.0
import Foundation
import PackageDescription

// The betamacs plugin for Hausmeister: keeps the screen censor's app and
// signed settings current through otactl's firmware pipeline, handing both
// to betamacsd (the root watchdog daemon) which re-verifies and installs.
// Lives here because betamacs owns the daemon side of the contract;
// Hausmeister (github.com/bdstark/hausmeister-mac) pulls it in by path.
// HAUSMEISTER_KIT_PATH overrides where the Kit is found.
let kitPath = ProcessInfo.processInfo.environment["HAUSMEISTER_KIT_PATH"]
  ?? "../../../../go/src/github.com/bdstark/hausmeister-mac/Kit"

let package = Package(
  name: "HausmeisterBetamacs",
  platforms: [.macOS(.v14)],
  products: [
    .library(name: "HausmeisterBetamacs", targets: ["BetamacsPlugin"]),
  ],
  dependencies: [
    .package(path: kitPath),
  ],
  targets: [
    .target(
      name: "BetamacsPlugin",
      dependencies: [.product(name: "HausmeisterKit", package: "Kit")],
      path: "Sources/BetamacsPlugin",
      swiftSettings: [.swiftLanguageMode(.v5)]
    ),
  ]
)
