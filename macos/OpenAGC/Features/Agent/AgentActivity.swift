import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Settings › Agents: which changes need approval, the optional API key,
/// and the activity log (spec §9.3, §10.3, §10.5).
struct AgentPermissionsSettings: View {
    @Environment(AppModel.self) private var model
    @State private var approve: Set<String> = Set(UserDefaults.standard.stringArray(forKey: AppModel.agentApprovalKey) ?? [])
    @State private var apiKey = ""
    @State private var hasKey = false
    @State private var keyError: String?
    @State private var showsActivity = false

    private static let keychainKey = "anthropic.api_key"

    var body: some View {
        Form {
            Section {
                ForEach(model.core?.configurableAgentTools ?? [], id: \.self) { tool in
                    Toggle(AgentStore.toolTitle(tool), isOn: binding(for: tool))
                }
            } header: {
                Text("Ask Before")
            } footer: {
                Text("Reading mail is always allowed. Sending, forwarding and deleting always ask. These changes can be undone, so by default the agent makes them straight away; turn one on to approve it each time.")
                    .foregroundStyle(.secondary)
            }
            Section {
                if hasKey {
                    HStack {
                        Label("A key is saved in your Keychain.", systemImage: "key.fill")
                        Spacer()
                        Button("Remove", role: .destructive, action: removeKey)
                    }
                } else {
                    HStack {
                        SecureField("sk-ant-…", text: $apiKey)
                            .textContentType(.password)
                        Button("Save", action: saveKey)
                            .disabled(apiKey.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                }
                if let keyError {
                    Text(keyError).foregroundStyle(.red).font(.callout)
                }
            } header: {
                Text("Claude API Key (Optional)")
            } footer: {
                Text("Only needed if your Claude subscription cannot be used from the command line. OpenAGC passes it to the claude process; it is never sent anywhere else.")
                    .foregroundStyle(.secondary)
            }
            Section {
                Button("Show Activity…") { showsActivity = true }
            } footer: {
                Text("Every tool call an agent made, what was decided and what it touched.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear { hasKey = ((try? KeychainSecretStore().get(Self.keychainKey)) ?? nil) != nil }
        .sheet(isPresented: $showsActivity) { AgentActivityView() }
    }

    private func binding(for tool: String) -> Binding<Bool> {
        Binding(
            get: { approve.contains(tool) },
            set: { on in
                if on { approve.insert(tool) } else { approve.remove(tool) }
                UserDefaults.standard.set(approve.sorted(), forKey: AppModel.agentApprovalKey)
                model.applyAgentPolicy()
            })
    }

    private func saveKey() {
        do {
            try KeychainSecretStore().set(Self.keychainKey, apiKey.trimmingCharacters(in: .whitespacesAndNewlines))
            apiKey = ""
            hasKey = true
            keyError = nil
        } catch {
            keyError = error.description
        }
    }

    private func removeKey() {
        do {
            try KeychainSecretStore().delete(Self.keychainKey)
            hasKey = false
        } catch {
            keyError = error.description
        }
    }
}

/// The audit log: newest first, exportable as JSON Lines.
struct AgentActivityView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var actions: [AgentActionInfo] = []

    var body: some View {
        VStack(spacing: 0) {
            Table(actions) {
                TableColumn("When") { a in
                    Text(Date(timeIntervalSince1970: TimeInterval(a.createdAt) / 1000), format: .dateTime.month().day().hour().minute())
                }
                .width(min: 90, ideal: 110)
                TableColumn("Action") { a in Text(AgentStore.toolTitle(a.tool)) }
                    .width(min: 110, ideal: 140)
                TableColumn("Decision") { a in Text(Self.stateText(a.state)).foregroundStyle(Self.stateColor(a.state)) }
                    .width(min: 80, ideal: 90)
                TableColumn("Details") { a in
                    Text(a.resultSummary ?? a.argumentsJson).lineLimit(1).truncationMode(.tail)
                        .help(a.argumentsJson)
                }
            }
            .overlay {
                if actions.isEmpty {
                    ContentUnavailableView("No Agent Activity", systemImage: "list.bullet.rectangle")
                }
            }
            Divider()
            HStack {
                Button("Export…", action: export).disabled(actions.isEmpty)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            .padding(12)
        }
        .frame(minWidth: 640, minHeight: 420)
        .task { actions = (try? await model.core?.agentActions()) ?? [] }
    }

    static func stateText(_ state: String) -> String {
        switch state {
        case "allowed", "done": "Done"
        case "approved": "Approved"
        case "pending": "Waiting"
        case "rejected": "Declined"
        case "expired": "Expired"
        case "denied": "Blocked"
        case "failed": "Failed"
        default: state.capitalized
        }
    }

    static func stateColor(_ state: String) -> Color {
        switch state {
        case "denied", "failed": .red
        case "rejected", "expired": .secondary
        case "pending": .orange
        default: .primary
        }
    }

    /// One JSON object per line, oldest first.
    static func jsonLines(_ actions: [AgentActionInfo]) -> String {
        actions.reversed().map { a in
            var object: [String: Any] = [
                "action_id": a.actionId, "session_id": a.sessionId, "tool": a.tool, "risk": a.risk,
                "state": a.state, "created_at": a.createdAt,
            ]
            object["arguments"] = (try? JSONSerialization.jsonObject(with: Data(a.argumentsJson.utf8))) ?? a.argumentsJson
            if let summary = a.resultSummary { object["result"] = summary }
            if let resolved = a.resolvedAt { object["resolved_at"] = resolved }
            let data = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])) ?? Data()
            return String(decoding: data, as: UTF8.self)
        }.joined(separator: "\n") + "\n"
    }

    private func export() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "openagc-agent-activity.jsonl"
        panel.allowedContentTypes = [UTType(filenameExtension: "jsonl") ?? .json]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        try? Self.jsonLines(actions).write(to: url, atomically: true, encoding: .utf8)
    }
}

extension AgentActionInfo: @retroactive Identifiable {
    public var id: Int64 { actionId }
}
