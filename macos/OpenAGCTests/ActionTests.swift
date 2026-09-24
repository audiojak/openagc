import AppKit
import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct ActionTests {
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

    private func inbox(_ model: AppModel) async throws -> [String] {
        try await model.core!.threads(in: "INBOX", limit: 500).rows.map(\.id)
    }

    @Test func archivingRemovesTheRowAtOnceSelectsTheNextAndPersists() async throws {
        let model = try await demo()
        let rows = model.threads.rows
        model.selectedThreadID = rows[1].id
        model.archiveSelection()
        #expect(!model.threads.rows.contains { $0.id == rows[1].id }, "optimistic")
        #expect(model.selectedThreadID == rows[2].id, "next row selected")
        try await Task.sleep(for: .milliseconds(200))
        #expect(try await !inbox(model).contains(rows[1].id), "store updated")
    }

    @Test func multiSelectionArchivesEveryTarget() async throws {
        let model = try await demo()
        let ids = Set(model.threads.rows.prefix(3).map(\.id))
        model.selectedThreadIDs = ids
        model.archiveSelection()
        #expect(model.threads.rows.allSatisfy { !ids.contains($0.id) })
        try await Task.sleep(for: .milliseconds(200))
        let remaining = Set(try await inbox(model))
        #expect(remaining.isDisjoint(with: ids))
    }

    @Test func toggleReadFlipsUnreadThreads() async throws {
        let model = try await demo()
        let unread = try #require(model.threads.rows.first { $0.unreadCount > 0 })
        model.selectedThreadID = unread.id
        model.toggleReadSelection()
        #expect(model.threads.rows.first { $0.id == unread.id }?.unreadCount == 0)
        try await Task.sleep(for: .milliseconds(200))
        let detail = try #require(try await model.core!.thread(unread.id))
        #expect(detail.thread.unreadCount == 0)
        model.toggleReadSelection()
        try await Task.sleep(for: .milliseconds(200))
        #expect(try await model.core!.thread(unread.id)?.thread.unreadCount ?? 0 > 0)
    }

    @Test func starringAndLabeling() async throws {
        let model = try await demo()
        let row = model.threads.rows[0]
        model.selectedThreadID = row.id
        model.toggleStarSelection()
        let label = try #require(model.mailboxes.labels.first?.labelId)
        model.setLabel(label, applied: true)
        try await Task.sleep(for: .milliseconds(300))
        let detail = try #require(try await model.core!.thread(row.id))
        #expect(detail.thread.isStarred == !row.isStarred)
        #expect(detail.thread.labelIds.contains(label))
    }

    @Test func theEKeyArchivesFromTheTable() async throws {
        let model = try await demo()
        let row = model.threads.rows[0]
        model.selectedThreadID = row.id
        let table = ThreadTableView()
        table.model = model
        let event = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                                                  windowNumber: 0, context: nil, characters: "e",
                                                  charactersIgnoringModifiers: "e", isARepeat: false, keyCode: 14))
        table.keyDown(with: event)
        #expect(!model.threads.rows.contains { $0.id == row.id })
    }
}
