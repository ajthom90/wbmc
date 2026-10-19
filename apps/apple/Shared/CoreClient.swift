// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import Foundation
import WBMCCore

typealias CoreClient = WbmcCoreProtocol

protocol MicrosoftSignInFlow: AnyObject, Sendable {
    func url() -> String
    func finish() async throws -> AccountInfo
}

extension MicrosoftSignIn: MicrosoftSignInFlow {}

protocol SignInStarter: Sendable {
    func start() throws -> any MicrosoftSignInFlow
}

struct CoreSignInStarter: SignInStarter {
    let core: any CoreClient
    func start() throws -> any MicrosoftSignInFlow { try core.beginMicrosoftSignIn() }
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
