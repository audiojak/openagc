import Foundation
import Testing
@testable import OpenAGC

struct CoreEventTests {
    private func client() throws -> CoreClient {
        try CoreClient(dataDirectory: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString))
    }

    /// Collects events for `duration`, long enough for Rust's 50 ms window.
    private func collect(from client: CoreClient, for duration: Duration = .milliseconds(300)) async -> [CoreClientEvent] {
        await withTaskGroup(of: [CoreClientEvent].self) { group in
            group.addTask {
                var events: [CoreClientEvent] = []
                for await event in client.events { events.append(event.event) }
                return events
            }
            try? await Task.sleep(for: duration)
            group.cancelAll()
            return await group.next() ?? []
        }
    }

    @Test func eventsCrossFromRustIntoAnAsyncStream() async throws {
        let client = try client()
        client.debugEmitThreadsChanged(mailboxID: "inbox", threadIDs: ["a", "b", "c"])
        let events = await collect(from: client)
        #expect(events == [.threadsChanged(mailboxID: "inbox", hint: ThreadChangeHint(inserted: ["a", "b", "c"]))])
    }

    @Test func aBurstBecomesOneInvalidation() async throws {
        let client = try client()
        client.debugEmitThreadsChanged(mailboxID: "inbox", threadIDs: (0..<500).map { "t\($0)" })
        let events = await collect(from: client)
        #expect(events == [.threadsChanged(mailboxID: "inbox", hint: ThreadChangeHint(invalidate: true))])
    }
}

@MainActor
struct AccountEventFilterTests {
    @Test func theWindowOnlyTakesEventsForItsAccountOrAppWideOnes() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        #expect(model.openAccountID == "demo")
        let event = CoreClientEvent.routinesChanged
        #expect(model.isForWindow(.init(accountID: "demo", event: event)))
        #expect(model.isForWindow(.init(accountID: nil, event: event)), "app-wide")
        #expect(!model.isForWindow(.init(accountID: "other", event: event)))
    }

    @Test func eventsFromTheCoreCarryTheAccountTheyAreAbout() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let client = try CoreClient(dataDirectory: dir)
        try await client.setCurrentAccount("demo")
        _ = try await client.seedDemoMailbox(threads: 60)
        let received: [CoreClientEvent.Tagged] = await withTaskGroup(of: [CoreClientEvent.Tagged].self) { group in
            group.addTask {
                var out: [CoreClientEvent.Tagged] = []
                for await e in client.events { out.append(e) }
                return out
            }
            try? await Task.sleep(for: .milliseconds(100))
            let row = try? await client.threads(in: "INBOX", limit: 1).rows.first
            if let row { try? await client.archive([row.id]) }
            try? await Task.sleep(for: .milliseconds(300))
            group.cancelAll()
            return await group.next() ?? []
        }
        let changes = received.filter { if case .threadsChanged = $0.event { true } else { false } }
        #expect(!changes.isEmpty)
        #expect(changes.allSatisfy { $0.accountID == "demo" })
    }
}
