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
            guard case let .agent(sessionID, events) = event, sessionID == session else { continue }
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
