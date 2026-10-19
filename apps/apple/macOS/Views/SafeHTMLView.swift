// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AppKit
import SwiftUI
import WebKit

@MainActor
struct SafeHTMLView: NSViewRepresentable {
    let html: String

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
        private static var ruleTask: Task<WKContentRuleList, any Error>?
        private static func blockingRules() async throws -> WKContentRuleList {
            if let task = ruleTask { return try await task.value }
            let task = Task { @MainActor in
                guard let rule = try await WKContentRuleListStore.default().compileContentRuleList(
                    forIdentifier: "WBMCBlockRemoteContent-v1",
                    encodedContentRuleList: #"[{"trigger":{"url-filter":"^https?://"},"action":{"type":"block"}}]"#) else {
                    throw CocoaError(.fileReadUnknown)
                }
                return rule
            }
            ruleTask = task
            return try await task.value
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
            loadError = nil
            do {
                let rules = try await Self.blockingRules()
                guard !Task.isCancelled else { return }
                webView.configuration.userContentController.removeAllContentRuleLists()
                webView.configuration.userContentController.add(rules)
                rulesInstalled = true
                allowsInitialNavigation = true
                // Mail HTML is intentionally loaded unchanged, without CSS or a wrapper.
                initialNavigation = webView.loadHTMLString(html, baseURL: nil)
            } catch {
                // Fail closed: never load mail without the rule list.
                loadError = "Couldn’t safely display this message: \(error.localizedDescription)"
            }
        }

        func policy(for url: URL?, type: WKNavigationType) -> WKNavigationActionPolicy {
            if type == .linkActivated {
                if let url { NSWorkspace.shared.open(url) }
                return .cancel
            }
            if allowsInitialNavigation, type == .other, url == nil || url?.absoluteString == "about:blank" {
                allowsInitialNavigation = false
                return .allow
            }
            return .cancel
        }

        func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction) async -> WKNavigationActionPolicy {
            if let url = navigationAction.request.url { navigationURLs.append(url) }
            guard navigationAction.navigationType == .linkActivated || navigationAction.targetFrame?.isMainFrame == true else {
                return .cancel
            }
            return policy(for: navigationAction.request.url, type: navigationAction.navigationType)
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
