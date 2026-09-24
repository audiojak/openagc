import Foundation
import Observation
import os

/// The agent panel's state (spec §14.6): which agent, the live session, and
/// a compact transcript built from the core's batched events.
@MainActor
@Observable
final class AgentStore {
    static let providerKey = "agentProvider"

    /// One line (or block) of the transcript.
    struct Entry: Identifiable, Equatable {
        enum Kind: Equatable {
            case prompt(String)
            case reply(String)
            case thinking(String)
            case tool(name: String, arguments: String, state: ToolState, summary: String)
            case results([ThreadRow])
            case error(String)
        }

        enum ToolState: Equatable { case running, succeeded, failed }

        let id: Int
        var kind: Kind
    }

    private(set) var providers: [AgentProviderInfo] = []
    var providerID: String = UserDefaults.standard.string(forKey: providerKey) ?? "claude-code" {
        didSet { UserDefaults.standard.set(providerID, forKey: Self.providerKey) }
    }
    private(set) var sessionID: String?
    private(set) var entries: [Entry] = []
    private(set) var isRunning = false
    var isPresented = false
    /// Tokens and cost of the last turn, for the footer.
    private(set) var lastUsage: String?

    @ObservationIgnored private let core: CoreClient?
    @ObservationIgnored private var nextID = 0
    @ObservationIgnored private var toolEntries: [String: Int] = [:]
    @ObservationIgnored private let logger = Logger(subsystem: "ai.actual.openagc", category: "agent")

    init(core: CoreClient?) {
        self.core = core
    }

    var provider: AgentProviderInfo? { providers.first { $0.id == providerID } }
    var providerName: String { provider?.name ?? (providerID == "codex" ? "Codex" : "Claude") }

    var isProviderReady: Bool {
        if case .ready = provider?.status { return true }
        return false
    }

    func loadProviders(refresh: Bool = false) async {
        guard let core else { return }
        providers = await core.agentProviders(refresh: refresh)
        // Fall back to whichever agent is ready.
        if !isProviderReady, let ready = providers.first(where: { if case .ready = $0.status { true } else { false } }) {
            providerID = ready.id
        }
    }

    // MARK: Prompts

    func send(_ prompt: String, context: PromptContextInfo) async {
        let text = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let core, !text.isEmpty, !isRunning else { return }
        isPresented = true
        append(.prompt(text))
        isRunning = true
        do {
            if sessionID == nil {
                sessionID = try await core.startAgentSession(provider: providerID)
            }
            try await core.sendAgentPrompt(sessionID!, text, context: context)
        } catch {
            isRunning = false
            append(.error(error.message))
        }
    }

    func cancel() {
        guard let core, let sessionID, isRunning else { return }
        Task { try? await core.cancelAgentTurn(sessionID) }
    }

    /// Start over: close the session and clear the transcript.
    func newConversation() {
        if let core, let sessionID { Task { try? await core.closeAgentSession(sessionID) } }
        sessionID = nil
        entries = []
        toolEntries = [:]
        isRunning = false
        lastUsage = nil
    }

    // MARK: Events

    func apply(sessionID: String, events: [AgentEventInfo]) async {
        guard sessionID == self.sessionID else { return }
        for event in events {
            switch event {
            case .sessionStarted, .sessionEnded:
                break
            case .turnStarted:
                isRunning = true
            case let .textDelta(text):
                appendText(text, thinking: false)
            case let .thinkingDelta(text):
                appendText(text, thinking: true)
            case let .toolCallStarted(callID, tool, args):
                toolEntries[callID] = append(.tool(name: tool, arguments: args, state: .running, summary: ""))
            case let .toolCallFinished(callID, ok, summary):
                if let id = toolEntries[callID], let i = entries.firstIndex(where: { $0.id == id }),
                   case let .tool(name, args, _, _) = entries[i].kind {
                    entries[i].kind = .tool(name: name, arguments: args, state: ok ? .succeeded : .failed, summary: summary)
                }
            case .actionProposed:
                break // Approval cards arrive with the write tools (M4).
            case let .resultsAvailable(threadIDs):
                let rows = await rows(for: threadIDs)
                if !rows.isEmpty { append(.results(rows)) }
            case let .turnCompleted(input, output, cost):
                isRunning = false
                lastUsage = Self.usageText(input: input, output: output, cost: cost)
            case let .turnFailed(message):
                isRunning = false
                append(.error(message))
            }
        }
    }

    private func rows(for ids: [String]) async -> [ThreadRow] {
        guard let core else { return [] }
        var out: [ThreadRow] = []
        for id in ids {
            if let detail = try? await core.thread(id) { out.append(detail.thread) }
        }
        return out
    }

    @discardableResult
    private func append(_ kind: Entry.Kind) -> Int {
        nextID += 1
        entries.append(Entry(id: nextID, kind: kind))
        return nextID
    }

    /// Consecutive deltas extend the last reply (or thinking) block.
    private func appendText(_ text: String, thinking: Bool) {
        if let last = entries.indices.last {
            switch (entries[last].kind, thinking) {
            case let (.reply(existing), false):
                entries[last].kind = .reply(existing + text)
                return
            case let (.thinking(existing), true):
                entries[last].kind = .thinking(existing + text)
                return
            default:
                break
            }
        }
        append(thinking ? .thinking(text) : .reply(text))
    }

    static func usageText(input: UInt64?, output: UInt64?, cost: Double?) -> String? {
        var parts: [String] = []
        if let input, let output { parts.append("\(input + output) tokens") }
        if let cost { parts.append(String(format: "$%.3f", cost)) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    /// "Searched mail" rather than "mail_search".
    static func toolTitle(_ name: String) -> String {
        switch name {
        case "mail_search": "Searched mail"
        case "mail_get_thread": "Read a thread"
        case "mail_get_message": "Read a message"
        case "mail_list_labels": "Listed labels"
        case "mail_get_attachment_text": "Read an attachment"
        case "mail_present_threads": "Showed threads"
        case "mail_create_draft": "Wrote a draft"
        case "mail_update_draft": "Edited a draft"
        case "mail_archive": "Archived"
        case "mail_mark_read": "Marked read"
        case "mail_mark_unread": "Marked unread"
        case "mail_add_label": "Added a label"
        case "mail_remove_label": "Removed a label"
        case "mail_create_label": "Created a label"
        case "mail_send": "Asked to send"
        case "mail_forward": "Asked to forward"
        case "mail_delete": "Asked to delete"
        default: name
        }
    }
}
