// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import SwiftUI
import WBMCCore

struct FolderNode: Identifiable {
    let folder: FolderInfo
    let children: [FolderNode]?
    var id: String { folder.id }

    static func tree(_ folders: [FolderInfo]) -> [FolderNode] {
        let roles = ["inbox", "drafts", "sent", "archive", "junk", "trash"]
        func nodes(parent: String?, ancestors: Set<String>) -> [FolderNode] {
            folders.filter { folder in
                let parentID = folders.contains(where: { $0.id == folder.parentId }) ? folder.parentId : nil
                return parentID == parent && !ancestors.contains(folder.id)
            }.sorted { lhs, rhs in
                let l = roles.firstIndex(of: lhs.role) ?? roles.count
                let r = roles.firstIndex(of: rhs.role) ?? roles.count
                return l == r ? lhs.name.localizedStandardCompare(rhs.name) == .orderedAscending : l < r
            }.map { folder in
                let children = nodes(parent: folder.id, ancestors: ancestors.union([folder.id]))
                return FolderNode(folder: folder, children: children.isEmpty ? nil : children)
            }
        }
        return nodes(parent: nil, ancestors: [])
    }
}

struct Sidebar: View {
    let model: AppModel
    let signInAgain: () -> Void

    var body: some View {
        List(selection: Binding(get: { model.selectedFolderID }, set: { model.select(folder: $0) })) {
            ForEach(model.accounts, id: \.id) { account in
                Section {
                    OutlineGroup(FolderNode.tree(model.folders[account.id] ?? []), children: \.children) { node in
                        Label(node.folder.name, systemImage: symbol(for: node.folder.role))
                            .badge(Int(node.folder.unreadCount))
                            .tag(node.id)
                    }
                } header: {
                    HStack {
                        Text(account.displayName.isEmpty ? account.email : account.displayName)
                            .help(account.email)
                        Spacer()
                        switch account.status {
                        case "syncing": ProgressView().controlSize(.small).accessibilityLabel("Syncing")
                        case "error": Image(systemName: "exclamationmark.triangle").help("Sync failed. Refresh to try again.")
                        case "needs_sign_in": Button("Sign in again", action: signInAgain).font(.caption)
                        default: EmptyView()
                        }
                    }
                }
            }
        }
        .listStyle(.sidebar)
        .overlay {
            if model.accounts.isEmpty {
                ContentUnavailableView("No Accounts", systemImage: "tray", description: Text("Add an account to read your mail."))
            }
        }
        .navigationTitle("Mail")
    }

    private func symbol(for role: String) -> String {
        switch role {
        case "inbox": "tray"
        case "drafts": "doc"
        case "sent": "paperplane"
        case "archive": "archivebox"
        case "junk": "xmark.bin"
        case "trash": "trash"
        default: "folder"
        }
    }
}
