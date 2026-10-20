// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import Foundation
import Observation
import WBMCCore

@Observable @MainActor
final class AppModel {
    private(set) var accounts: [AccountInfo] = []
    private(set) var folders: [String: [FolderInfo]] = [:]
    private(set) var selectedFolderID: String?
    private(set) var messages: [MessageRow] = []
    private(set) var selectedMessageID: String?
    private(set) var content: MessageContent?
    private(set) var isLoadingContent = false
    private(set) var canLoadMore = false
    var errorMessage: String?
    private(set) var accountErrors: [String: String] = [:]

    static let unifiedInboxID = "unified:inbox"

    let core: any CoreClient
    let signInStarter: any SignInStarter
    private var loadedLimit: UInt32 = 100
    private var contentRequest = UUID()

    var showsUnifiedInbox: Bool { accounts.count >= 2 }

    var unifiedUnreadCount: Int {
        folders.values.joined().reduce(0) { total, folder in
            folder.role == "inbox" ? total + Int(folder.unreadCount) : total
        }
    }

    init(core: any CoreClient, signInStarter: (any SignInStarter)? = nil) {
        self.core = core
        self.signInStarter = signInStarter ?? CoreSignInStarter(core: core)
    }

    func load() {
        accounts = core.accounts()
        accountErrors = accountErrors.filter { id, _ in accounts.contains(where: { $0.id == id }) }
        folders = Dictionary(uniqueKeysWithValues: accounts.map { ($0.id, core.folders(accountId: $0.id)) })
        validateFolderSelection()
    }

    func select(folder: String?) {
        guard selectedFolderID != folder else { return }
        selectedFolderID = folder
        clearMessage()
        loadedLimit = 100
        messages = page(offset: 0, limit: 100)
        canLoadMore = messages.count == 100
    }

    func loadMore() {
        guard selectedFolderID != nil, canLoadMore else { return }
        let next = page(offset: UInt32(messages.count), limit: 100)
        messages.append(contentsOf: next)
        loadedLimit = UInt32(messages.count)
        canLoadMore = next.count == 100
    }

    func select(message: String?) async {
        clearMessage()
        selectedMessageID = message
        guard let message else { return }
        let request = contentRequest
        isLoadingContent = true
        do {
            let value = try await core.messageContent(messageId: message)
            guard request == contentRequest else { return }
            guard !Task.isCancelled else { isLoadingContent = false; return }
            content = value
            isLoadingContent = false
        } catch {
            guard request == contentRequest else { return }
            isLoadingContent = false
            if !Task.isCancelled { errorMessage = friendlyError(error) }
        }
    }

    func refresh() async { await core.syncAll() }

    /// Deletes the account locally. Mail on the server is left alone.
    /// Clears the open folder when it belonged to this account, then reloads.
    func removeAccount(accountID: String) async {
        let selectedBelongs = folders[accountID]?.contains { $0.id == selectedFolderID } ?? false
        do {
            try await core.removeAccount(accountId: accountID)
        } catch {
            errorMessage = friendlyError(error)
            return
        }
        if selectedBelongs {
            select(folder: nil)
        }
        load()
    }

    func handle(_ event: FfiEvent) {
        switch event {
        case .accountsChanged:
            load()
        case .foldersChanged(let accountID):
            folders[accountID] = core.folders(accountId: accountID)
            validateFolderSelection()
        case .messagesChanged(let folderID):
            let reloadsUnified = selectedFolderID == Self.unifiedInboxID && isInbox(folderID)
            if folderID == selectedFolderID || reloadsUnified {
                let count = max(100, loadedLimit)
                messages = page(offset: 0, limit: count)
                canLoadMore = messages.count == Int(count)
                if let id = selectedMessageID, !messages.contains(where: { $0.id == id }) { clearMessage() }
            }
            if let accountID = folders.first(where: { $0.value.contains(where: { $0.id == folderID }) })?.key {
                folders[accountID] = core.folders(accountId: accountID)
            }
        case .syncStatus(let accountID, let status):
            guard let index = accounts.firstIndex(where: { $0.id == accountID }) else { return }
            switch status {
            case .idle:
                accounts[index].status = "idle"
                accountErrors[accountID] = nil
            case .syncing:
                accounts[index].status = "syncing"
                accountErrors[accountID] = nil
            case .error(let message):
                accounts[index].status = "error"
                accountErrors[accountID] = message
            case .needsSignIn:
                accounts[index].status = "needs_sign_in"
                accountErrors[accountID] = needsSignInMessage(kind: accounts[index].kind)
            }
        }
    }

    private func clearMessage() {
        contentRequest = UUID()
        selectedMessageID = nil
        content = nil
        isLoadingContent = false
        errorMessage = nil
    }

    private func page(offset: UInt32, limit: UInt32) -> [MessageRow] {
        guard let folder = selectedFolderID else { return [] }
        if folder == Self.unifiedInboxID {
            return core.unifiedMessages(offset: offset, limit: limit)
        }
        return core.messages(folderId: folder, offset: offset, limit: limit)
    }

    private func isInbox(_ folderID: String) -> Bool {
        folders.values.joined().contains { $0.id == folderID && $0.role == "inbox" }
    }

    private func validateFolderSelection() {
        guard let id = selectedFolderID else { return }
        if id == Self.unifiedInboxID {
            if !showsUnifiedInbox { select(folder: nil) }
            return
        }
        if !folders.values.joined().contains(where: { $0.id == id }) {
            select(folder: nil)
        }
    }
}

/// The mutable weak reference belongs to the main actor; callback entry is thread safe.
final class ModelListener: CoreListener {
    @MainActor weak var model: AppModel?

    func onEvent(event: FfiEvent) {
        Task { @MainActor [weak self] in self?.model?.handle(event) }
    }
}
