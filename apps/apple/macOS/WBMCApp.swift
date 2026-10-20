// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import SwiftUI
import WBMCCore

@main @MainActor
struct WBMCApp: App {
    private let model: AppModel?
    private let initializationError: String?
    private let listener: ModelListener

    init() {
        let listener = ModelListener()
        self.listener = listener
        do {
            var directory = FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("Library/Application Support/io.github.ajthom90.wbmc", isDirectory: true)
            #if DEBUG
            if let override = ProcessInfo.processInfo.environment["WBMC_DATA_DIR"], !override.isEmpty {
                directory = URL(fileURLWithPath: override, isDirectory: true)
            }
            #endif
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let core = try WbmcCore(dataDir: directory.path, listener: listener)
            let model = AppModel(core: core)
            listener.model = model
            model.load()
            self.model = model
            initializationError = nil
            Task { await model.refresh() }
        } catch {
            model = nil
            initializationError = friendlyError(error)
        }
    }

    var body: some Scene {
        WindowGroup {
            if let model {
                ReaderWindow(model: model)
            } else {
                ContentUnavailableView("Couldn’t Open WBMC", systemImage: "exclamationmark.triangle",
                                       description: Text(initializationError ?? "Unknown error"))
                    .textSelection(.enabled).frame(minWidth: 900, minHeight: 560)
            }
        }
        .defaultSize(width: 1150, height: 750)
        #if DEBUG
        .commands {
            CommandMenu("Debug") {
                Button("Copy refresh token…") {
                    NotificationCenter.default.post(name: .copyRefreshToken, object: nil)
                }
                .disabled(model?.accounts.contains(where: { $0.kind == "microsoft" || $0.kind == "google" }) != true)
            }
        }
        #endif
    }
}

private struct ReaderWindow: View {
    let model: AppModel
    @State private var columnVisibility: NavigationSplitViewVisibility = .all
    @State private var showingAddAccount = false
    @State private var resumeAccount: AccountInfo?
    #if DEBUG
    @State private var showingTokens = false
    #endif

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            Sidebar(model: model) { account in
                resumeAccount = account
                showingAddAccount = true
            }
                .navigationSplitViewColumnWidth(min: 180, ideal: 220)
        } content: {
            MessageList(model: model).navigationSplitViewColumnWidth(min: 260, ideal: 330)
        } detail: {
            MessageView(model: model)
        }
        .frame(minWidth: 900, minHeight: 560)
        .toolbar {
            ToolbarItemGroup {
                Button { Task { await model.refresh() } } label: { Label("Refresh", systemImage: "arrow.clockwise") }
                    .help("Refresh")
                Button {
                    resumeAccount = nil
                    showingAddAccount = true
                } label: { Label("Add Account", systemImage: "plus") }
                    .help("Add Account")
            }
        }
        .sheet(isPresented: $showingAddAccount, onDismiss: { resumeAccount = nil }) {
            AddAccount(model: model, resume: resumeAccount)
        }
        #if DEBUG
        .onReceive(NotificationCenter.default.publisher(for: .copyRefreshToken)) { _ in showingTokens = true }
        .sheet(isPresented: $showingTokens) { RefreshTokenSheet(model: model) }
        #endif
    }
}

#if DEBUG
private extension Notification.Name {
    static let copyRefreshToken = Notification.Name("WBMCCopyRefreshToken")
}

private struct RefreshTokenSheet: View {
    let model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var accountID = ""
    @State private var confirming = false
    @State private var errorMessage: String?

    private var oauthAccounts: [AccountInfo] { model.accounts.filter { $0.kind == "microsoft" || $0.kind == "google" } }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Copy refresh token…").font(.title2)
            Picker("Account", selection: $accountID) {
                ForEach(oauthAccounts, id: \.id) { Text($0.email).tag($0.id) }
            }
            if let errorMessage { Text(errorMessage).foregroundStyle(.red) }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Copy…") { confirming = true }.disabled(accountID.isEmpty)
            }
        }
        .padding(24).frame(width: 440)
        .onAppear { accountID = oauthAccounts.first?.id ?? "" }
        .alert("Copy a secret to the clipboard?", isPresented: $confirming) {
            Button("Cancel", role: .cancel) {}
            Button("Copy Token") { copyToken() }
        } message: {
            Text("A refresh token is a secret that grants access to your mailbox. Anyone who can read your clipboard may be able to use it. Never share or commit it.")
        }
    }

    private func copyToken() {
        guard let token = model.core.debugRefreshToken(accountId: accountID) else {
            errorMessage = "No refresh token is available. Sign in to this account again."
            return
        }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(token, forType: .string)
        dismiss()
    }
}
#endif
