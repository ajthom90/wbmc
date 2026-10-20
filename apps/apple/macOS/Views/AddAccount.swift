// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AppKit
import SwiftUI
import WBMCCore

struct AddAccount: View {
    let model: AppModel
    var resume: AccountInfo?
    @Environment(\.dismiss) private var dismiss
    @State private var session: AddAccountSession
    @State private var state: AddAccountState = .email
    @State private var email = ""
    @State private var password = ""
    @State private var displayName = ""
    @State private var host = ""
    @State private var port = "993"
    @State private var security = "tls"
    @State private var smtpHost = ""
    @State private var smtpPort = "465"
    @State private var smtpSecurity = "tls"
    @State private var username = ""
    @State private var detectedUsername: String?
    @State private var showServerSettings = false
    @State private var errorMessage: String?
    @State private var accountTask: Task<Void, Never>?
    @State private var didResume = false

    @MainActor
    init(model: AppModel, resume: AccountInfo? = nil) {
        self.model = model
        self.resume = resume
        _session = State(initialValue: AddAccountSession(core: model.core, signInStarter: model.signInStarter))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Add Account").font(.title2)
            fields
            if let errorMessage {
                Text(errorMessage).foregroundStyle(.red).textSelection(.enabled)
            }
            buttons
        }
        .padding(24)
        .frame(width: 540)
        .onDisappear {
            accountTask?.cancel()
            session.cancel()
        }
        .task { resumeIfNeeded() }
    }

    @ViewBuilder private var fields: some View {
        switch state {
        case .email:
            Form { TextField("Email", text: $email) }
                .formStyle(.grouped)
        case .detecting:
            ProgressView("Looking up your account…")
                .frame(maxWidth: .infinity, minHeight: 80)
        case .oauth(_, let providerName):
            Text("Continue in your browser to sign in to \(providerName).")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .appPassword(let providerName, let url, _):
            imapPrompt(appPasswordName: providerName, appPasswordURL: url)
        case .password:
            imapPrompt(appPasswordName: nil, appPasswordURL: nil)
        case .unknown(let domain):
            Text("We couldn't find settings for \(domain).")
                .frame(maxWidth: .infinity, alignment: .leading)
        case .manual:
            manualForm
        case .signingIn:
            ProgressView("Waiting for you to finish signing in in your browser…")
                .frame(maxWidth: .infinity, minHeight: 80)
        case .adding:
            ProgressView("Connecting to your mail server…")
                .frame(maxWidth: .infinity, minHeight: 80)
        }
    }

    private func imapPrompt(appPasswordName: String?, appPasswordURL: String?) -> some View {
        Form {
            if let appPasswordName {
                Text("\(appPasswordName) needs an app password, not your regular password.")
                if let appPasswordURL, let destination = URL(string: appPasswordURL) {
                    Link("Create an app password", destination: destination)
                }
            }
            Text(email).foregroundStyle(.secondary)
            SecureField("Password", text: $password)
            DisclosureGroup("Show server settings", isExpanded: $showServerSettings) {
                serverFields
            }
        }
        .formStyle(.grouped)
    }

    private var manualForm: some View {
        Form {
            TextField("Email", text: $email)
            TextField("Display name", text: $displayName)
            Section("Incoming mail (IMAP)") {
                TextField("IMAP host", text: $host)
                TextField("IMAP port", text: $port)
                securityPicker("IMAP security", selection: $security)
            }
            Section("Outgoing mail (SMTP)") {
                TextField("SMTP host", text: $smtpHost)
                TextField("SMTP port", text: $smtpPort)
                securityPicker("SMTP security", selection: $smtpSecurity)
            }
            TextField("Username", text: $username)
            SecureField("Password", text: $password)
        }
        .formStyle(.grouped)
        .frame(height: 470)
        .onChange(of: email) { old, newValue in
            if username.isEmpty || username == old { username = newValue }
        }
    }

    @ViewBuilder private var serverFields: some View {
        TextField("Email", text: $email)
        TextField("Display name", text: $displayName)
        TextField("IMAP host", text: $host)
        TextField("IMAP port", text: $port)
        securityPicker("IMAP security", selection: $security)
        TextField("SMTP host", text: $smtpHost)
        TextField("SMTP port", text: $smtpPort)
        securityPicker("SMTP security", selection: $smtpSecurity)
        TextField("Username", text: $username)
    }

    private func securityPicker(_ title: String, selection: Binding<String>) -> some View {
        Picker(title, selection: selection) {
            Text("TLS").tag("tls")
            Text("STARTTLS").tag("starttls")
            Text("None").tag("none")
        }
    }

    private var buttons: some View {
        HStack {
            Spacer()
            Button("Cancel", action: cancel).keyboardShortcut(.cancelAction)
            switch state {
            case .email:
                Button("Continue", action: continueFromEmail).keyboardShortcut(.defaultAction)
            case .oauth(let provider, _):
                Button("Set Up Manually") {
                    errorMessage = nil
                    state.setUpManually(email: email)
                }
                Button("Sign In") { signIn(provider: provider) }
                    .keyboardShortcut(.defaultAction)
            case .appPassword, .password, .manual:
                Button("Add Account", action: addAccount).keyboardShortcut(.defaultAction)
            case .unknown:
                Button("Microsoft 365") { signIn(provider: "microsoft") }
                Button("Google") { signIn(provider: "google") }
                Button("Other (IMAP)", action: showManual)
            case .detecting, .signingIn, .adding:
                EmptyView()
            }
        }
    }

    private func continueFromEmail() {
        accountTask?.cancel()
        accountTask = Task { await lookup() }
    }

    private func lookup() async {
        let address = email.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let at = address.lastIndex(of: "@"), at != address.startIndex, address.index(after: at) != address.endIndex else {
            errorMessage = "Enter an email address."
            return
        }
        email = address
        errorMessage = nil
        state = .detecting
        do {
            let setup = try await session.detect(email: address)
            try Task.checkCancellation()
            if let settings = setup.imap { apply(settings) }
            state = AddAccountState.from(setup, email: address)
        } catch {
            if !Task.isCancelled {
                errorMessage = friendlyError(error)
                state = AddAccountState.detectionFailure(email: address)
            }
        }
    }

    private func apply(_ settings: ImapSettings) {
        host = settings.host
        port = String(settings.port)
        security = settings.security
        username = settings.username
        detectedUsername = settings.username
        smtpHost = settings.smtpHost
        smtpPort = String(settings.smtpPort)
        smtpSecurity = settings.smtpSecurity
        showServerSettings = false
    }

    private func showManual() {
        detectedUsername = nil
        if username.isEmpty { username = email }
        if port.isEmpty { port = "993" }
        if smtpPort.isEmpty { smtpPort = "465" }
        if security.isEmpty { security = "tls" }
        if smtpSecurity.isEmpty { smtpSecurity = "tls" }
        state = .manual(settings: ImapSettings(
            host: host, port: UInt16(port) ?? 993, security: security, username: username,
            smtpHost: smtpHost, smtpPort: UInt16(smtpPort) ?? 465, smtpSecurity: smtpSecurity))
    }

    private func signIn(provider: String) {
        errorMessage = nil
        let previous = state.beginSignIn()
        accountTask?.cancel()
        accountTask = Task { @MainActor in
            do {
                let hint = email.trimmingCharacters(in: .whitespacesAndNewlines)
                let url = try session.startOAuth(provider: provider, loginHint: hint.isEmpty ? nil : hint)
                guard NSWorkspace.shared.open(url) else {
                    throw FfiError.InvalidInput(message: "Couldn’t open your browser.")
                }
                _ = try await session.finishOAuth()
                try Task.checkCancellation()
                model.load()
                dismiss()
            } catch {
                if !Task.isCancelled {
                    errorMessage = friendlyError(error)
                    state.signInFailed(returningTo: previous)
                }
            }
        }
    }

    private func addAccount() {
        let previous = state
        errorMessage = nil
        state = .adding
        accountTask?.cancel()
        accountTask = Task { @MainActor in
            do {
                _ = try await session.core.addImapAccount(input: try imapInput())
                password = ""
                try Task.checkCancellation()
                model.load()
                dismiss()
            } catch {
                if !Task.isCancelled {
                    errorMessage = friendlyError(error)
                    state = previous
                }
            }
        }
    }

    private func imapInput() throws -> ImapAccountInput {
        guard let port = UInt16(port), port > 0, let smtpPort = UInt16(smtpPort), smtpPort > 0 else {
            throw FfiError.InvalidInput(message: "Ports must be numbers between 1 and 65535.")
        }
        let address = email.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !address.isEmpty, !host.isEmpty, !smtpHost.isEmpty, !password.isEmpty else {
            throw FfiError.InvalidInput(message: "Enter your email, server addresses, and password.")
        }
        return ImapAccountInput(
            email: address, displayName: displayName, host: host, port: port, security: security,
            username: usernameForSubmit(), password: password, smtpHost: smtpHost, smtpPort: smtpPort,
            smtpSecurity: smtpSecurity)
    }

    private func usernameForSubmit() -> String {
        let typed = username.trimmingCharacters(in: .whitespacesAndNewlines)
        if let detectedUsername {
            if typed.isEmpty || typed == detectedUsername { return "" }
            return typed
        }
        return typed.isEmpty ? email.trimmingCharacters(in: .whitespacesAndNewlines) : typed
    }

    private func resumeIfNeeded() {
        guard !didResume else { return }
        didResume = true
        guard let resume, let request = Sidebar.signInAgain(for: resume) else { return }
        email = request.loginHint
        state = .oauth(provider: request.provider, providerName: request.provider == "google" ? "Google" : "Microsoft")
        signIn(provider: request.provider)
    }

    private func cancel() {
        accountTask?.cancel()
        session.cancel()
        dismiss()
    }
}
