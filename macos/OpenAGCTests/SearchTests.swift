import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct SearchTests {
    private func demo() async throws -> AppModel {
        let model = AppModel(core: try CoreClient(dataDirectory: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)))
        await model.start(openDemo: true)
        return model
    }

    private func settle() async throws {
        try await Task.sleep(for: .milliseconds(250))
    }

    @Test func searchReplacesTheListAndClearingRestoresTheMailbox() async throws {
        let model = try await demo()
        let inbox = model.threads.rows.map(\.id)
        model.searchText = "from:billing"
        try await settle()
        #expect(model.threads.searchQuery == "from:billing")
        #expect(!model.threads.rows.isEmpty)
        #expect(model.threads.rows.allSatisfy { $0.participants.contains { $0.email.hasPrefix("billing@") } })
        model.searchText = ""
        try await settle()
        #expect(model.threads.searchQuery == nil)
        #expect(model.threads.rows.map(\.id) == inbox)
    }

    @Test func anInvalidQueryExplainsItselfAndKeepsTheList() async throws {
        let model = try await demo()
        let before = model.threads.rows.map(\.id)
        model.searchText = "in:nowhere"
        try await settle()
        #expect(model.threads.searchError?.contains("unknown mailbox") == true)
        #expect(model.threads.rows.map(\.id) == before)
    }

    @Test func onlyTheLastOfRapidKeystrokesIsShown() async throws {
        let model = try await demo()
        for partial in ["b", "bi", "bil", "bill", "billi", "billing"] {
            model.searchText = partial
            try await Task.sleep(for: .milliseconds(5))
        }
        try await settle()
        #expect(model.threads.searchQuery == "billing")
        let expected = try await model.core!.search("billing").rows.map(\.id)
        #expect(model.threads.rows.prefix(expected.count).map(\.id) == expected)
    }

    @Test func switchingMailboxesClearsTheSearch() async throws {
        let model = try await demo()
        model.searchText = "invoice"
        try await settle()
        model.selectedMailboxID = "@archive"
        try await settle()
        #expect(model.searchText.isEmpty)
        #expect(model.threads.searchQuery == nil)
        #expect(model.threads.mailboxID == "@archive")
    }
}
