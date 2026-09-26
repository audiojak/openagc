import Foundation
import Testing
@testable import OpenAGC

struct AgentFFITests {
    @Test func aFakeAgentStreamsItsReplyToSwift() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let core = try CoreClient(dataDirectory: dir)
        core.useFakeAgents()
        try await core.openAccount(AppModel.demoAccountID)

        let providers = await core.agentProviders()
        #expect(providers.map(\.id) == ["claude-code", "codex"])
        #expect(providers.first?.name == "Claude")
        if case .ready = providers[0].status {} else { Issue.record("Claude should be ready: \(providers[0].status)") }

        let session = try await core.startAgentSession(provider: "claude-code")
        try await core.sendAgentPrompt(session, "What needs a reply?")

        var text = ""
        var completed = false
        let deadline = ContinuousClock.now + .seconds(5)
        for await event in core.events {
            guard case let .agent(sessionID, events) = event.event, sessionID == session else { continue }
            for e in events {
                if case let .textDelta(delta) = e { text += delta }
                if case .turnCompleted = e { completed = true }
            }
            if completed || ContinuousClock.now > deadline { break }
        }
        #expect(completed)
        #expect(text == "You said: What needs a reply?")
        try await core.closeAgentSession(session)
    }
}

struct SystemPromptTests {
    @Test func theSystemPromptShipsAndCoversTheRules() throws {
        let url = try #require(Bundle.main.url(forResource: "agent-system-prompt", withExtension: "md"))
        let text = try String(contentsOf: url, encoding: .utf8)
        #expect(text.contains("untrusted"))
        #expect(text.contains("mail_present_threads"))
        #expect(text.contains("rejected_by_user"))
        #expect(text.utf8.count < 4_000, "every word costs every turn")
        // Every tool the prompt names exists.
        let named = text.matches(of: /`(mail_[a-z_]+)`/).map { String($0.1) }
        #expect(!named.isEmpty)
        let known: Set<String> = ["mail_search", "mail_get_thread", "mail_get_message", "mail_list_labels",
                                  "mail_get_attachment_text", "mail_present_threads", "mail_create_draft",
                                  "mail_update_draft", "mail_archive", "mail_mark_read", "mail_mark_unread",
                                  "mail_add_label", "mail_remove_label", "mail_create_label", "mail_send",
                                  "mail_forward", "mail_delete"]
        for name in named { #expect(known.contains(name), "\(name)") }
    }
}
