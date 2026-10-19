// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import XCTest

final class LaunchTests: XCTestCase {
    @MainActor
    func testLaunchShowsAddAccount() throws {
        let app = XCUIApplication()
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("WBMC-UI-\(UUID().uuidString)")
        defer { app.terminate(); try? FileManager.default.removeItem(at: directory) }
        app.launchEnvironment["WBMC_SECRETS"] = "memory"
        app.launchEnvironment["WBMC_DATA_DIR"] = directory.path
        app.launch()
        XCTAssertTrue(app.buttons["Add Account"].waitForExistence(timeout: 15))
    }
}
