import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct AgentPanelTests {
    private func demo() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    private func waitUntil(_ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !condition() {
            guard ContinuousClock.now < deadline else { throw Timeout() }
            try await Task.sleep(for: .milliseconds(20))
        }
    }
    private struct Timeout: Error {}

    @Test func testsNeverSeeTheRealAgents() async throws {
        #expect(CoreClient.isRunningTests)
        let model = try await demo()
        await model.agent.loadProviders()
        #expect(model.agent.providers.first?.status == .ready(version: "1.0.0 (fake)"))
    }

    @Test func askingOpensTheInspectorAndStreamsTheReply() async throws {
        let model = try await demo()
        await model.agent.loadProviders()
        #expect(model.agent.isProviderReady)
        await model.askAgent("What needs a reply?")
        #expect(model.agent.isPresented)
        try await waitUntil { !model.agent.isRunning && model.agent.entries.count >= 2 }
        #expect(model.agent.entries.map(\.kind) == [.prompt("What needs a reply?"), .reply("You said: What needs a reply?")])
        #expect(model.agent.lastUsage == "15 tokens")
        model.agent.newConversation()
        #expect(model.agent.entries.isEmpty && model.agent.sessionID == nil)
    }

    @Test func eventsBuildACompactTranscript() async throws {
        let model = try await demo()
        let agent = model.agent
        await agent.loadProviders()
        await agent.send("find it", context: PromptContextInfo(mailboxId: nil, selectedThreadIds: [], searchQuery: nil))
        try await waitUntil { !agent.isRunning }
        let session = try #require(agent.sessionID)
        let thread = try #require(model.threads.rows.first)
        await agent.apply(sessionID: session, events: [
            .turnStarted,
            .thinkingDelta(text: "Hmm"),
            .toolCallStarted(callId: "c1", tool: "mail_search", argsSummary: "query: is:unread"),
            .toolCallFinished(callId: "c1", ok: true, summary: "{}"),
            .textDelta(text: "Here "),
            .textDelta(text: "they are."),
            .resultsAvailable(threadIds: [thread.id, "missing"]),
            .turnFailed(message: "Oops"),
        ])
        let kinds = agent.entries.dropFirst(2).map(\.kind)
        #expect(kinds[0] == .thinking("Hmm"))
        #expect(kinds[1] == .tool(name: "mail_search", arguments: "query: is:unread", state: .succeeded, summary: "{}"))
        #expect(kinds[2] == .reply("Here they are."))
        if case let .results(rows) = kinds[3] { #expect(rows.map(\.id) == [thread.id]) } else { Issue.record("\(kinds[3])") }
        #expect(kinds[4] == .error("Oops"))
        #expect(!agent.isRunning)
        // Events for another session are ignored.
        await agent.apply(sessionID: "other", events: [.textDelta(text: "nope")])
        #expect(agent.entries.count == 7)
    }

    @Test func toolTitlesAreFriendly() {
        #expect(AgentStore.toolTitle("mail_search") == "Searched mail")
        #expect(AgentStore.toolTitle("mail_future_tool") == "mail_future_tool")
        #expect(AgentStore.usageText(input: 10, output: 5, cost: 0.0123) == "15 tokens · $0.012")
        #expect(AgentStore.usageText(input: nil, output: nil, cost: nil) == nil)
    }
}

@MainActor
struct AgentHistoryTests {
    @Test func aConversationCanBeReopenedAndContinued() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        let agent = model.agent
        await agent.loadProviders()
        await model.askAgent("first question")
        var deadline = ContinuousClock.now + .seconds(5)
        while agent.isRunning || agent.entries.count < 2 {
            guard ContinuousClock.now < deadline else { Issue.record("timed out"); return }
            try await Task.sleep(for: .milliseconds(20))
        }
        let original = try #require(agent.sessionID)
        agent.newConversation()
        try await Task.sleep(for: .milliseconds(100))
        await agent.loadHistory()
        let stored = try #require(agent.history.first)
        #expect(stored.title == "first question")

        await agent.open(stored)
        #expect(agent.entries.map(\.kind) == [.prompt("first question"), .reply("You said: first question")])
        #expect(agent.resumeID == original && agent.sessionID == nil)

        await model.askAgent("second")
        deadline = ContinuousClock.now + .seconds(5)
        while agent.isRunning || agent.entries.count < 4 {
            guard ContinuousClock.now < deadline else { Issue.record("timed out"); return }
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(agent.sessionID == original, "the same conversation continues")
        #expect(agent.entries.last?.kind == .reply("You said: second"))
    }
}

@MainActor
struct ApprovalCardTests {
    @Test func proposalsBecomeCardsAndResolveFromTheirEvent() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        let agent = model.agent
        await agent.loadProviders()
        await model.askAgent("hi")
        let session = try #require(agent.sessionID)
        agent.isPresented = false
        await agent.apply(sessionID: session, events: [
            .actionProposed(actionId: 7, tool: "mail_send", summary: "Send “Lunch” to sam@example.org", draftId: 3),
            .actionProposed(actionId: 8, tool: "mail_delete", summary: "Move 2 threads to Trash", draftId: nil),
        ])
        #expect(agent.isPresented, "a proposal brings the panel up")
        #expect(agent.pendingProposals == [7, 8])
        await agent.apply(sessionID: session, events: [.actionResolved(actionId: 7, approved: true)])
        #expect(agent.pendingProposals == [8])
        // Resolving an action the core no longer waits on marks it declined.
        agent.resolve(8, approve: true)
        #expect(agent.pendingProposals.isEmpty)
        let states = agent.entries.compactMap { entry -> AgentStore.Entry.ProposalState? in
            if case let .proposal(_, _, _, _, state) = entry.kind { state } else { nil }
        }
        #expect(states == [.approved, .rejected])
    }

    @Test func reviewRequestsCarryTheAgentName() {
        #expect(ComposeRequest.review(draftID: 3, agent: "Claude").agentName == "Claude")
        #expect(ComposeRequest.draft(id: 3).agentName == nil)
    }
}
