import AppKit
import SwiftUI

/// Settings › Accounts: the connected account, signing out or in again,
/// and which Google sign-in client to use (spec §7.3).
struct AccountSettings: View {
    @Environment(AppModel.self) private var model
    @State private var confirmingSignOut = false

    var body: some View {
        Form {
            Section("Gmail") {
                switch model.accountState {
                case .open(let id) where id == AppModel.demoAccountID:
                    LabeledContent("Account") { Text("Demo mailbox (nothing leaves this Mac)") }
                    Button("Connect Gmail Instead…") { Task { await model.signIn(with: .effective()) } }
                        .disabled(!GoogleClientConfiguration.effective().isUsable)
                case .open:
                    LabeledContent("Account") { Text(model.accountEmail ?? "Connected") }
                    if model.needsReauthentication {
                        Label("Google asked you to sign in again.", systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                    }
                    HStack {
                        Button("Sign In Again…") { Task { await model.signIn(with: .effective()) } }
                        Button("Sign Out…", role: .destructive) { confirmingSignOut = true }
                    }
                case .signingIn:
                    HStack {
                        ProgressView().controlSize(.small)
                        Text("Finish signing in with Google in your browser…")
                    }
                default:
                    Text("No account is connected.").foregroundStyle(.secondary)
                    Button("Connect Gmail…") { Task { await model.signIn(with: .effective()) } }
                        .disabled(!GoogleClientConfiguration.effective().isUsable)
                }
            }
            Section {
                GoogleClientFields()
            } header: {
                Text("Google sign-in client")
            } footer: {
                Text("Your own client avoids Google's unverified-app warning. OpenAGC asks only for permission to read and organize mail (gmail.modify).")
                    .foregroundStyle(.secondary)
            }
            Section("Data on this Mac") {
                HStack {
                    Button("Show Mail Data") {
                        if let dir = try? CoreClient.defaultDataDirectory() { NSWorkspace.shared.activateFileViewerSelecting([dir]) }
                    }
                    Button("Show Logs") { NSWorkspace.shared.activateFileViewerSelecting([CoreClient.defaultLogDirectory()]) }
                }
            }
        }
        .formStyle(.grouped)
        .confirmationDialog("Sign out of Gmail?", isPresented: $confirmingSignOut) {
            Button("Sign Out", role: .destructive) { Task { await model.signOut() } }
        } message: {
            Text("OpenAGC forgets the sign-in. Mail already downloaded stays on this Mac until you delete it.")
        }
    }
}

/// The bring-your-own-client fields, shared with onboarding.
struct GoogleClientFields: View {
    @State private var customID = UserDefaults.standard.string(forKey: GoogleClientConfiguration.customClientIDKey) ?? ""
    @State private var customSecret = ""
    @State private var status: String?

    var body: some View {
        let client = GoogleClientConfiguration.effective()
        LabeledContent("Using") {
            Text(client.isCustom ? "Your own client" : (client.isUsable ? "OpenAGC's client" : "No client in this build"))
        }
        TextField("Client ID", text: $customID)
        SecureField("Client secret (optional)", text: $customSecret)
        HStack {
            Button("Save") {
                do {
                    try GoogleClientConfiguration.saveCustom(clientID: customID, clientSecret: customSecret)
                    status = customID.isEmpty ? "Using OpenAGC's client." : "Saved. Sign in again to use it."
                } catch {
                    status = String(describing: error)
                }
            }
            Link("How to create one", destination: URL(string: "https://github.com/audiojak/openagc/blob/main/docs/google-oauth-client.md")!)
            if let status { Text(status).foregroundStyle(.secondary).font(.callout) }
        }
    }
}

/// Settings › Privacy: what leaves the Mac, and remote images.
struct PrivacySettings: View {
    @State private var allowed: [String] = UserDefaults.standard.stringArray(forKey: ReaderStore.allowedSendersKey) ?? []

    var body: some View {
        Form {
            Section("What leaves this Mac") {
                Label("Mail syncs directly between Google and this Mac. OpenAGC has no servers and collects nothing.", systemImage: "lock.shield")
                Label("An agent sees mail only when you ask it something, through OpenAGC's tools, and every access is logged in Permissions › Activity.", systemImage: "sparkles")
                Label("A Claude cloud routine works on Anthropic's side, under the Gmail access you granted at claude.ai.", systemImage: "cloud")
            }
            .font(.callout)
            Section {
                if allowed.isEmpty {
                    Text("Remote images are blocked in every message until you load them.").foregroundStyle(.secondary)
                }
                ForEach(allowed, id: \.self) { sender in
                    HStack {
                        Text(sender)
                        Spacer()
                        Button("Remove") { save(allowed.filter { $0 != sender }) }.controlSize(.small)
                    }
                }
                if !allowed.isEmpty {
                    Button("Remove All", role: .destructive) { save([]) }
                }
            } header: {
                Text("Remote images always loaded from")
            } footer: {
                Text("Remote images can tell a sender when and where you opened a message, so they load only when you ask.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    private func save(_ list: [String]) {
        allowed = list
        UserDefaults.standard.set(list, forKey: ReaderStore.allowedSendersKey)
    }
}

/// Settings › Routines: a summary; editing happens in the Routines window.
struct RoutineSettings: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Form {
            Section {
                if model.routines.routines.isEmpty {
                    Text("No routines yet.").foregroundStyle(.secondary)
                }
                ForEach(model.routines.routines, id: \.id) { routine in
                    LabeledContent(routine.name) {
                        Text("\(RoutineRunner(rawValue: routine.runner)?.badge ?? "") · \(RoutinesStore.activity(model.routines.latestRuns[routine.id]))")
                    }
                }
                Button("Open Routines…") { openWindow(id: "routines") }
            } footer: {
                Text("A routine files automated mail into labels on a schedule, on Claude's cloud or here on this Mac.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .task { await model.routines.load() }
    }
}
