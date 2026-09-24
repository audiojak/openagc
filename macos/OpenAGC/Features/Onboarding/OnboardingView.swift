import SwiftUI

/// First run: connect Gmail (shipped client or your own), or explore the
/// demo mailbox. Explains what leaves the Mac (spec §2.1 of the product spec).
struct OnboardingView: View {
    @Environment(AppModel.self) private var model
    @State private var showAdvanced = false
    @State private var customID = UserDefaults.standard.string(forKey: GoogleClientConfiguration.customClientIDKey) ?? ""
    @State private var customSecret = ""
    @State private var saveError: String?

    private var client: GoogleClientConfiguration { GoogleClientConfiguration.effective() }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Welcome to OpenAGC").font(.title.weight(.semibold))
                    Text("A Mac email client that works with the AI agents you already use.")
                        .foregroundStyle(.secondary)
                }

                VStack(alignment: .leading, spacing: 8) {
                    Label("Your mail syncs straight from Google to this Mac. OpenAGC has no servers.", systemImage: "lock.shield")
                    Label("Agents see mail only when you ask them to, through tools you control.", systemImage: "sparkles")
                    Label("Sending and deleting always wait for your approval.", systemImage: "hand.raised")
                }
                .font(.callout)

                if model.accountState == .signingIn {
                    HStack(spacing: 10) {
                        ProgressView().controlSize(.small)
                        Text("Finish signing in with Google in your browser…")
                        Spacer()
                        Button("Cancel") { model.cancelSignIn() }
                    }
                } else {
                    HStack {
                        Button {
                            Task { await model.signIn(with: client) }
                        } label: {
                            Label("Connect Gmail", systemImage: "envelope.badge")
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.large)
                        .disabled(!client.isUsable)
                        Button("Explore a Demo Mailbox") { Task { await model.openDemoMailbox() } }
                            .controlSize(.large)
                    }
                    if !client.isUsable {
                        Text("This build doesn't include a Google sign-in client yet. Add your own under Advanced; it takes about five minutes.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                    if let error = model.signInError {
                        Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.red).font(.callout)
                    }
                }

                DisclosureGroup("Advanced: use your own Google OAuth client", isExpanded: $showAdvanced) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Create a “Desktop app” OAuth client in Google Cloud with the Gmail API enabled, then paste its ID and secret. Your own client avoids Google's unverified-app warning.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                        Link("Step-by-step instructions", destination: URL(string: "https://github.com/audiojak/openagc/blob/main/docs/google-oauth-client.md")!)
                            .font(.callout)
                        TextField("Client ID", text: $customID)
                        SecureField("Client secret", text: $customSecret)
                        HStack {
                            Button("Save") {
                                do {
                                    try GoogleClientConfiguration.saveCustom(clientID: customID, clientSecret: customSecret)
                                    saveError = nil
                                } catch {
                                    saveError = String(describing: error)
                                }
                            }
                            if client.isCustom { Text("Using your client").foregroundStyle(.secondary).font(.callout) }
                            if let saveError { Text(saveError).foregroundStyle(.red).font(.callout) }
                        }
                    }
                    .textFieldStyle(.roundedBorder)
                    .padding(.top, 6)
                }
            }
            .padding(28)
            .frame(maxWidth: 560, alignment: .leading)
        }
    }
}
