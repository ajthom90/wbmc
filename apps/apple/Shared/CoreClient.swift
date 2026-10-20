// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import Foundation
import WBMCCore

typealias CoreClient = WbmcCoreProtocol

protocol OAuthSignInFlow: AnyObject, Sendable {
    func url() -> String
    func finish() async throws -> AccountInfo
    func cancel()
}

extension OAuthSignIn: OAuthSignInFlow {}

protocol SignInStarter: Sendable {
    func start(provider: String, loginHint: String?) throws -> any OAuthSignInFlow
}

struct CoreSignInStarter: SignInStarter {
    let core: any CoreClient
    func start(provider: String, loginHint: String?) throws -> any OAuthSignInFlow {
        try core.beginSignIn(provider: provider, loginHint: loginHint)
    }
}

func needsSignInMessage(kind: String) -> String {
    if kind == "microsoft" || kind == "google" {
        return "Please sign in to this account again."
    }
    return "Sign-in failed. Add the account again with the new password to update it."
}

func friendlyError(_ error: any Error) -> String {
    guard let error = error as? FfiError else { return error.localizedDescription }
    switch error {
    case .Network(let message): return "Couldn’t connect. \(message)"
    case .Protocol(let message): return "The mail server reported a problem. \(message)"
    case .Storage(let message): return "Couldn’t access the mail store. \(message)"
    case .NeedsSignIn: return "Please sign in to this account again."
    case .NotFound(let message): return "This item is no longer available. \(message)"
    case .InvalidInput(let message): return "Check your account settings. \(message)"
    }
}
