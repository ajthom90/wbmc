// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AppKit
import Testing
import WebKit
@testable import WBMC

@MainActor
struct SafeHTMLViewTests {
    @Test func remoteImagesAndScriptAreBlocked() async throws {
        // Equivalent to remote_images_and_script.eml, with a detectable script side effect.
        let html = """
        <html><head><title>Safe mail</title></head><body><p>Visible text here.</p>
        <img src="https://tracker.example.com/p.gif">
        <script>document.title = 'SCRIPT EXECUTED'; alert(1)</script>
        <form action="https://evil.example.com"><input name="x"></form></body></html>
        """
        let coordinator = SafeHTMLView.Coordinator()
        let webView = SafeHTMLView.makeSecureWebView(coordinator: coordinator)
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = webView
        defer { window.contentView = nil; webView.stopLoading() }
        await coordinator.load(html: html, in: webView)
        for _ in 0..<200 {
            if coordinator.didFinish || coordinator.loadError != nil { break }
            try await Task.sleep(for: .milliseconds(50))
        }
        #expect(coordinator.loadError == nil)
        #expect(coordinator.didFinish)
        #expect(coordinator.rulesInstalled)
        #expect(!webView.configuration.defaultWebpagePreferences.allowsContentJavaScript)
        #expect(!webView.configuration.websiteDataStore.isPersistent)
        #expect(webView.title == "Safe mail")
        #expect(!coordinator.navigationURLs.contains { ["http", "https"].contains($0.scheme ?? "") })
    }

    @Test func navigationPolicyRejectsFormsAndOnlyOpensLinksExternally() {
        let coordinator = SafeHTMLView.Coordinator()
        let remote = URL(string: "https://example.com")!
        #expect(coordinator.policy(for: remote, type: .formSubmitted) == .cancel)
        #expect(coordinator.policy(for: URL(string: "file:///etc/passwd")!, type: .other) == .cancel)
        #expect(coordinator.policy(for: URL(string: "about:blank")!, type: .other) == .cancel)
    }
}
