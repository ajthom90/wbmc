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

        private static func blockingRules() async throws -> WKContentRuleList {
            let compilation: RuleCompilation
            if let cached = ruleTask {
                compilation = cached
            } else {
                let task = Task { @MainActor in
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

    mutating func securedHTML(metadata: String) -> String {
        var output: [UInt8] = []
        output.reserveCapacity(bytes.count + metadata.utf8.count)
        var headInsertion: Int?
        var rawText: String?
        var index = 0
        var copiedThrough = 0
        while index < bytes.count {
            guard bytes[index] == 60 else { index += 1; continue } // <
            if let raw = rawText {
                // Do not repeatedly scan raw-text '<' characters for a distant '>'.
                let prefix = Array(("</" + raw).utf8)
                guard matches(prefix, at: index, ignoringCase: true),
                      index + prefix.count < bytes.count,
                      isDelimiter(bytes[index + prefix.count]) else { index += 1; continue }
            } else if matches(Array("<!--".utf8), at: index) {
                var end = index + 4
                while end < bytes.count && !matches(Array("-->".utf8), at: end) { end += 1 }
                index = min(bytes.count, end + 3)
                continue
            }
            guard let tag = tag(at: index) else { break }
            let attributes = (tag.name == "link" || tag.name == "meta") ? attributes(of: tag) : [:]
            let remove: Bool
            if !tag.closing, tag.name == "link" {
                let rel = Set((attributes["rel"] ?? "").lowercased().split(whereSeparator: { $0.isWhitespace }).map(String.init))
                let hints: Set<String> = ["dns-prefetch", "preconnect", "prefetch", "preload", "prerender", "modulepreload"]
                let href = (attributes["href"] ?? "").lowercased().filter { !$0.isWhitespace && !$0.isASCIIControl }
                remove = !rel.isDisjoint(with: hints) || (rel.contains("stylesheet") &&
                    (href.hasPrefix("http:") || href.hasPrefix("https:") || href.hasPrefix("//")))
            } else if !tag.closing, tag.name == "meta" {
                remove = attributes["http-equiv"]?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() == "x-dns-prefetch-control"
            } else {
                remove = false
            }
            if remove {
                output.append(contentsOf: bytes[copiedThrough..<index])
                copiedThrough = tag.end
            }
            if !tag.closing, tag.name == "head", headInsertion == nil {
                output.append(contentsOf: bytes[copiedThrough..<tag.end])
                copiedThrough = tag.end
                headInsertion = output.count
            }
            if tag.closing, tag.name == rawText {
                rawText = nil
            } else if !tag.closing, ["script", "style", "title", "textarea", "xmp", "iframe", "noembed", "noframes", "plaintext"].contains(tag.name) {
                rawText = tag.name
            }
            index = tag.end
        }
        output.append(contentsOf: bytes[copiedThrough...])
        if let headInsertion {
            output.insert(contentsOf: metadata.utf8, at: headInsertion)
            return String(decoding: output, as: UTF8.self)
        }
        return "<head>" + metadata + "</head>" + String(decoding: output, as: UTF8.self)
    }

    private func tag(at start: Int) -> Tag? {
        var end = start + 1
        var quote: UInt8?
        var awaitingValue = false
        var unquotedValue = false
        while end < bytes.count {
            let byte = bytes[end]
            if let current = quote {
                if byte == current { quote = nil }
            } else if byte == 62 { break } // >
            else if unquotedValue {
                if isSpace(byte) { unquotedValue = false }
            } else if awaitingValue {
                if !isSpace(byte) {
                    if byte == 34 || byte == 39 { quote = byte }
                    else { unquotedValue = true }
                    awaitingValue = false
                }
            } else if byte == 61 {
                awaitingValue = true
            }
            end += 1
        }
        guard end < bytes.count else { return nil }
        var nameStart = start + 1
        let closing = bytes[nameStart] == 47
        if closing { nameStart += 1 }
        var nameEnd = nameStart
        while nameEnd < end && !isDelimiter(bytes[nameEnd]) { nameEnd += 1 }
        return Tag(name: String(decoding: bytes[nameStart..<nameEnd], as: UTF8.self).lowercased(),
                   closing: closing, attributesStart: nameEnd, end: end + 1)
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

private extension Character {
    var isASCIIControl: Bool { unicodeScalars.allSatisfy { $0.value < 32 || $0.value == 127 } }
}
