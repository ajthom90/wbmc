// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AppKit
import SwiftUI
import WebKit

@MainActor
struct SafeHTMLView: NSViewRepresentable {
    let html: String

    static let securityMetadata = #"<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:; media-src data:"><meta http-equiv="x-dns-prefetch-control" content="off">"#
    static let contentRulesJSON = #"[{"trigger":{"url-filter":".*"},"action":{"type":"block"}},{"trigger":{"url-filter":"^data:"},"action":{"type":"ignore-previous-rules"}},{"trigger":{"url-filter":"^about:"},"action":{"type":"ignore-previous-rules"}}]"#

    static func securedHTML(_ html: String) -> String {
        var scanner = MailHTMLScanner(html)
        return scanner.securedHTML(metadata: securityMetadata)
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> WKWebView {
        Self.makeSecureWebView(coordinator: context.coordinator)
    }

    func updateNSView(_ webView: WKWebView, context: Context) {
        guard context.coordinator.requestedHTML != html else { return }
        context.coordinator.requestedHTML = html
        context.coordinator.loadTask?.cancel()
        context.coordinator.loadTask = Task { @MainActor in
            await context.coordinator.load(html: html, in: webView)
        }
    }

    static func dismantleNSView(_ webView: WKWebView, coordinator: Coordinator) {
        coordinator.loadTask?.cancel()
        webView.stopLoading()
        webView.navigationDelegate = nil
    }

    static func makeSecureWebView(coordinator: Coordinator) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        let preferences = WKWebpagePreferences()
        preferences.allowsContentJavaScript = false
        configuration.defaultWebpagePreferences = preferences
        configuration.websiteDataStore = .nonPersistent()
        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = coordinator
        return webView
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate {
        // All coordinators await the same compilation, including concurrent readers.
        private typealias RuleCompilation = (id: UUID, task: Task<WKContentRuleList, any Error>)
        private static var ruleTask: RuleCompilation?
        private static var legacyRuleRemovalAttempted = false

        private static func blockingRules() async throws -> WKContentRuleList {
            let compilation: RuleCompilation
            if let cached = ruleTask {
                compilation = cached
            } else {
                let task = Task { @MainActor in
                    if !legacyRuleRemovalAttempted {
                        legacyRuleRemovalAttempted = true
                        try? await WKContentRuleListStore.default().removeContentRuleList(forIdentifier: "WBMCBlockRemoteContent-v1")
                    }
                    guard let rule = try await WKContentRuleListStore.default().compileContentRuleList(
                        forIdentifier: "WBMCBlockRemoteContent-v2",
                        encodedContentRuleList: SafeHTMLView.contentRulesJSON) else {
                        throw CocoaError(.fileReadUnknown)
                    }
                    return rule
                }
                compilation = (UUID(), task)
                ruleTask = compilation
            }
            do {
                return try await compilation.task.value
            } catch {
                // An old waiter must not discard a newer retry's compilation.
                if ruleTask?.id == compilation.id { ruleTask = nil }
                throw error
            }
        }

        var requestedHTML: String?
        var loadTask: Task<Void, Never>?
        private var initialNavigation: WKNavigation?
        private var allowsInitialNavigation = false
        private(set) var rulesInstalled = false
        private(set) var didFinish = false
        private(set) var loadError: String?
        private(set) var navigationURLs: [URL] = []

        func load(html: String, in webView: WKWebView) async {
            webView.stopLoading()
            didFinish = false
            allowsInitialNavigation = false
            loadError = nil
            do {
                let rules = try await Self.blockingRules()
                guard !Task.isCancelled else { return }
                webView.configuration.userContentController.removeAllContentRuleLists()
                webView.configuration.userContentController.add(rules)
                rulesInstalled = true
                allowsInitialNavigation = true
                initialNavigation = webView.loadHTMLString(SafeHTMLView.securedHTML(html), baseURL: nil)
            } catch {
                // Fail closed: never load mail without the rule list.
                loadError = "Couldn’t safely display this message: \(error.localizedDescription)"
            }
        }

        func policy(for url: URL?, type: WKNavigationType) -> (navigation: WKNavigationActionPolicy, urlToOpen: URL?) {
            if type == .linkActivated {
                let allowed = ["http", "https", "mailto"].contains(url?.scheme?.lowercased() ?? "")
                return (.cancel, allowed ? url : nil)
            }
            if allowsInitialNavigation, type == .other, url == nil || url?.absoluteString == "about:blank" {
                allowsInitialNavigation = false
                return (.allow, nil)
            }
            return (.cancel, nil)
        }

        func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction) async -> WKNavigationActionPolicy {
            if let url = navigationAction.request.url { navigationURLs.append(url) }
            guard navigationAction.navigationType == .linkActivated || navigationAction.targetFrame?.isMainFrame == true else {
                return .cancel
            }
            let decision = policy(for: navigationAction.request.url, type: navigationAction.navigationType)
            if let url = decision.urlToOpen { NSWorkspace.shared.open(url) }
            return decision.navigation
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            guard navigation === initialNavigation else { return }
            didFinish = true
            allowsInitialNavigation = false
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: any Error) {
            guard navigation === initialNavigation else { return }
            loadError = error.localizedDescription
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: any Error) {
            guard navigation === initialNavigation else { return }
            loadError = error.localizedDescription
        }
    }
}

/// A forward-only UTF-8 tag scan. Each tag/attribute is scanned a bounded number
/// of times; quoted '>' characters, comments and raw-text elements stay intact.
private struct MailHTMLScanner {
    private let bytes: [UInt8]

    init(_ html: String) { bytes = Array(html.utf8) }

    private struct Tag {
        let name: String
        let closing: Bool
        let attributesStart: Int
        let end: Int
    }

    private static let commentStart = Array("<!--".utf8)
    private static let commentClose = Array("-->".utf8)
    private static let bangCommentClose = Array("--!>".utf8)
    private static let doctypeStart = Array("<!doctype".utf8)
    private static let rawTextClosers = Dictionary(uniqueKeysWithValues:
        ["script", "style", "title", "textarea", "xmp", "iframe", "noembed", "noframes", "plaintext"].map {
            ($0, Array(("</" + $0).utf8))
        })

    mutating func securedHTML(metadata: String) -> String {
        var output: [UInt8] = []
        output.reserveCapacity(bytes.count + metadata.utf8.count)
        // Install policy before any untrusted bytes, independently of how
        // WebKit tokenizes malformed mail or handles later html/head tags.
        output.append(contentsOf: metadata.utf8)
        var rawText: (name: String, close: [UInt8])?
        var index = 0
        var copiedThrough = 0
        while index < bytes.count {
            guard bytes[index] == 60 else { // <
                index += 1
                continue
            }
            if let raw = rawText {
                // Close-tag bytes are precomputed once, never per raw-text '<'.
                guard matches(raw.close, at: index, ignoringCase: true),
                      index + raw.close.count < bytes.count,
                      isDelimiter(bytes[index + raw.close.count]) else { index += 1; continue }
            } else {
                if matches(Self.commentStart, at: index) {
                    guard let end = commentEnd(at: index) else { break }
                    index = end
                    continue
                }
                if matches(Self.doctypeStart, at: index, ignoringCase: true) {
                    guard let end = declarationEnd(at: index + Self.doctypeStart.count) else { break }
                    index = end
                    continue
                }
                let next = index + 1 < bytes.count ? bytes[index + 1] : nil
                if next == 33 || next == 63 || (next == 47 && !isTagStart(at: index)) {
                    // Bogus comments ignore quotes and end at the first '>'.
                    guard let end = declarationEnd(at: index + 2) else { break }
                    index = end
                    continue
                }
                guard isTagStart(at: index) else {
                    // A bare '<' is text; quotes after it cannot hide real tags.
                    index += 1
                    continue
                }
            }
            guard let tag = tag(at: index) else {
                // HTML does not emit a real tag left incomplete at EOF. Drop it
                // rather than leaving an unscanned suffix in the secured output.
                output.append(contentsOf: bytes[copiedThrough..<index])
                copiedThrough = bytes.count
                break
            }
            let attributes = (tag.name == "link" || tag.name == "meta") ? attributes(of: tag) : [:]
            let remove: Bool
            if !tag.closing, tag.name == "link" {
                let href = (attributes["href"] ?? "").drop(while: { $0.isWhitespace }).lowercased()
                remove = !href.hasPrefix("data:")
            } else if !tag.closing, tag.name == "meta" {
                let directive = attributes["http-equiv"]?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
                remove = directive == "x-dns-prefetch-control" || directive == "refresh"
            } else if !tag.closing, tag.name == "base" {
                remove = true
            } else {
                remove = false
            }
            if remove {
                output.append(contentsOf: bytes[copiedThrough..<index])
                copiedThrough = tag.end
            }
            if tag.closing, tag.name == rawText?.name {
                rawText = nil
            } else if !tag.closing, let close = Self.rawTextClosers[tag.name] {
                rawText = (tag.name, close)
            }
            index = tag.end
        }
        output.append(contentsOf: bytes[copiedThrough...])
        return String(decoding: output, as: UTF8.self)
    }

    private func isASCIILetter(_ byte: UInt8) -> Bool {
        (byte >= 65 && byte <= 90) || (byte >= 97 && byte <= 122)
    }

    private func isTagStart(at index: Int) -> Bool {
        guard index + 1 < bytes.count else { return false }
        if isASCIILetter(bytes[index + 1]) { return true }
        return bytes[index + 1] == 47 && index + 2 < bytes.count && isASCIILetter(bytes[index + 2])
    }

    private func declarationEnd(at start: Int) -> Int? {
        var index = start
        while index < bytes.count {
            if bytes[index] == 62 { return index + 1 }
            index += 1
        }
        return nil
    }

    private func commentEnd(at start: Int) -> Int? {
        var index = start + Self.commentStart.count
        // HTML's comment-start and comment-start-dash states accept these
        // abrupt empty endings before entering the normal comment state.
        if index < bytes.count && bytes[index] == 62 { return index + 1 }
        if index + 1 < bytes.count && bytes[index] == 45 && bytes[index + 1] == 62 { return index + 2 }
        while index < bytes.count {
            if matches(Self.commentClose, at: index) { return index + Self.commentClose.count }
            if matches(Self.bangCommentClose, at: index) { return index + Self.bangCommentClose.count }
            index += 1
        }
        return nil
    }

    private enum AttributeState {
        case beforeName, name, afterName, beforeValue, quoted(UInt8), unquoted
    }

    private func tag(at start: Int) -> Tag? {
        guard isTagStart(at: start) else { return nil }
        let closing = bytes[start + 1] == 47
        let nameStart = start + (closing ? 2 : 1)
        var nameEnd = nameStart
        while nameEnd < bytes.count && !isDelimiter(bytes[nameEnd]) { nameEnd += 1 }
        var end = nameEnd
        var state = AttributeState.beforeName
        while end < bytes.count {
            let byte = bytes[end]
            // Only attribute values recognize quotes. Quotes or '=' in a tag
            // name/attribute name cannot swallow the next real tag.
            if case .quoted(let quote) = state {
                if byte == quote { state = .beforeName }
            } else if byte == 62 {
                return Tag(name: String(decoding: bytes[nameStart..<nameEnd], as: UTF8.self).lowercased(),
                           closing: closing, attributesStart: nameEnd, end: end + 1)
            } else {
                switch state {
                case .beforeName:
                    if !isSpace(byte) && byte != 47 { state = .name }
                case .name:
                    if isSpace(byte) { state = .afterName }
                    else if byte == 47 { state = .beforeName }
                    else if byte == 61 { state = .beforeValue }
                case .afterName:
                    if byte == 61 { state = .beforeValue }
                    else if byte == 47 { state = .beforeName }
                    else if !isSpace(byte) { state = .name }
                case .beforeValue:
                    if byte == 34 || byte == 39 { state = .quoted(byte) }
                    else if !isSpace(byte) { state = .unquoted }
                case .unquoted:
                    if isSpace(byte) { state = .beforeName }
                case .quoted:
                    break
                }
            }
            end += 1
        }
        return nil
    }

    private func attributes(of tag: Tag) -> [String: String] {
        var result: [String: String] = [:]
        var index = tag.attributesStart
        let end = tag.end - 1
        while index < end {
            while index < end && (isSpace(bytes[index]) || bytes[index] == 47) { index += 1 }
            let nameStart = index
            while index < end && !isSpace(bytes[index]) && bytes[index] != 61 && bytes[index] != 47 { index += 1 }
            guard index > nameStart else { index += 1; continue }
            let name = String(decoding: bytes[nameStart..<index], as: UTF8.self).lowercased()
            while index < end && isSpace(bytes[index]) { index += 1 }
            var value = ""
            if index < end && bytes[index] == 61 { // =
                index += 1
                while index < end && isSpace(bytes[index]) { index += 1 }
                if index < end && (bytes[index] == 34 || bytes[index] == 39) {
                    let quote = bytes[index]
                    index += 1
                    let valueStart = index
                    while index < end && bytes[index] != quote { index += 1 }
                    value = Self.decodeAttribute(bytes[valueStart..<index])
                    if index < end { index += 1 }
                } else {
                    let valueStart = index
                    while index < end && !isSpace(bytes[index]) { index += 1 }
                    value = Self.decodeAttribute(bytes[valueStart..<index])
                }
            }
            // HTML uses the first occurrence of a duplicated attribute.
            if result[name] == nil { result[name] = value }
        }
        return result
    }

    private func matches(_ prefix: [UInt8], at index: Int, ignoringCase: Bool = false) -> Bool {
        guard prefix.count <= bytes.count - index else { return false }
        for offset in prefix.indices {
            let byte = bytes[index + offset]
            let normalized = ignoringCase && byte >= 65 && byte <= 90 ? byte + 32 : byte
            if normalized != prefix[offset] { return false }
        }
        return true
    }

    private func isSpace(_ byte: UInt8) -> Bool { [9, 10, 12, 13, 32].contains(byte) }
    private func isDelimiter(_ byte: UInt8) -> Bool { isSpace(byte) || byte == 47 || byte == 62 }

    /// Decode numeric references (including references without a semicolon) and
    /// relevant named ASCII references without rescanning an unbounded suffix.
    private static func decodeAttribute(_ slice: ArraySlice<UInt8>) -> String {
        let input = Array(slice)
        var output: [UInt8] = []
        var index = 0
        let named: [(String, UInt8)] = [("amp;", 38), ("quot;", 34), ("apos;", 39), ("lt;", 60), ("gt;", 62),
                                         ("colon;", 58), ("sol;", 47), ("bsol;", 92), ("Tab;", 9), ("NewLine;", 10)]
        while index < input.count {
            if input[index] == 38, index + 1 < input.count {
                var cursor = index + 1
                if input[cursor] == 35 { // #
                    cursor += 1
                    let hex = cursor < input.count && (input[cursor] == 120 || input[cursor] == 88)
                    if hex { cursor += 1 }
                    let digitsStart = cursor
                    var value: UInt32 = 0
                    while cursor < input.count {
                        let byte = input[cursor]
                        let digit: UInt32
                        if byte >= 48 && byte <= 57 { digit = UInt32(byte - 48) }
                        else if hex && byte >= 65 && byte <= 70 { digit = UInt32(byte - 65 + 10) }
                        else if hex && byte >= 97 && byte <= 102 { digit = UInt32(byte - 97 + 10) }
                        else { break }
                        value = min(0x110000, value * (hex ? 16 : 10) + digit)
                        cursor += 1
                    }
                    if cursor > digitsStart {
                        if cursor < input.count && input[cursor] == 59 { cursor += 1 }
                        let scalar = value == 0 ? UnicodeScalar(0xFFFD)! : UnicodeScalar(value) ?? UnicodeScalar(0xFFFD)!
                        output.append(contentsOf: String(scalar).utf8)
                        index = cursor
                        continue
                    }
                } else if let entity = named.first(where: { name, _ in
                    let entityBytes = Array(name.utf8)
                    return entityBytes.count <= input.count - cursor && input[cursor..<(cursor + entityBytes.count)].elementsEqual(entityBytes)
                }) {
                    output.append(entity.1)
                    index = cursor + entity.0.utf8.count
                    continue
                }
            }
            output.append(input[index])
            index += 1
        }
        return String(decoding: output, as: UTF8.self)
    }
}
