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
        #expect(coordinator.policy(for: remote, type: .formSubmitted).navigation == .cancel)
        #expect(coordinator.policy(for: URL(string: "file:///etc/passwd")!, type: .other).navigation == .cancel)
        #expect(coordinator.policy(for: URL(string: "about:blank")!, type: .other).navigation == .cancel)
    }

    @Test func linkPolicyOnlyReturnsApprovedSchemes() {
        let coordinator = SafeHTMLView.Coordinator()
        for address in ["file:///etc/passwd", "smb://server/share", "vnc://server", "x-apple.systempreferences:privacy", "wbmc-custom:secret"] {
            let decision = coordinator.policy(for: URL(string: address), type: .linkActivated)
            #expect(decision.navigation == .cancel)
            #expect(decision.urlToOpen == nil)
        }
        for address in ["https://example.com", "HTTP://example.com", "HtTpS://example.com", "mailto:reader@example.com", "MAILTO:reader@example.com"] {
            let url = URL(string: address)!
            let decision = coordinator.policy(for: url, type: .linkActivated)
            #expect(decision.navigation == .cancel)
            #expect(decision.urlToOpen == url)
        }
    }

    @Test func securityMetadataIsFirstInHeadAndSpeculativeLinksAreRemoved() {
        let html = """
        <!DOCTYPE html><HTML><HEAD data-test="a > b"><title>Mail</title>
        <LiNk ReL="dns-prefetch" href="//tracker.example.com">
        <link href='https://tracker.example.com' rel='PRECONNECT'>
        <link rel="prefetch preload prerender modulepreload" href="https://tracker.example.com">
        <link rel="alternate stylesheet" href="https://tracker.example.com/mail.css">
        <meta HTTP-EQUIV='X-DNS-PREFETCH-CONTROL' content='on'>
        <link rel="icon" href="data:image/png;base64,AA==">
        </HEAD><body>Keep this mail.</body></HTML>
        """
        let secured = SafeHTMLView.securedHTML(html)
        #expect(secured.hasPrefix("<!DOCTYPE html><HTML><HEAD data-test=\"a > b\">" + SafeHTMLView.securityMetadata))
        #expect(!secured.contains("tracker.example.com"))
        #expect(!secured.contains("content='on'"))
        #expect(secured.contains("data:image/png;base64,AA=="))
        #expect(secured.contains("Keep this mail."))
    }

    @Test func fragmentsGetASecurityHeadAndEncodedHintsAreRemoved() {
        let secured = SafeHTMLView.securedHTML("""
        <link rel='dns&#45;prefetch' href='//tracker.example.com'>
        <link rel=stylesheet href='h&#116;tps://tracker.example.com/style.css'>
        <meta http-equiv='x-dns-prefetch-contr&#111;l' content=on><p>Message</p>
        """)
        #expect(secured.hasPrefix("<head>" + SafeHTMLView.securityMetadata + "</head>"))
        #expect(!secured.contains("tracker.example.com"))
        #expect(!secured.contains("content=on"))
        #expect(secured.hasSuffix("<p>Message</p>"))
    }

    @Test func commentsAndRawTextCannotImpersonateTheHead() {
        let html = "<!-- <head> --> <script>const fake = '<head>';</script><HEAD><title>Mail</title></HEAD>"
        let secured = SafeHTMLView.securedHTML(html)
        #expect(secured.contains("<HEAD>" + SafeHTMLView.securityMetadata + "<title>"))
    }

    @Test func ruleListBlocksEveryNonLocalScheme() throws {
        let rules = try #require(JSONSerialization.jsonObject(with: Data(SafeHTMLView.contentRulesJSON.utf8)) as? [[String: Any]])
        #expect(rules.count == 3)
        for (url, expectedBlocked) in [("ws://tracker.example.com", true), ("ftp://tracker.example.com", true),
                                       ("https://tracker.example.com", true), ("data:image/png;base64,AA==", false), ("about:blank", false)] {
            var blocked = false
            for rule in rules {
                let trigger = try #require(rule["trigger"] as? [String: Any])
                let action = try #require(rule["action"] as? [String: String])
                let pattern = try #require(trigger["url-filter"] as? String)
                let regex = try NSRegularExpression(pattern: pattern)
                if regex.firstMatch(in: url, range: NSRange(url.startIndex..., in: url)) != nil {
                    blocked = action["type"] == "block"
                }
            }
            #expect(blocked == expectedBlocked)
        }
    }
}
