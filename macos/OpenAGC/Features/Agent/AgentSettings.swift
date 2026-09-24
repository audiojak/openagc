import SwiftUI

/// Settings › Agents (spec §9.2): which agent CLIs are installed, signed in
/// and new enough. Detection runs when this opens (cached per launch) and
/// again on Check Again.
struct AgentSettings: View {
    @Environment(AppModel.self) private var model
    @State private var providers: [AgentProviderInfo] = []
    @State private var checking = false

    var body: some View {
        Form {
            Section {
                if providers.isEmpty {
                    HStack {
                        ProgressView().controlSize(.small)
                        Text("Looking for Claude Code and Codex…").foregroundStyle(.secondary)
                    }
                }
                ForEach(providers, id: \.id) { provider in
                    AgentRow(provider: provider)
                }
            } header: {
                Text("Agents")
            } footer: {
                Text("OpenAGC uses the Claude Code or Codex command-line tool you already have, signed in with your own account. It never reads their credentials.")
                    .foregroundStyle(.secondary)
            }
            Section {
                Button(checking ? "Checking…" : "Check Again") { Task { await load(refresh: true) } }
                    .disabled(checking)
            }
        }
        .formStyle(.grouped)
        .task { await load(refresh: false) }
    }

    private func load(refresh: Bool) async {
        guard let core = model.core else { return }
        checking = true
        providers = await core.agentProviders(refresh: refresh)
        checking = false
    }
}

private struct AgentRow: View {
    let provider: AgentProviderInfo

    var body: some View {
        let status = AgentStatusText(provider)
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: status.symbol)
                .foregroundStyle(status.isReady ? .green : .secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(provider.name).font(.headline)
                Text(status.detail).font(.callout).foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// How an agent's status reads in the UI. Pure, so it is unit-tested.
struct AgentStatusText: Equatable {
    let symbol: String
    let detail: String
    let isReady: Bool

    init(_ provider: AgentProviderInfo) {
        let cli = provider.id == "codex" ? "codex" : "claude"
        switch provider.status {
        case let .ready(version):
            self.init("checkmark.circle.fill", "Ready — version \(version)", true)
        case .notInstalled:
            self.init("arrow.down.circle", provider.id == "codex"
                ? "Not installed. Install with: brew install codex"
                : "Not installed. Install with: brew install --cask claude-code", false)
        case let .notAuthenticated(version):
            self.init("person.crop.circle.badge.exclamationmark",
                      "Version \(version) is installed but not signed in. Run \(cli == "codex" ? "codex login" : "claude") in Terminal to sign in.", false)
        case let .updateRequired(version, minimum):
            self.init("exclamationmark.arrow.circlepath",
                      "Version \(version) is too old; OpenAGC needs \(minimum) or later.", false)
        case let .error(message):
            self.init("exclamationmark.triangle", "Couldn’t check: \(message)", false)
        }
    }

    private init(_ symbol: String, _ detail: String, _ isReady: Bool) {
        self.symbol = symbol
        self.detail = detail
        self.isReady = isReady
    }
}
