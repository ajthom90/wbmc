// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import Foundation
import WBMCCore

enum AddAccountState: Equatable, Sendable {
    case email
    case detecting
    case oauth(provider: String, providerName: String)
    case appPassword(providerName: String, url: String, settings: ImapSettings)
    case password(providerName: String, settings: ImapSettings)
    case unknown(domain: String)
    case manual(settings: ImapSettings)
    case signingIn
    case adding

    static func from(_ setup: AccountSetupInfo, email: String) -> AddAccountState {
        switch setup.kind {
        case "microsoft", "google":
            return .oauth(provider: setup.kind, providerName: setup.providerName)
        case "imap":
            guard let settings = setup.imap else { return .unknown(domain: domain(of: email)) }
            if let url = setup.appPasswordUrl, !url.isEmpty {
                return .appPassword(providerName: setup.providerName, url: url, settings: settings)
            }
            return .password(providerName: setup.providerName, settings: settings)
        default:
            return .unknown(domain: domain(of: email))
        }
    }

    private static func domain(of email: String) -> String {
        guard let at = email.lastIndex(of: "@") else { return email }
        let domain = email[email.index(after: at)...]
        return domain.isEmpty ? email : String(domain)
    }
}

/// Holds the browser sign-in the Add Account sheet started, so Cancel can stop the Rust flow.
@MainActor
final class AddAccountSession {
    let core: any CoreClient
    private let signInStarter: any SignInStarter
    private var flow: (any OAuthSignInFlow)?

    init(core: any CoreClient, signInStarter: any SignInStarter) {
        self.core = core
        self.signInStarter = signInStarter
    }

    func detect(email: String) async throws -> AccountSetupInfo {
        try await core.detectAccount(email: email)
    }

    @discardableResult
    func startOAuth(provider: String, loginHint: String?) throws -> URL {
        flow?.cancel()
        flow = nil
        let started = try signInStarter.start(provider: provider, loginHint: loginHint)
        guard let url = URL(string: started.url()), url.scheme == "https" || url.scheme == "http" else {
            started.cancel()
            throw FfiError.InvalidInput(message: "Couldn’t open your browser.")
        }
        flow = started
        return url
    }

    func finishOAuth() async throws -> AccountInfo {
        guard let flow else {
            throw FfiError.InvalidInput(message: "Sign-in was cancelled.")
        }
        defer {
            if self.flow === flow { self.flow = nil }
        }
        return try await flow.finish()
    }

    func cancel() {
        flow?.cancel()
        flow = nil
    }
}
