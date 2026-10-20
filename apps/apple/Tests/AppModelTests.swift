// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import Foundation
import Synchronization
import Testing
import WBMCCore
@testable import WBMC

final class FakeCore: WbmcCoreProtocol, Sendable {
    struct State {
        var accounts = [AccountInfo(id: "a", kind: "microsoft", email: "reader@wbmc.test", displayName: "Reader", status: "idle")]
        var folderRequests: [String] = []
        var messageRequests: [(String, UInt32, UInt32)] = []
        var rowCount = 250
        var unreadCount: UInt32 = 3
        var customFolders: [String: [FolderInfo]] = [:]
        var removedAccountIDs: [String] = []
        var unifiedRows: [MessageRow] = []
        var unifiedRequests: [(UInt32, UInt32)] = []
    }
    let state = Mutex(State())
    let contentProvider: @Sendable (String) async throws -> MessageContent

    init(contentProvider: @escaping @Sendable (String) async throws -> MessageContent = { _ in FakeCore.content }) {
        self.contentProvider = contentProvider
    }

    func accounts() -> [AccountInfo] { state.withLock { $0.accounts } }
    func folders(accountId: String) -> [FolderInfo] {
        let (custom, unread) = state.withLock { value in
            value.folderRequests.append(accountId)
            return (value.customFolders[accountId], value.unreadCount)
        }
        if let custom { return custom }
        var inbox = Self.folder("inbox", account: accountId)
        inbox.unreadCount = unread
        return [inbox, Self.folder("other", account: accountId)]
    }
    static func folder(_ id: String, account: String = "a", role: String? = nil) -> FolderInfo {
        FolderInfo(id: id, accountId: account, name: id, role: role ?? (id == "inbox" ? "inbox" : "other"),
                   parentId: nil, unreadCount: 3, totalCount: 250)
    }
    func messages(folderId: String, offset: UInt32, limit: UInt32) -> [MessageRow] {
        let count = state.withLock { value in
            value.messageRequests.append((folderId, offset, limit))
            return value.rowCount
        }
        return (Int(offset)..<max(Int(offset), min(count, Int(offset + limit)))).map {
            MessageRow(id: "m\($0)", accountId: "a", subject: "Subject \($0)", fromName: "Sender", fromEmail: "sender@wbmc.test",
                       dateMs: 0, snippet: "Preview", isRead: false, isFlagged: false, hasAttachments: false)
        }
    }
    func messageContent(messageId: String) async throws -> MessageContent {
        try await contentProvider(messageId)
    }
    static let content = MessageContent(subject: "Body", fromName: "Sender", fromEmail: "sender@wbmc.test",
                                       to: ["reader@wbmc.test"], cc: [], dateMs: 0, html: nil, text: "Hello", attachments: [])
    func addImapAccount(input: ImapAccountInput) async throws -> AccountInfo { accounts()[0] }
    func beginSignIn(provider: String, loginHint: String?) throws -> OAuthSignIn {
        throw FfiError.InvalidInput(message: "Use FakeSignInStarter")
    }
    func detectAccount(email: String) async throws -> AccountSetupInfo {
        throw FfiError.InvalidInput(message: "not used")
    }
    func unifiedMessages(offset: UInt32, limit: UInt32) -> [MessageRow] {
        state.withLock { value in
            value.unifiedRequests.append((offset, limit))
            let rows = value.unifiedRows
            let start = min(Int(offset), rows.count)
            let end = min(rows.count, start + Int(limit))
            return Array(rows[start..<end])
        }
    }
    func debugRefreshToken(accountId: String) -> String? { "fake-secret" }
    func removeAccount(accountId: String) async throws {
        state.withLock { value in
            value.removedAccountIDs.append(accountId)
            value.accounts.removeAll { $0.id == accountId }
            value.customFolders.removeValue(forKey: accountId)
        }
    }
    func syncAccount(accountId: String) async throws {}
    func syncAll() async {}
}

@MainActor
struct AppModelTests {
    @Test func selectFolderLoadsFirstPage() {
        let core = FakeCore()
        let model = AppModel(core: core)
        model.load()
        model.select(folder: "inbox")
        #expect(model.messages.count == 100)
        model.loadMore()
        #expect(model.messages.count == 200)
        model.loadMore()
        #expect(model.messages.count == 250)
        let before = core.state.withLock { $0.messageRequests.count }
        model.loadMore()
        #expect(core.state.withLock { $0.messageRequests.count } == before)
    }

    @Test func messagesChangedEventReloadsOnlyThatFolder() async {
        let core = FakeCore()
        let model = AppModel(core: core)
        model.load()
        model.select(folder: "inbox")
        model.loadMore()
        await model.select(message: "m4")
        let before = core.state.withLock { $0.messageRequests.count }
        model.handle(.messagesChanged(folderId: "other"))
        #expect(core.state.withLock { $0.messageRequests.count } == before)
        #expect(core.state.withLock { $0.folderRequests.last } == "a")
        core.state.withLock { $0.folderRequests = []; $0.unreadCount = 7 }
        model.handle(.messagesChanged(folderId: "inbox"))
        #expect(core.state.withLock { $0.folderRequests } == ["a"])
        #expect(model.folders["a"]?.first(where: { $0.id == "inbox" })?.unreadCount == 7)
        #expect(model.messages.count == 200)
        #expect(core.state.withLock { $0.messageRequests.last?.2 } == 200)
        #expect(model.selectedMessageID == "m4")
        #expect(model.content == FakeCore.content)
    }

    @Test func needsSignInStatusSurfaces() {
        let core = FakeCore()
        let model = AppModel(core: core)
        model.load()
        model.handle(.syncStatus(accountId: "a", status: .needsSignIn))
        #expect(model.accounts[0].status == "needs_sign_in")
        #expect(model.accountErrors["a"] == "Please sign in to this account again.")
        model.handle(.syncStatus(accountId: "a", status: .syncing(done: 1)))
        #expect(model.accounts[0].status == "syncing")
        model.handle(.syncStatus(accountId: "a", status: .error(message: "Invalid IMAP credentials")))
        #expect(model.accountErrors["a"] == "Invalid IMAP credentials")
        model.handle(.syncStatus(accountId: "a", status: .idle))
        #expect(model.accountErrors["a"] == nil)
        core.state.withLock { $0.accounts[0].kind = "imap" }
        model.load()
        model.handle(.syncStatus(accountId: "a", status: .needsSignIn))
        #expect(model.accountErrors["a"] == "Sign-in failed. Add the account again with the new password to update it.")
    }

    @Test func removeAccountClearsItsSelectionAndReloads() async {
        let core = FakeCore()
        core.state.withLock { value in
            value.accounts.append(AccountInfo(id: "b", kind: "imap", email: "other@wbmc.test", displayName: "Other", status: "idle"))
            value.customFolders = [
                "a": [FakeCore.folder("a-inbox", account: "a")],
                "b": [FakeCore.folder("b-inbox", account: "b")],
            ]
        }
        let model = AppModel(core: core)
        model.load()
        model.select(folder: "b-inbox")
        await model.select(message: "m0")

        await model.removeAccount(accountID: "a")
        #expect(model.selectedFolderID == "b-inbox")
        #expect(model.selectedMessageID == "m0")
        #expect(model.accounts.map(\.id) == ["b"])
        #expect(model.folders["a"] == nil)
        #expect(core.state.withLock { $0.removedAccountIDs } == ["a"])

        await model.removeAccount(accountID: "b")
        #expect(model.selectedFolderID == nil)
        #expect(model.selectedMessageID == nil)
        #expect(model.content == nil)
        #expect(model.accounts.isEmpty)
        #expect(core.state.withLock { $0.removedAccountIDs } == ["a", "b"])
    }

    @Test func onlyMicrosoftAccountsOfferSignInAgain() {
        var account = FakeCore().accounts()[0]
        account.status = "needs_sign_in"
        #expect(Sidebar.offersSignInAgain(for: account))
        account.kind = "imap"
        #expect(!Sidebar.offersSignInAgain(for: account))
        account.status = "error"
        #expect(!Sidebar.offersSignInAgain(for: account))
        account.kind = "microsoft"
        #expect(!Sidebar.offersSignInAgain(for: account))
    }

    @Test func accountAndFolderEventsReloadOnlyTheirScope() {
        let core = FakeCore()
        let model = AppModel(core: core)
        model.load()
        core.state.withLock { $0.folderRequests = [] }
        model.handle(.foldersChanged(accountId: "a"))
        #expect(core.state.withLock { $0.folderRequests } == ["a"])
        core.state.withLock {
            $0.accounts.append(AccountInfo(id: "b", kind: "imap", email: "other@wbmc.test", displayName: "Other", status: "idle"))
            $0.folderRequests = []
        }
        model.handle(.accountsChanged)
        #expect(model.accounts.count == 2)
        #expect(Set(core.state.withLock { $0.folderRequests }) == ["a", "b"])
    }

    @Test func staleMessageContentDoesNotReplaceNewFolderSelection() async {
        let gate = ContentGate()
        let core = FakeCore(contentProvider: { _ in await gate.wait() })
        let model = AppModel(core: core)
        model.select(folder: "inbox")
        let pending = Task { await model.select(message: "m0") }
        while !(await gate.isWaiting) { await Task.yield() }
        model.select(folder: "other")
        await gate.release()
        await pending.value
        #expect(model.selectedFolderID == "other")
        #expect(model.selectedMessageID == nil)
        #expect(model.content == nil)
        #expect(!model.isLoadingContent)
    }

    @Test func unifiedInboxLoadsAcrossAccounts() {
        let core = FakeCore()
        core.state.withLock { value in
            value.accounts.append(AccountInfo(id: "b", kind: "imap", email: "bob@wbmc.test", displayName: "Bob", status: "idle"))
            var alice = FakeCore.folder("a-inbox", account: "a", role: "inbox")
            alice.unreadCount = 2
            var bob = FakeCore.folder("b-inbox", account: "b", role: "inbox")
            bob.unreadCount = 4
            value.customFolders = ["a": [alice], "b": [bob]]
            value.unifiedRows = [Self.row("m-a", account: "a"), Self.row("m-b", account: "b", date: 1)]
        }
        let model = AppModel(core: core)
        model.load()
        #expect(model.showsUnifiedInbox)
        #expect(model.unifiedUnreadCount == 6)
        let folderMessages = core.state.withLock { $0.messageRequests.count }
        model.select(folder: AppModel.unifiedInboxID)
        #expect(model.selectedFolderID == AppModel.unifiedInboxID)
        #expect(model.messages.map(\.accountId) == ["a", "b"])
        let requests = core.state.withLock { $0.unifiedRequests }
        #expect(requests.count == 1)
        #expect(requests.first?.0 == 0)
        #expect(requests.first?.1 == 100)
        #expect(core.state.withLock { $0.messageRequests.count } == folderMessages)
        model.handle(.foldersChanged(accountId: "a"))
        #expect(model.selectedFolderID == AppModel.unifiedInboxID)
    }

    @Test func inboxChangeReloadsUnified() {
        let core = FakeCore()
        core.state.withLock { value in
            value.accounts.append(AccountInfo(id: "b", kind: "imap", email: "bob@wbmc.test", displayName: "Bob", status: "idle"))
            value.customFolders = [
                "a": [FakeCore.folder("a-inbox", account: "a", role: "inbox")],
                "b": [
                    FakeCore.folder("b-inbox", account: "b", role: "inbox"),
                    FakeCore.folder("b-sent", account: "b", role: "sent"),
                ],
            ]
            value.unifiedRows = [Self.row("m-a", account: "a")]
        }
        let model = AppModel(core: core)
        model.load()
        model.select(folder: AppModel.unifiedInboxID)
        let loaded = core.state.withLock { $0.unifiedRequests.count }
        model.handle(.messagesChanged(folderId: "b-sent"))
        #expect(core.state.withLock { $0.unifiedRequests.count } == loaded)
        core.state.withLock { $0.unifiedRows.append(Self.row("m-b", account: "b")) }
        model.handle(.messagesChanged(folderId: "a-inbox"))
        #expect(core.state.withLock { $0.unifiedRequests.count } == loaded + 1)
        #expect(model.messages.map(\.id) == ["m-a", "m-b"])
        let reloaded = core.state.withLock { $0.unifiedRequests.last }
        #expect(reloaded?.0 == 0)
        #expect(reloaded?.1 == 100)
    }

    @Test func unifiedHiddenWithOneAccount() {
        let core = FakeCore()
        let model = AppModel(core: core)
        model.load()
        #expect(model.accounts.count == 1)
        #expect(!model.showsUnifiedInbox)
    }

    @Test func googleNeedsSignInOffersSignInAgain() {
        var account = AccountInfo(id: "g", kind: "google", email: "reader@gmail.com", displayName: "Reader", status: "needs_sign_in")
        #expect(Sidebar.offersSignInAgain(for: account))
        #expect(Sidebar.signInAgain(for: account)?.provider == "google")
        #expect(Sidebar.signInAgain(for: account)?.loginHint == "reader@gmail.com")
        account.status = "idle"
        #expect(!Sidebar.offersSignInAgain(for: account))
        #expect(Sidebar.signInAgain(for: account) == nil)

        let core = FakeCore()
        core.state.withLock { $0.accounts[0].kind = "google" }
        let model = AppModel(core: core)
        model.load()
        model.handle(.syncStatus(accountId: "a", status: .needsSignIn))
        #expect(model.accountErrors["a"] == "Please sign in to this account again.")
    }

    @Test func cancelCancelsRustFlow() throws {
        let flow = RecordingOAuthFlow()
        let starter = RecordingSignInStarter(flow: flow)
        let session = AddAccountSession(core: FakeCore(), signInStarter: starter)
        let url = try session.startOAuth(provider: "google", loginHint: "reader@gmail.com")
        #expect(url.absoluteString == "https://accounts.example/oauth")
        let calls = starter.calls
        #expect(calls.count == 1)
        #expect(calls.first?.0 == "google")
        #expect(calls.first?.1 == "reader@gmail.com")
        session.cancel()
        #expect(flow.cancelled)
        session.cancel()
        #expect(flow.cancelCount == 1)
    }

    @Test func detectionResultDrivesState() {
        let microsoft = AccountSetupInfo(kind: "microsoft", providerName: "Microsoft", imap: nil, appPasswordUrl: nil)
        #expect(AddAccountState.from(microsoft, email: "ada@outlook.com") == .oauth(provider: "microsoft", providerName: "Microsoft"))

        let google = AccountSetupInfo(kind: "google", providerName: "Google", imap: nil, appPasswordUrl: nil)
        #expect(AddAccountState.from(google, email: "ada@gmail.com") == .oauth(provider: "google", providerName: "Google"))

        let settings = Self.imapSettings(username: "ada")
        let imap = AccountSetupInfo(kind: "imap", providerName: "GreenMail", imap: settings, appPasswordUrl: nil)
        #expect(AddAccountState.from(imap, email: "ada@wbmc.test") == .password(providerName: "GreenMail", settings: settings))

        let appPassword = AccountSetupInfo(kind: "imap", providerName: "iCloud", imap: settings, appPasswordUrl: "https://appleid.apple.com")
        #expect(AddAccountState.from(appPassword, email: "ada@icloud.com")
                == .appPassword(providerName: "iCloud", url: "https://appleid.apple.com", settings: settings))

        let unknown = AccountSetupInfo(kind: "unknown", providerName: "", imap: nil, appPasswordUrl: nil)
        #expect(AddAccountState.from(unknown, email: "ada@example.com") == .unknown(domain: "example.com"))
    }

    @Test func detectionFailureFallsBackToUnknown() {
        #expect(AddAccountState.detectionFailure(email: "ada@example.com") == .unknown(domain: "example.com"))
    }

    @Test func accountHueIsStable() {
        #expect(accountHueDegrees(accountId: "account-a") == 14)
        #expect(accountHueDegrees(accountId: "account-b") == 283)
        #expect(accountHueDegrees(accountId: "account-a") == accountHueDegrees(accountId: "account-a"))
    }

    @Test func selectMessageLoadsContent() async {
        let model = AppModel(core: FakeCore())
        model.select(folder: "inbox")
        await model.select(message: "m0")
        #expect(model.selectedMessageID == "m0")
        #expect(model.content?.text == "Hello")
        await model.select(message: nil)
        #expect(model.content == nil)
    }
}

private extension AppModelTests {
    static func row(_ id: String, account: String, date: Int64 = 0) -> MessageRow {
        MessageRow(id: id, accountId: account, subject: "Subject \(id)", fromName: "Sender", fromEmail: "sender@wbmc.test",
                   dateMs: date, snippet: "Preview", isRead: false, isFlagged: false, hasAttachments: false)
    }

    static func imapSettings(username: String) -> ImapSettings {
        ImapSettings(host: "localhost", port: 3143, security: "none", username: username,
                     smtpHost: "localhost", smtpPort: 3025, smtpSecurity: "none")
    }
}

final class RecordingOAuthFlow: OAuthSignInFlow, @unchecked Sendable {
    private let state = Mutex((cancelled: false, count: 0))

    func url() -> String { "https://accounts.example/oauth" }

    func finish() async throws -> AccountInfo {
        AccountInfo(id: "new", kind: "google", email: "reader@gmail.com", displayName: "Reader", status: "idle")
    }

    func cancel() {
        state.withLock { value in
            value.cancelled = true
            value.count += 1
        }
    }

    var cancelled: Bool { state.withLock { $0.cancelled } }
    var cancelCount: Int { state.withLock { $0.count } }
}

final class RecordingSignInStarter: SignInStarter, @unchecked Sendable {
    let flow: RecordingOAuthFlow
    private let recorded = Mutex([(String, String?)]())

    init(flow: RecordingOAuthFlow) { self.flow = flow }

    func start(provider: String, loginHint: String?) throws -> any OAuthSignInFlow {
        recorded.withLock { $0.append((provider, loginHint)) }
        return flow
    }

    var calls: [(String, String?)] { recorded.withLock { $0 } }
}

private actor ContentGate {
    private var continuation: CheckedContinuation<MessageContent, Never>?
    var isWaiting: Bool { continuation != nil }

    func wait() async -> MessageContent {
        await withCheckedContinuation { continuation = $0 }
    }

    func release() {
        continuation?.resume(returning: FakeCore.content)
        continuation = nil
    }
}
