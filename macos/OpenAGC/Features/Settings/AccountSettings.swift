import AppKit
import SwiftUI

/// Settings › Accounts: the connected account, signing out or in again,
/// and which Google sign-in client to use (spec §7.3).
struct AccountSettings: View {
    @Environment(AppModel.self) private var model
    @State private var removing: AccountSummary?
    @State private var orphans: [OrphanedStore] = []

    var body: some View {
        Form {
            Section("Gmail") {
                switch model.accountState {
                case .open(let id) where id == AppModel.demoAccountID:
                    LabeledContent("Account") { Text("Demo mailbox (nothing leaves this Mac)") }
                    Button("Connect Gmail Instead…") { Task { await model.signIn(with: .effective()) } }
                        .hoverHelp("Sign in with Google to use your Gmail instead of the demo")
                        .disabled(!GoogleClientConfiguration.effective().isUsable)
                case .open:
                    ForEach(model.accounts, id: \.id) { account in
                        AccountRow(account: account, onRemove: { removing = account })
                    }
                    if model.accounts.isEmpty {
                        LabeledContent("Account") { Text(model.accountEmail ?? "Connected") }
                    }
                    if model.needsReauthentication {
                        Label(reauthenticationHint, systemImage: "exclamationmark.triangle").foregroundStyle(Tone.caution)
                    }
                    Button("Add Account…") { Task { await model.addAccount() } }
                        .hoverHelp("Sign in to another Gmail account")
                        .disabled(!GoogleClientConfiguration.effective().isUsable)
                    Button("Create an Account from an Archived Mailbox…") { Task { await model.beginImport() } }
                        .hoverHelp("Make a read-only account from an .mbox file, such as a Google Takeout export")
                        .disabled(model.runningImport != nil)
                case .signingIn:
                    HStack {
                        ProgressView().controlSize(.small)
                        Text("Finish signing in with Google in your browser…")
                    }
                default:
                    Text("No account is connected.").foregroundStyle(.secondary)
                    Button("Connect Gmail…") { Task { await model.signIn(with: .effective()) } }
                        .hoverHelp("Sign in with Google to add your Gmail")
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
                ForEach(orphans, id: \.id) { orphan in
                    HStack {
                        VStack(alignment: .leading, spacing: Space.hair) {
                            Text("Leftover mail from \(orphan.email ?? "an old sign-in")")
                            Text(ByteCountFormatter.string(fromByteCount: Int64(orphan.bytes), countStyle: .file))
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Button("Delete", role: .destructive) {
                            Task {
                                try? await model.core?.removeOrphanedStore(orphan.id)
                                orphans = (try? await model.core?.orphanedStores()) ?? []
                            }
                        }
                    }
                    .hoverHelp("A copy of downloaded mail that no account in OpenAGC uses any more. Gmail is not affected")
                }
                HStack {
                    Button("Show Mail Data") {
                        if let dir = try? CoreClient.defaultDataDirectory() { NSWorkspace.shared.activateFileViewerSelecting([dir]) }
                    }
                    .hoverHelp("Show the folder where OpenAGC keeps downloaded mail, in Finder")
                    Button("Show Logs") { NSWorkspace.shared.activateFileViewerSelecting([CoreClient.defaultLogDirectory()]) }
                        .hoverHelp("Show OpenAGC's log files in Finder")
                }
            }
        }
        .formStyle(.grouped)
        .task(id: model.accounts.map(\.id)) { orphans = (try? await model.core?.orphanedStores()) ?? [] }
        .confirmationDialog("Remove \(removing?.email ?? "this account") from OpenAGC?",
                            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } })) {
            Button("Remove", role: .destructive) { // no-help: confirmation dialog button
                if let account = removing { Task { await model.removeAccount(account.id) } }
                removing = nil
            }
        } message: {
            Text("OpenAGC forgets the sign-in and deletes the mail it downloaded for this account. Gmail itself is not changed.")
        }
    }
}

extension AccountSettings {
    /// One line on why syncing stopped. A saved sign-in the Keychain will
    /// not hand over (typically a rebuilt development app) is not Google's
    /// doing, and the mail already here is safe.
    var reauthenticationHint: String {
        switch model.reauthenticationReason {
        case .savedSignInUnavailable:
            "The saved sign-in isn't available to this copy of OpenAGC, so mail isn't syncing. Downloaded mail is kept; sign in again to resume."
        case .googleRejected, nil:
            "Google asked you to sign in again."
        }
    }
}

/// One account in Settings › Accounts (spec §7.7): who it is, whether it
/// is syncing, how far back it downloads, and Remove….
struct AccountRow: View {
    @Environment(AppModel.self) private var model
    let account: AccountSummary
    let onRemove: () -> Void
    @State private var window: SyncWindow?
    @State private var bodyWindow: BodyWindow?
    @State private var signedIn: Bool?
    @State private var backfill: BackfillStatus?
    @State private var name = ""
    @State private var nameError: String?
    @FocusState private var nameFocused: Bool

    /// Above this many messages, suggest IMAP to accounts without it.
    static let suggestIMAPAbove: UInt64 = 20_000

    static let bodyWindowChoices: [(BodyWindow, String)] = [
        (.month, "Last 30 days"), (.halfYear, "Last 6 months"), (.window, "Everything downloaded"),
    ]

    /// Whether to suggest IMAP: a large mailbox still on the API.
    /// The Downloads row: which transport and, if not IMAP, why.
    static func transportText(imapEnabled: Bool, transport: String?) -> String {
        guard imapEnabled else { return "Over the Gmail API (this sign-in does not allow IMAP)" }
        switch transport {
        case "imap": return "Over IMAP"
        case "imap-refused": return "Over the Gmail API (IMAP was refused)"
        case nil, "none": return "Over IMAP when syncing"
        default: return "Over the Gmail API"
        }
    }

    /// The name as it can be changed: an archive's is its listed name.
    static func editableName(_ account: AccountSummary) -> String {
        account.kind == .archive ? account.email : account.displayName ?? ""
    }

    private func rename() async {
        guard let core = model.core, name != Self.editableName(account) else { return }
        do {
            try await core.renameAccount(account.id, to: name)
            nameError = nil
            await model.reloadAccounts()
        } catch {
            nameError = error.message
            name = Self.editableName(account)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Space.m) {
            HStack(spacing: Space.m) {
                AccountAvatar(account: account, size: 32)
                VStack(alignment: .leading, spacing: Space.hair) {
                    Text(account.displayName ?? account.email).font(.body.weight(.medium))
                    if account.displayName != nil { Text(account.email).font(.caption).foregroundStyle(.secondary) }
                    Text(status).font(.caption).foregroundStyle(signedIn == false ? .orange : .secondary)
                }
                Spacer()
                if account.id != model.openAccountID {
                    Button("Show") { Task { await model.switchAccount(to: account.id) } }
                        .hoverHelp("Switch the window to this account")
                }
                if account.kind == .gmail, signedIn == true {
                    Button("Refresh from Gmail") { Task { _ = try? await model.core?.refreshFromServer(account.id) } }
                        .hoverHelp("Download this account's mail again so labels and messages match Gmail. Nothing is sent or changed on the server")
                }
                if account.kind == .archive {
                    Button("Re-import…") { Task { _ = try? await model.core?.reimportArchive(account.id) } }
                        .hoverHelp("Import the mailbox file again, adding anything missing")
                        .disabled(model.imports[account.id].map { !$0.done } ?? false)
                }
                if signedIn == false {
                    Button("Sign In…") {
                        Task {
                            await model.switchAccount(to: account.id)
                            await model.signIn(with: .effective())
                        }
                    }
                    .hoverHelp("Sign in to Google again for this account")
                }
                Button("Remove…", role: .destructive, action: onRemove)
                    .hoverHelp("Remove this account from OpenAGC; Gmail itself is not changed")
            }
            TextField("Name", text: $name,
                      prompt: Text(account.kind == .archive ? "A name for this mailbox" : "Your name from Google"))
                .focused($nameFocused)
                .onSubmit { Task { await rename() } }
                .onChange(of: nameFocused) { _, focused in if !focused { Task { await rename() } } }
                .hoverHelp(account.kind == .archive ? "What this mailbox is called in OpenAGC"
                           : "Shown beside the address in OpenAGC; leave empty to use your Google profile's name")
                .onAppear { name = Self.editableName(account) }
                .onChange(of: account.displayName) { name = Self.editableName(account) }
                .onChange(of: account.email) { name = Self.editableName(account) }
            if let nameError {
                Text(nameError).font(.caption).foregroundStyle(Tone.failure)
            }
            if account.kind == .gmail {
                Picker("Download mail from", selection: Binding(
                    get: { window ?? .halfYear },
                    set: { newValue in
                        window = newValue
                        Task { try? await model.core?.setSyncWindow(newValue, for: account.id) }
                    }
                )) {
                    ForEach(SyncWindowSection.choices, id: \.0) { choice in
                        Text(choice.1).tag(choice.0)
                    }
                }
                .hoverHelp("How far back OpenAGC keeps a copy of this account's mail")
                .disabled(window == nil)
                LabeledContent("Downloads") {
                    Text(Self.transportText(imapEnabled: account.imapEnabled, transport: backfill?.transport))
                        .foregroundStyle(.secondary)
                }
                .hoverHelp("IMAP is used for downloading; the Gmail API for categories, drafts, changes made elsewhere and sending, and whenever IMAP fails")
                if !account.imapEnabled, signedIn == true {
                    Button("Sign In Again for IMAP…") { Task { await model.signInAgainForIMAP(account.id) } }
                        .hoverHelp("Grant the full mail access IMAP needs; downloads get much faster. OpenAGC still never deletes mail permanently")
                }
                if account.imapEnabled {
                    // Tiered download (spec §7.4): older mail in the range
                    // comes down as headers; bodies when needed.
                    Picker("Full messages for", selection: Binding(
                        get: { bodyWindow ?? .month },
                        set: { newValue in
                            bodyWindow = newValue
                            Task { try? await model.core?.setBodyWindow(newValue, for: account.id) }
                        }
                    )) {
                        ForEach(Self.bodyWindowChoices, id: \.0) { choice in
                            Text(choice.1).tag(choice.0)
                        }
                    }
                    .disabled(bodyWindow == nil)
                    .hoverHelp("Older mail shows its sender, subject and a preview; its full text downloads when you open it, search for it, or an agent reads it. The Inbox always comes down in full")
                }
            }
        }
        .padding(.vertical, Space.hair)
        .task(id: account.id) {
            window = try? await model.core?.syncWindow(for: account.id)
            bodyWindow = account.kind == .gmail ? try? await model.core?.bodyWindow(for: account.id) : nil
            signedIn = account.kind == .gmail ? ((try? model.core?.accountHasCredentials(account.id)) ?? false) : nil
            backfill = await model.core?.backfillStatus(account.id)
        }
    }

    /// Which transport backfill is using, when it is not the default.
    private var transportNote: String {
        switch backfill?.transport {
        case "imap":
            let today = ByteCountFormatter.string(fromByteCount: Int64(backfill?.imapBytesToday ?? 0), countStyle: .file)
            return " · over IMAP (\(today) today)"
        case "imap-refused": return " · IMAP refused by Google, using the API"
        default: return ""
        }
    }

    private var status: String {
        switch (account.kind, signedIn) {
        case (.archive, _):
            if let status = model.imports[account.id], !status.done {
                "Importing… \(status.imported.formatted()) messages"
            } else {
                "Imported mailbox · cannot send"
            }
        case (_, false?): "Not syncing — sign in again"
        case (_, true?):
            (account.id == model.openAccountID ? "Showing · syncing" : "Syncing in the background") + transportNote
        default: " "
        }
    }
}

/// How far back mail is downloaded (spec §7.4). The inbox and the last 30
/// days always come down; this bounds the rest.
struct SyncWindowSection: View {
    @Environment(AppModel.self) private var model
    @State private var window: SyncWindow?

    var body: some View {
        Section {
            Picker("Download mail from", selection: Binding(
                get: { window ?? .halfYear },
                set: { newValue in
                    window = newValue
                    Task { try? await model.core?.setSyncWindow(newValue) }
                }
            )) {
                ForEach(SyncWindowSection.choices, id: \.0) { choice in
                    Text(choice.1).tag(choice.0)
                }
            }
            .hoverHelp("How far back OpenAGC keeps a copy of your mail")
            .disabled(window == nil)
        } header: {
            Text("Mail on this Mac")
        } footer: {
            Text("The inbox and the last 30 days are always downloaded. Older mail outside this range stays in Gmail and is not searchable here; widening the range downloads it, narrowing keeps what is already here.")
                .foregroundStyle(.secondary)
        }
        .task { window = try? await model.core?.syncWindow() }
    }

    static let choices: [(SyncWindow, String)] = [
        (.month, "Last month"), (.halfYear, "Last 6 months"), (.year, "Last year"), (.everything, "Everything"),
    ]
}

/// The bring-your-own-client fields, shared with onboarding.
struct GoogleClientFields: View {
    @State private var customID = CoreClient.appDefaults().string(forKey: GoogleClientConfiguration.customClientIDKey) ?? ""
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
            .hoverHelp("Use this Google client for sign-in")
            Link("How to create one", destination: URL(string: "https://github.com/audiojak/openagc/blob/main/docs/google-oauth-client.md")!)
            if let status { Text(status).foregroundStyle(.secondary).font(.callout) }
        }
    }
}

/// Settings › Privacy: what leaves the Mac, and remote images.
struct PrivacySettings: View {
    @State private var allowed: [String] = CoreClient.appDefaults().stringArray(forKey: ReaderStore.allowedSendersKey) ?? []

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
                            .hoverHelp("Stop loading remote images from this sender automatically")
                    }
                }
                if !allowed.isEmpty {
                    Button("Remove All", role: .destructive) { save([]) }
                        .hoverHelp("Stop loading remote images automatically from any sender")
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
        CoreClient.appDefaults().set(list, forKey: ReaderStore.allowedSendersKey)
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
                    .hoverHelp("Open the Routines window")
            } footer: {
                Text("A routine files automated mail into labels on a schedule, on Claude's cloud or here on this Mac.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .task { await model.routines.load() }
    }
}
