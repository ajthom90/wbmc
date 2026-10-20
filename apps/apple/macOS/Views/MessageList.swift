// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import SwiftUI
import WBMCCore

struct MessageList: View {
    let model: AppModel

    var body: some View {
        List(selection: Binding(get: { model.selectedMessageID }, set: { id in
            Task { await model.select(message: id) }
        })) {
            ForEach(model.messages, id: \.id) { message in
                MessageListRow(message: message, accountEmail: accountEmail(for: message))
                    .tag(message.id)
                    .onAppear {
                        if message.id == model.messages.last?.id { model.loadMore() }
                    }
            }
        }
        .overlay {
            if model.messages.isEmpty {
                ContentUnavailableView(model.selectedFolderID == nil ? "Select a Folder" : "No Messages",
                                       systemImage: "tray", description: Text("Your synced mail appears here."))
            }
        }
        .navigationTitle(navigationTitle)
    }

    private var navigationTitle: String {
        if model.selectedFolderID == AppModel.unifiedInboxID { return "All Inboxes" }
        return model.folders.values.joined().first { $0.id == model.selectedFolderID }?.name ?? "Messages"
    }

    private func accountEmail(for message: MessageRow) -> String? {
        guard model.selectedFolderID == AppModel.unifiedInboxID else { return nil }
        return model.accounts.first { $0.id == message.accountId }?.email ?? message.accountId
    }
}

func accountHueDegrees(accountId: String) -> Int {
    var hash: UInt64 = 14_695_981_039_346_656_037
    for byte in accountId.utf8 {
        hash ^= UInt64(byte)
        hash &*= 1_099_511_628_211
    }
    return Int(hash % 360)
}

private struct MessageListRow: View {
    let message: MessageRow
    var accountEmail: String?

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Circle().fill(message.isRead ? Color.clear : Color.accentColor)
                .frame(width: 6, height: 6).padding(.top, 6)
                .accessibilityLabel(message.isRead ? "Read" : "Unread")
            if let accountEmail {
                Circle()
                    .fill(Color(hue: Double(accountHueDegrees(accountId: message.accountId)) / 360,
                                saturation: 0.65, brightness: 0.85))
                    .frame(width: 8, height: 8)
                    .padding(.top, 5)
                    .help(accountEmail)
                    .accessibilityLabel(accountEmail)
            }
            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline) {
                    Text(message.fromName.isEmpty ? message.fromEmail : message.fromName)
                        .fontWeight(message.isRead ? .regular : .bold).lineLimit(1)
                    Spacer(minLength: 4)
                    Text(Date(timeIntervalSince1970: Double(message.dateMs) / 1000), format: .relative(presentation: .named))
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                HStack {
                    Text(message.subject.isEmpty ? "(No subject)" : message.subject).lineLimit(1)
                    Spacer(minLength: 0)
                    if message.isFlagged { Image(systemName: "flag.fill").accessibilityLabel("Flagged") }
                    if message.hasAttachments { Image(systemName: "paperclip").accessibilityLabel("Has attachments") }
                }
                Text(message.snippet).font(.callout).foregroundStyle(.secondary).lineLimit(2)
            }
        }
        .padding(.vertical, 5)
    }
}
