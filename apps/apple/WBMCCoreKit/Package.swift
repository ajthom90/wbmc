// swift-tools-version: 6.2
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import PackageDescription

let package = Package(
    name: "WBMCCoreKit",
    platforms: [.macOS(.v26), .iOS(.v26)],
    products: [
        .library(name: "WBMCCore", targets: ["WBMCCore"]),
    ],
    targets: [
        // Built by scripts/build-apple.sh (not checked in).
        .binaryTarget(name: "WBMCCoreFFI", path: "WBMCCoreFFI.xcframework"),
        .target(
            name: "WBMCCore",
            dependencies: ["WBMCCoreFFI"],
            path: "Sources/WBMCCore",
            linkerSettings: [
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
    ]
)
