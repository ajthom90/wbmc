// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AppKit
import Network
import Synchronization
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

    @Test func remoteSubresourceNeverReachesServer() async throws {
        let parameters = NWParameters.tcp
        parameters.requiredInterfaceType = .loopback
        if let internet = parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            internet.version = .v4
        }
        let listener = try NWListener(using: parameters, on: .any)
        let accepted = Mutex(0)
        listener.newConnectionHandler = { connection in
            accepted.withLock { $0 += 1 }
            connection.cancel()
        }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let resumed = Mutex(false)
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    let first = resumed.withLock { value -> Bool in
                        if value { return false }
                        value = true
                        return true
                    }
                    if first { continuation.resume() }
                case .failed(let error):
                    let first = resumed.withLock { value -> Bool in
                        if value { return false }
                        value = true
                        return true
                    }
                    if first { continuation.resume(throwing: error) }
                default:
                    break
                }
            }
            listener.start(queue: .global(qos: .userInitiated))
        }
        defer { listener.cancel() }
        let port = try #require(listener.port)
        let probe = NWConnection(host: .ipv4(try #require(IPv4Address("127.0.0.1"))), port: port, using: .tcp)
        probe.start(queue: .global(qos: .userInitiated))
        var probeCount = 0
        for _ in 0..<100 {
            probeCount = accepted.withLock { $0 }
            if probeCount >= 1 { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        probe.cancel()
        #expect(probeCount == 1)
        accepted.withLock { $0 = 0 }
        let address = "http://127.0.0.1:\(port.rawValue)/x.png"
        let html = """
        <html><head>
        <link rel="stylesheet" href="\(address)">
        <style>body { background-image: url("\(address)"); }</style>
        </head><body>
        <img src="\(address)" srcset="\(address) 1x, \(address) 2x">
        </body></html>
        """
        let coordinator = SafeHTMLView.Coordinator()
        let webView = SafeHTMLView.makeSecureWebView(coordinator: coordinator)
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = webView
        defer { window.contentView = nil; webView.stopLoading() }
        await coordinator.load(html: html, in: webView)
        try await Task.sleep(for: .seconds(2))
        #expect(coordinator.didFinish)
        #expect(accepted.withLock { $0 } == 0)
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

    @Test func securityMetadataFollowsLeadingDoctypeAndRemoteLinksAreRemoved() {
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
        #expect(secured.hasPrefix("<!DOCTYPE html>" + SafeHTMLView.securityMetadata + "<HTML><HEAD data-test=\"a > b\">"))
        #expect(!secured.contains("tracker.example.com"))
        #expect(!secured.contains("content='on'"))
        #expect(secured.contains("data:image/png;base64,AA=="))
        #expect(secured.contains("Keep this mail."))
    }

    @Test func fragmentsStartWithMetadataAndEncodedHintsAreRemoved() {
        let secured = SafeHTMLView.securedHTML("""
        <link rel='dns&#45;prefetch' href='//tracker.example.com'>
        <link rel=stylesheet href='h&#116;tps://tracker.example.com/style.css'>
        <meta http-equiv='x-dns-prefetch-contr&#111;l' content=on><p>Message</p>
        """)
        #expect(secured.hasPrefix(SafeHTMLView.securityMetadata))
        #expect(!secured.contains("tracker.example.com"))
        #expect(!secured.contains("content=on"))
        #expect(secured.hasSuffix("<p>Message</p>"))
    }

    @Test func commentsAndRawTextCannotImpersonateTheHead() {
        let html = "<!-- <head> --> <script>const fake = '<head>';</script><HEAD><title>Mail</title></HEAD>"
        let secured = SafeHTMLView.securedHTML(html)
        #expect(secured.hasPrefix(SafeHTMLView.securityMetadata + html))
        #expect(secured.contains("const fake = '<head>';"))
    }

    @Test func bareLessThanWithAnUnterminatedQuoteCannotHidePreconnect() {
        for html in [#"a <= "b <link rel=preconnect href=https://x>"#,
                     #"a <= "b <link rel=preconnect href=https://x>" later <p>Keep</p>"#] {
            let secured = SafeHTMLView.securedHTML(html)
            #expect(!secured.contains("<link"))
            #expect(!secured.contains("https://x"))
            #expect(secured.contains(#"a <= "b "#))
        }
    }

    @Test func quotesInTagAndAttributeNamesCannotHideLaterHints() {
        for html in [#"<odd= "><link rel=preconnect href=https://x>"#,
                     #"<div ="><link rel=preconnect href=https://x>"#] {
            let secured = SafeHTMLView.securedHTML(html)
            #expect(!secured.contains("<link"))
            #expect(!secured.contains("https://x"))
        }
    }

    @Test func lateHeadCannotPlaceCSPAfterAnImage() {
        let secured = SafeHTMLView.securedHTML("<img src=https://x><head><title>Mail</title></head>")
        #expect(secured.hasPrefix(SafeHTMLView.securityMetadata + "<img"))
    }

    @Test func securityMetadataImmediatelyFollowsTheLeadingDoctype() {
        let prefix = "<!DOCTYPE html><!-- lead --><html lang='en'>\n"
        let secured = SafeHTMLView.securedHTML(prefix + "<img src=https://x><head></head>")
        #expect(secured.hasPrefix("<!DOCTYPE html>" + SafeHTMLView.securityMetadata + "<!-- lead --><html lang='en'>\n<img"))
    }

    @Test func emptyCommentCannotHidePreconnect() {
        let secured = SafeHTMLView.securedHTML("<!--><link rel=preconnect href=https://x>-->")
        #expect(!secured.contains("<link"))
        #expect(!secured.contains("https://x"))
        #expect(secured.hasPrefix(SafeHTMLView.securityMetadata + "<!-->"))
    }

    @Test func abruptlyClosedCommentCannotHideDNSPrefetch() {
        let secured = SafeHTMLView.securedHTML("<!---><link rel=dns-prefetch href=//x>")
        #expect(!secured.contains("<link"))
        #expect(!secured.contains("//x"))
    }

    @Test func bangCommentEndCannotHidePreload() {
        let secured = SafeHTMLView.securedHTML("<!-- --!><link rel=preload href=https://x>")
        #expect(!secured.contains("<link"))
        #expect(!secured.contains("https://x"))
    }

    @Test func unterminatedRealTagIsDroppedAtEOF() {
        let secured = SafeHTMLView.securedHTML(#"<p>Keep</p><link rel=preconnect href="https://x"#)
        #expect(!secured.contains("<link"))
        #expect(!secured.contains("https://x"))
        #expect(secured.hasSuffix("<p>Keep</p>"))
    }

    @Test func bogusCommentsEndAtTheFirstGreaterThanDespiteQuotes() {
        for html in [#"<!bogus "><link rel=preconnect href=https://x>"#,
                     #"<?bogus "><link rel=preload href=https://x>"#] {
            let secured = SafeHTMLView.securedHTML(html)
            #expect(!secured.contains("<link"))
            #expect(!secured.contains("https://x"))
        }
    }

    @Test func unterminatedLeadingCommentDoesNotSwallowCSP() {
        let secured = SafeHTMLView.securedHTML("<!-- unclosed")
        #expect(secured.hasPrefix(SafeHTMLView.securityMetadata + "<!-- unclosed"))
    }

    @Test func metadataIsAtByteZeroWithoutALeadingDoctype() {
        for html in ["", " \n", "<!-- c --><!DOCTYPE html>",
                     "<img src=https://x><head>", "<!-- unclosed", #"a <= "b"#] {
            let secured = SafeHTMLView.securedHTML(html)
            #expect(Array(secured.utf8.prefix(SafeHTMLView.securityMetadata.utf8.count)) == Array(SafeHTMLView.securityMetadata.utf8))
        }
    }

    @Test func allNonDataLinksAreRemovedRegardlessOfRel() {
        for tag in ["<link rel=stylesheet href=https://x/a.css>",
                    "<link rel=&#112;reconnect href=https://x>",
                    "<link rel=unknown href=ftp://x/a>", "<link rel=icon>", "<link>"] {
            #expect(SafeHTMLView.securedHTML(tag) == SafeHTMLView.securityMetadata)
        }
    }

    @Test func dataLinksArePreservedAfterDecodingAndLeadingWhitespace() {
        for tag in [#"<link rel=icon href="data:image/png;base64,AA==">"#,
                    #"<link rel=icon href="  DaTa:image/png;base64,AA==">"#,
                    #"<link rel=icon href="&#9;&#100;ata:image/png;base64,AA==">"#] {
            #expect(SafeHTMLView.securedHTML(tag) == SafeHTMLView.securityMetadata + tag)
        }
    }

    @Test func refreshAndBaseTagsAreRemoved() {
        let html = "<base href=https://x/><meta http-equiv=refresh content='0;url=https://x'>"
            + "<META HTTP-EQUIV='ref&#114;esh' content='0;url=https://x'><p>Keep</p>"
        #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + "<p>Keep</p>")
    }

    @Test func foreignContentCannotHideResourceOrNavigationTags() {
        for (html, expected) in [
            ("<svg><title><link rel=preconnect href=https://x></title></svg>", "<svg><title></title></svg>"),
            ("<svg><style><meta http-equiv=x-dns-prefetch-control content=on></style></svg>", "<svg><style></style></svg>"),
            (#"<svg><style><meta http-equiv=refresh content="0;url=https://x">"#, "<svg><style>"),
            ("<svg><style/><link rel=preconnect href=https://x>", "<svg><style/>"),
            ("<math><style><base href=https://x><link href=https://x></style></math>", "<math><style></style></math>")
        ] {
            #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + expected)
        }
    }

    @Test func filteringInsideRawTextDoesNotChangeItsClosingTag() {
        let html = "<style><link href=data:image/png;base64,AA==><script></style>"
            + "<!--<link href=https://comment>--><p>Keep</p>"
        #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + html)
    }

    @Test func rawTextDoesNotProcessCommentsOrBogusComments() {
        let html = "<style><!--<link href=https://x><?bogus <meta http-equiv=refresh content=on>"
            + "<base href=https://x></style>"
        #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + "<style><!--<?bogus </style>")
    }

    @Test func resourceTagSpanningRawTextCloserFailsClosed() {
        let raw = #"<style>some <link href="unfinished</style><p>Gone</p>"#
        #expect(SafeHTMLView.securedHTML(raw + "<link href=https://x>") == SafeHTMLView.securityMetadata + "<style>some ")
        let script = #"<script><meta content="unfinished"#
        #expect(SafeHTMLView.securedHTML(script) == SafeHTMLView.securityMetadata + "<script>")
        let foreign = #"<svg><title><link rel=preconnect a="</title><!--" href=https://x>--><p>t</p>"#
        let secured = SafeHTMLView.securedHTML(foreign)
        #expect(!secured.contains("https://x"))
        #expect(!secured.contains("<link"))
    }

    @Test func splitScriptClosingTextRemainsUnchanged() {
        let html = #"<script>var s = "</scr" + "ipt>";</script>"#
        #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + html)
    }

    @Test func noscriptCommentsCannotHideLinksAfterItsClosingTag() {
        let html = "<noscript><!--</noscript><link rel=preconnect href=https://x>"
        #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + "<noscript><!--</noscript>")
    }

    @Test func onlyWhitespaceAndOneLeadingDoctypeCanPrecedeMetadata() {
        for prefix in ["<!DOCTYPE html>", " \n<!dOcTyPe html>"] {
            #expect(SafeHTMLView.securedHTML(prefix + "<html></html>") == prefix + SafeHTMLView.securityMetadata + "<html></html>")
        }
        for html in ["<!-- c --><!DOCTYPE html>", "<html><!DOCTYPE html>", "<!DOCTYPE html", "<!doctype-not-a-token>"] {
            #expect(SafeHTMLView.securedHTML(html).hasPrefix(SafeHTMLView.securityMetadata))
        }
        #expect(SafeHTMLView.securedHTML("<!DOCTYPE html><!DOCTYPE html>") == "<!DOCTYPE html>" + SafeHTMLView.securityMetadata + "<!DOCTYPE html>")
    }

    @Test func malformedLinkAttributeNamesAreRejected() {
        for html in ["<link =href=data:x href=https://evil>", "<link =other=x href=data:x>",
                     #"<link bad"name=x href=data:x>"#, "<link bad'name=x href=data:x>",
                     "<link bad<name=x href=data:x>"] {
            #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata)
        }
    }

    @Test func duplicateHrefUsesOnlyTheFirstOccurrence() {
        for html in ["<link href=https://evil href=data:x>", "<link HREF=https://evil href=data:x>",
                     "<link href href=data:x>"] {
            #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata)
        }
        for html in ["<link href=data:x href=https://evil>", #"<link href='data:x' HREF="https://evil">"#,
                     #"<link href="data:x" href=https://evil>"#] {
            #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata + html)
        }
    }

    @Test func metaDirectivesUseFirstOccurrenceAndRejectAmbiguousNames() {
        for html in [#"<meta http-equiv=refresh http-equiv=x content="0;url=https://x">"#,
                     "<meta =http-equiv=x content=on>", "<meta http-equiv=x bad'name=value>",
                     #"<meta http-equiv=x bad"name=value>"#, "<meta http-equiv=x bad<name=value>"] {
            #expect(SafeHTMLView.securedHTML(html) == SafeHTMLView.securityMetadata)
        }
        let safe = "<meta HTTP-EQUIV=x http-equiv=refresh content='0;url=https://x'>"
        #expect(SafeHTMLView.securedHTML(safe) == SafeHTMLView.securityMetadata + safe)
        let unrelated = "<meta charset=utf-8 bad'name=value>"
        #expect(SafeHTMLView.securedHTML(unrelated) == SafeHTMLView.securityMetadata + unrelated)
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
