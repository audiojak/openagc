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
                for await event in client.events { events.append(event) }
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
