import AppKit
import Foundation
import Testing
@testable import OpenAGC

@MainActor
@Suite(.serialized)
struct AccountSwitchTests {
    struct Timeout: Error {}

    private func twoAccounts() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let core = try CoreClient(dataDirectory: dir)
        try await core.addDemoAccount("work", email: "work@example.com", name: "Work Me", threads: 40)
        try await core.addDemoAccount("home", email: "home@example.com", threads: 12)
        let model = AppModel(core: core, defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.start(openDemo: false)
        return model
    }

    @Test func theFirstAccountOpensAndAllAreListedWithUnreadCounts() async throws {
        let model = try await twoAccounts()
        #expect(model.openAccountID == "work")
        #expect(model.accounts.map(\.id) == ["work", "home"])
        #expect(model.accounts[0].displayName == "Work Me")
        #expect(model.accountEmail == "work@example.com")
        let inbox = model.mailboxes.mailboxes.first { $0.kind == .inbox }?.unreadCount ?? 0
        #expect(model.accounts[0].inboxUnread == inbox)
    }

    @Test func switchingRebindsTheWindowAndReturnsToWhereEachAccountWasLeft() async throws {
        let model = try await twoAccounts()
        let workRows = model.threads.rows.map(\.id)
        let picked = try #require(model.threads.rows.dropFirst().first).id
        model.selectedThreadID = picked
        let workAgent = model.agent

        await model.switchAccount(to: "home")
        #expect(model.openAccountID == "home")
        #expect(model.accountEmail == "home@example.com")
        #expect(model.selectedThreadID == nil)
        #expect(model.threads.rows.map(\.id) != workRows, "home's own threads")
        #expect(model.agent !== workAgent, "each account has its own agent panel")
        #expect(model.defaults.string(forKey: "accountID") == "home", "remembered for the next launch")

        await model.switchAccount(position: 0)
        #expect(model.openAccountID == "work")
        #expect(model.threads.rows.map(\.id) == workRows)
        #expect(model.selectedThreadID == picked, "back where work was left")
        #expect(model.agent === workAgent, "the work panel was kept")
    }

    @Test func aComposerStaysOnItsAccountAfterASwitch() async throws {
        let model = try await twoAccounts()
        let store = ComposerStore(core: model.core, account: "work",
                                  attachmentsDirectory: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString))
        await store.load(.new(to: "someone@example.com"))
        await model.switchAccount(to: "home")
        store.subject = "Written on work"
        store.body = NSAttributedString(string: "hello")
        await store.save()
        let core = try #require(model.core)
        let homeDrafts = try await core.drafts()
        #expect(!homeDrafts.contains { $0.subject == "Written on work" }, "not saved into home")
        await model.switchAccount(to: "work")
        #expect(try await core.drafts().contains { $0.subject == "Written on work" })
    }
}
