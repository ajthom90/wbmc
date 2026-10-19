// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import SwiftUI
import WBMCCore

struct MessageView: View {
    let model: AppModel

    var body: some View {
        Group {
            if let content = model.content {
                VStack(alignment: .leading, spacing: 0) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(content.subject.isEmpty ? "(No subject)" : content.subject).font(.title).textSelection(.enabled)
                        Text("\(content.fromName) <\(content.fromEmail)>").textSelection(.enabled)
                        Text("To: " + content.to.joined(separator: ", ")).foregroundStyle(.secondary).textSelection(.enabled)
                        if !content.cc.isEmpty {
                            Text("Cc: " + content.cc.joined(separator: ", ")).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Text(Date(timeIntervalSince1970: Double(content.dateMs) / 1000), format: .dateTime)
                            .font(.caption).foregroundStyle(.secondary)
                        if !content.attachments.isEmpty {
                            ScrollView(.horizontal) {
                                HStack {
                                    ForEach(Array(content.attachments.enumerated()), id: \.offset) { _, attachment in
                                        Label("\(attachment.filename) · \(ByteCountFormatter.string(fromByteCount: Int64(clamping: attachment.size), countStyle: .file))",
                                              systemImage: "paperclip")
                                            .font(.caption).padding(8).background(.quaternary, in: Capsule())
                                    }
                                }
                            }
                        }
                    }
                    .padding(20)
                    Divider()
                    if let html = content.html {
                        SafeHTMLView(html: html)
                    } else {
                        ScrollView {
                            Text(content.text ?? "").textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading).padding(20)
                        }
                    }
                }
            } else if model.isLoadingContent {
                ProgressView("Loading message…").frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let error = model.errorMessage {
                ContentUnavailableView("Couldn’t Load Message", systemImage: "exclamationmark.triangle", description: Text(error))
            } else {
                ContentUnavailableView("Select a Message", systemImage: "envelope", description: Text("Choose a message to read it."))
            }
        }
    }
}
