import SwiftUI

/// First run: connect Gmail (shipped client or your own), or explore the
/// demo mailbox. Explains what leaves the Mac (spec §2.1 of the product spec).
struct OnboardingView: View {
    @Environment(AppModel.self) private var model
    @State private var showAdvanced = false

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

                OnboardingAgents()

                DisclosureGroup("Advanced: use your own Google OAuth client", isExpanded: $showAdvanced) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Create a “Desktop app” OAuth client in Google Cloud with the Gmail API enabled, then paste its ID and secret. Your own client avoids Google's unverified-app warning.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                        Link("Step-by-step instructions", destination: URL(string: "https://github.com/audiojak/openagc/blob/main/docs/google-oauth-client.md")!)
                            .font(.callout)
                        GoogleClientFields()
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

/// Which agents are installed, and which to use by default (spec §9.2).
private struct OnboardingAgents: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let agent = model.agent
        VStack(alignment: .leading, spacing: 8) {
            Text("Agents").font(.headline)
            Text("OpenAGC works with the Claude Code or Codex command-line tools you already have, signed in with your own account. You can add one later.")
                .font(.callout)
                .foregroundStyle(.secondary)
            if agent.providers.isEmpty {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Looking for Claude Code and Codex…").foregroundStyle(.secondary)
                }
            }
            ForEach(agent.providers, id: \.id) { provider in
                let status = AgentStatusText(provider)
                Label {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(provider.name)
                        Text(status.detail).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                } icon: {
                    Image(systemName: status.symbol).foregroundStyle(status.isReady ? .green : .secondary)
                }
            }
            let ready = agent.providers.filter { AgentStatusText($0).isReady }
            if ready.count > 1 {
                Picker("Ask by default", selection: Bindable(agent).providerID) {
                    ForEach(ready, id: \.id) { Text($0.name).tag($0.id) }
                }
                .fixedSize()
            }
            Button("Check Again") { Task { await agent.loadProviders(refresh: true) } }
                .controlSize(.small)
        }
        .task { await agent.loadProviders() }
    }
}
