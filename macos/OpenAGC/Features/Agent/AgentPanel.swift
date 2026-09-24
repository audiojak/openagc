import SwiftUI

/// The prompt bar under the thread list (spec §14.6): "Ask Claude…" with
/// the agent switcher. Sending opens the inspector.
struct AgentPromptBar: View {
    @Environment(AppModel.self) private var model
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        let agent = model.agent
        HStack(spacing: 8) {
            Menu {
                ForEach(agent.providers, id: \.id) { provider in
                    Button {
                        agent.providerID = provider.id
                    } label: {
                        if provider.id == agent.providerID {
                            Label(provider.name, systemImage: "checkmark")
                        } else {
                            Text(provider.name)
                        }
                    }
                    .disabled({ if case .ready = provider.status { false } else { true } }())
                }
                Divider()
                Button("Agent Settings…") { NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil) }
            } label: {
                Image(systemName: "sparkles")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Choose the agent")

            TextField("Ask \(agent.providerName)…", text: $text)
                .textFieldStyle(.plain)
                .focused($focused)
                .onSubmit(send)
                .disabled(!agent.isProviderReady)
                .accessibilityLabel("Ask \(agent.providerName)")

            if agent.isRunning {
                Button("Stop", systemImage: "stop.circle.fill") { agent.cancel() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .help("Stop the agent")
            } else {
                Button("Send", systemImage: "arrow.up.circle.fill", action: send)
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .disabled(text.trimmingCharacters(in: .whitespaces).isEmpty || !agent.isProviderReady)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
        .task { await agent.loadProviders() }
        .onChange(of: model.agentFocusRequests) { focused = true }
    }

    private func send() {
        let prompt = text
        text = ""
        Task { await model.askAgent(prompt) }
    }
}

/// The inspector column: transcript, results as thread rows, cancel.
struct AgentInspector: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let agent = model.agent
        VStack(spacing: 0) {
            HStack {
                Text(agent.providerName).font(.headline)
                Spacer()
                if agent.pendingProposals.count > 1 {
                    Button("Approve All (\(agent.pendingProposals.count))") { agent.approveAll() }
                        .controlSize(.small)
                }
                if agent.isRunning {
                    ProgressView().controlSize(.small)
                    Button("Stop") { agent.cancel() }
                        .controlSize(.small)
                }
                Menu {
                    if agent.history.isEmpty {
                        Text("No earlier conversations")
                    }
                    ForEach(agent.history, id: \.sessionId) { conversation in
                        Button(conversation.title.isEmpty ? "Untitled" : conversation.title) {
                            Task { await agent.open(conversation) }
                        }
                    }
                } label: {
                    Image(systemName: "clock.arrow.circlepath")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("Earlier conversations")
                .onAppear { Task { await agent.loadHistory() } }
                Button("New Conversation", systemImage: "square.and.pencil") { agent.newConversation() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .help("Start a new conversation")
                    .disabled(agent.entries.isEmpty)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            Divider()
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(agent.entries) { entry in
                            EntryView(entry: entry).id(entry.id)
                        }
                    }
                    .padding(12)
                }
                .onChange(of: agent.entries.last) { _, last in
                    if let last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
            }
            if let usage = agent.lastUsage {
                Divider()
                Text(usage).font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .trailing)
                    .padding(.horizontal, 12).padding(.vertical, 4)
            }
        }
        .overlay {
            if agent.entries.isEmpty {
                ContentUnavailableView("Ask About Your Mail", systemImage: "sparkles",
                                       description: Text("“What needs a reply today?” “Summarize the thread with Alex.”"))
            }
        }
    }
}

private struct EntryView: View {
    let entry: AgentStore.Entry
    @Environment(AppModel.self) private var model
    @State private var expanded = false

    var body: some View {
        switch entry.kind {
        case let .prompt(text):
            Text(text)
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.tint.opacity(0.12), in: .rect(cornerRadius: 8))
                .textSelection(.enabled)
        case let .reply(text):
            Text(LocalizedStringKey(text))
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
        case let .thinking(text):
            DisclosureGroup("Thinking", isExpanded: $expanded) {
                Text(text).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            }
            .font(.callout)
            .foregroundStyle(.secondary)
        case let .tool(name, arguments, state, summary):
            DisclosureGroup(isExpanded: $expanded) {
                VStack(alignment: .leading, spacing: 4) {
                    if !arguments.isEmpty { Text(arguments) }
                    if !summary.isEmpty { Text(summary).foregroundStyle(.secondary) }
                }
                .font(.caption.monospaced())
                .textSelection(.enabled)
            } label: {
                HStack(spacing: 6) {
                    switch state {
                    case .running: ProgressView().controlSize(.mini)
                    case .succeeded: Image(systemName: "checkmark.circle").foregroundStyle(.secondary)
                    case .failed: Image(systemName: "xmark.octagon").foregroundStyle(.red)
                    }
                    Text(AgentStore.toolTitle(name))
                    if !arguments.isEmpty {
                        Text(arguments).foregroundStyle(.tertiary).lineLimit(1)
                    }
                }
                .font(.callout)
                .foregroundStyle(.secondary)
            }
        case let .results(rows):
            VStack(alignment: .leading, spacing: 0) {
                ForEach(rows, id: \.id) { row in
                    ResultRow(row: row, selected: model.selectedThreadID == row.id)
                        .contentShape(.rect)
                        .onTapGesture { model.selectedThreadID = row.id }
                    Divider()
                }
            }
            .background(.background, in: .rect(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
        case let .error(message):
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .font(.callout)
                .foregroundStyle(.orange)
        case let .proposal(actionID, tool, summary, draftID, state):
            ProposalCard(actionID: actionID, tool: tool, summary: summary, draftID: draftID, state: state)
        }
    }
}

/// An action waiting for the user (spec §10.4): what it does, a way to
/// review the message for sends and forwards, and the decision.
private struct ProposalCard: View {
    let actionID: Int64
    let tool: String
    let summary: String
    let draftID: Int64?
    let state: AgentStore.Entry.ProposalState
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(summary, systemImage: Self.symbol(tool))
                .font(.callout.weight(.medium))
                .fixedSize(horizontal: false, vertical: true)
            switch state {
            case .pending:
                HStack {
                    if let draftID {
                        Button("Review…") {
                            model.compose(.review(draftID: draftID, agent: model.agent.providerName))
                        }
                    }
                    Spacer()
                    Button("Reject", role: .destructive) { model.agent.resolve(actionID, approve: false) }
                    Button("Approve") { model.agent.resolve(actionID, approve: true) }
                        .buttonStyle(.borderedProminent)
                }
                .controlSize(.small)
            case .approved:
                Label("Approved", systemImage: "checkmark.circle.fill").font(.caption).foregroundStyle(.green)
            case .rejected:
                Label("Declined", systemImage: "xmark.circle").font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .background(state == .pending ? AnyShapeStyle(.yellow.opacity(0.12)) : AnyShapeStyle(.quaternary.opacity(0.4)),
                    in: .rect(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(state == .pending ? .yellow.opacity(0.6) : .clear))
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Proposed: \(summary)")
    }

    static func symbol(_ tool: String) -> String {
        switch tool {
        case "mail_send": "paperplane"
        case "mail_forward": "arrowshape.turn.up.right"
        case "mail_delete": "trash"
        default: "hand.raised"
        }
    }
}

/// A thread from the agent's results, like a row in the main list.
private struct ResultRow: View {
    let row: ThreadRow
    let selected: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(ThreadRowView.senderLine(row))
                    .fontWeight(row.unreadCount > 0 ? .semibold : .regular)
                    .lineLimit(1)
                Spacer()
                Text(RowDateFormatter.string(forMillis: row.lastMessageAt))
                    .font(.caption).foregroundStyle(.secondary)
            }
            Text(row.subject.isEmpty ? "(no subject)" : row.subject).font(.callout).lineLimit(1)
            Text(row.snippet).font(.caption).foregroundStyle(.secondary).lineLimit(1)
        }
        .padding(8)
        .background(selected ? AnyShapeStyle(.selection) : AnyShapeStyle(.clear))
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
    }
}
