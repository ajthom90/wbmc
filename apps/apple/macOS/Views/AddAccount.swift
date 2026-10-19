// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AppKit
import SwiftUI
import WBMCCore

struct AddAccount: View {
    let model: AppModel
    var startWithMicrosoft = false
    @Environment(\.dismiss) private var dismiss
    @State private var method = "microsoft"
    @State private var form = ImapForm()
    @State private var errorMessage: String?
    @State private var isWorking = false
    @State private var accountTask: Task<Void, Never>?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Add Account").font(.title2)
            if isWorking {
                ProgressView(method == "microsoft"
                             ? "Waiting for you to finish signing in in your browser…"
                             : "Connecting to your mail server…")
                    .frame(maxWidth: .infinity, minHeight: 100)
            } else {
                Picker("Account type", selection: $method) {
                    Text("Microsoft 365 / Outlook.com").tag("microsoft")
                    Text("Other (IMAP)").tag("imap")
                }
                .pickerStyle(.segmented)
                if method == "imap" { imapFields }
                else { Text("Continue in your browser to securely sign in to Microsoft.").foregroundStyle(.secondary) }
            }
            if let errorMessage { Text(errorMessage).foregroundStyle(.red).textSelection(.enabled) }
            HStack {
                Spacer()
                Button("Cancel") { accountTask?.cancel(); dismiss() }.keyboardShortcut(.cancelAction)
                if !isWorking {
                    Button(method == "microsoft" ? "Sign In" : "Add Account", action: connect)
                        .keyboardShortcut(.defaultAction)
                }
            }
        }
        .padding(24).frame(width: 540)
        .onDisappear { accountTask?.cancel() }
        .onChange(of: form.email) { old, email in
            if form.username.isEmpty || form.username == old { form.username = email }
            #if DEBUG
            if email.hasSuffix("@wbmc.test") {
                form.host = "localhost"; form.port = "3143"; form.security = "none"
                form.smtpHost = "localhost"; form.smtpPort = "3025"; form.smtpSecurity = "none"
            }
            #endif
        }
        .task {
            if startWithMicrosoft { connect() }
        }
    }

    private var imapFields: some View {
        Form {
            TextField("Email", text: $form.email)
            TextField("Display name", text: $form.displayName)
            Section("Incoming mail (IMAP)") {
                TextField("IMAP host", text: $form.host)
                TextField("IMAP port", text: $form.port)
                securityPicker("IMAP security", selection: $form.security)
            }
            Section("Outgoing mail (SMTP)") {
                TextField("SMTP host", text: $form.smtpHost)
                TextField("SMTP port", text: $form.smtpPort)
                securityPicker("SMTP security", selection: $form.smtpSecurity)
            }
            TextField("Username", text: $form.username)
            SecureField("Password", text: $form.password)
        }
        .formStyle(.grouped)
        .frame(height: 470)
    }

    private func securityPicker(_ title: String, selection: Binding<String>) -> some View {
        Picker(title, selection: selection) {
            Text("TLS").tag("tls")
            Text("STARTTLS").tag("starttls")
            Text("None").tag("none")
        }
    }

    private func connect() {
        errorMessage = nil
        isWorking = true
        accountTask = Task { @MainActor in
            defer { isWorking = false }
            do {
                if method == "microsoft" {
                    let flow = try model.signInStarter.start()
                    guard let url = URL(string: flow.url()), NSWorkspace.shared.open(url) else {
                        throw FfiError.InvalidInput(message: "Couldn’t open your browser.")
                    }
                    _ = try await flow.finish()
                } else {
                    _ = try await model.core.addImapAccount(input: form.input())
                    form.password = ""
                }
                try Task.checkCancellation()
                model.load()
                dismiss()
            } catch {
                if !Task.isCancelled { errorMessage = friendlyError(error) }
            }
        }
    }
}

private struct ImapForm {
    var email = ""
    var displayName = ""
    var host = ""
    var port = "993"
    var security = "tls"
    var smtpHost = ""
    var smtpPort = "465"
    var smtpSecurity = "tls"
    var username = ""
    var password = ""

    func input() throws -> ImapAccountInput {
        guard let port = UInt16(port), port > 0, let smtpPort = UInt16(smtpPort), smtpPort > 0 else {
            throw FfiError.InvalidInput(message: "Ports must be numbers between 1 and 65535.")
        }
        guard !email.isEmpty, !host.isEmpty, !smtpHost.isEmpty, !password.isEmpty else {
            throw FfiError.InvalidInput(message: "Enter your email, server addresses, and password.")
        }
        return ImapAccountInput(email: email, displayName: displayName, host: host, port: port,
                                security: security, username: username.isEmpty ? email : username,
                                password: password, smtpHost: smtpHost, smtpPort: smtpPort, smtpSecurity: smtpSecurity)
    }
}
