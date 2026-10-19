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
    }
    let state = Mutex(State())
    let contentProvider: @Sendable (String) async throws -> MessageContent

    init(contentProvider: @escaping @Sendable (String) async throws -> MessageContent = { _ in FakeCore.content }) {
        self.contentProvider = contentProvider
    }

    func accounts() -> [AccountInfo] { state.withLock { $0.accounts } }
    func folders(accountId: String) -> [FolderInfo] {
        let unread = state.withLock { value in
            value.folderRequests.append(accountId)
            return value.unreadCount
        }
        var inbox = Self.folder("inbox", account: accountId)
        inbox.unreadCount = unread
        return [inbox, Self.folder("other", account: accountId)]
    }
    static func folder(_ id: String, account: String = "a") -> FolderInfo {
        FolderInfo(id: id, accountId: account, name: id, role: id == "inbox" ? "inbox" : "other",
                   parentId: nil, unreadCount: 3, totalCount: 250)
    }
    func messages(folderId: String, offset: UInt32, limit: UInt32) -> [MessageRow] {
        let count = state.withLock { value in
            value.messageRequests.append((folderId, offset, limit))
            return value.rowCount
        }
        return (Int(offset)..<max(Int(offset), min(count, Int(offset + limit)))).map {
            MessageRow(id: "m\($0)", subject: "Subject \($0)", fromName: "Sender", fromEmail: "sender@wbmc.test",
                       dateMs: 0, snippet: "Preview", isRead: false, isFlagged: false, hasAttachments: false)
        }
    }
    func messageContent(messageId: String) async throws -> MessageContent {
        try await contentProvider(messageId)
    }
    static let content = MessageContent(subject: "Body", fromName: "Sender", fromEmail: "sender@wbmc.test",
                                       to: ["reader@wbmc.test"], cc: [], dateMs: 0, html: nil, text: "Hello", attachments: [])
    func addImapAccount(input: ImapAccountInput) async throws -> AccountInfo { accounts()[0] }
    func beginMicrosoftSignIn() throws -> MicrosoftSignIn { throw FfiError.InvalidInput(message: "Use FakeSignInStarter") }
    func debugRefreshToken(accountId: String) -> String? { "fake-secret" }
    func removeAccount(accountId: String) throws {}
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
        model.handle(.syncStatus(accountId: "a", status: .syncing(done: 1)))
        #expect(model.accounts[0].status == "syncing")
        model.handle(.syncStatus(accountId: "a", status: .error(message: "Invalid IMAP credentials")))
        #expect(model.accountErrors["a"] == "Invalid IMAP credentials")
        model.handle(.syncStatus(accountId: "a", status: .idle))
        #expect(model.accountErrors["a"] == nil)
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
