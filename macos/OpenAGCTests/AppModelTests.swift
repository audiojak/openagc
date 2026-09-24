import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct AppModelTests {
    private func demoModel() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    @Test func demoModeOpensAnAccountAndFillsTheSidebarAndList() async throws {
        let model = try await demoModel()
        #expect(model.accountState == .open(accountID: AppModel.demoAccountID))
        let inbox = try #require(model.mailboxes.systemMailboxes.first { $0.kind == .inbox })
        #expect(inbox.unreadCount > 0)
        #expect(model.mailboxes.systemMailboxes.map(\.kind) == [.inbox, .starred, .sent, .drafts, .archive, .spam, .trash])
        #expect(model.mailboxes.labels.map(\.name) == ["Customers", "Hiring", "Newsletters", "Receipts", "Travel"])
        #expect(model.threads.mailboxID == "INBOX")
        #expect(model.threads.rows.count == min(Int(ThreadListStore.pageSize), Int(inbox.totalCount)))
    }

    @Test func switchingMailboxesReloadsTheListAndClearsSelection() async throws {
        let model = try await demoModel()
        model.selectedThreadID = model.threads.rows.first?.id
        model.selectedMailboxID = "@archive"
        #expect(model.selectedThreadID == nil)
        try await waitUntil { model.threads.mailboxID == "@archive" && !model.threads.rows.isEmpty }
        let archive = try #require(model.mailboxes.mailboxes.first { $0.kind == .archive })
        #expect(model.threads.rows.count == min(Int(ThreadListStore.pageSize), Int(archive.totalCount)))
    }

    @Test func scrollingNearTheEndLoadsTheNextPage() async throws {
        let model = try await demoModel()
        model.selectedMailboxID = "@archive"
        try await waitUntil { model.threads.mailboxID == "@archive" && !model.threads.rows.isEmpty }
        let first = model.threads.rows.count
        #expect(model.threads.hasMore)
        model.threads.rowWillAppear(at: first - 1)
        try await waitUntil { model.threads.rows.count > first }
        #expect(Set(model.threads.rows.map(\.id)).count == model.threads.rows.count, "no duplicates after paging")
    }

    private func waitUntil(timeout: Duration = .seconds(5), _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else { throw WaitTimeout() }
            try await Task.sleep(for: .milliseconds(20))
        }
    }

    private struct WaitTimeout: Error {}
}
