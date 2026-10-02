import Foundation
import Testing
@testable import OpenAGC

struct AccountAvatarTests {
    @Test func initialsComeFromTheNameOrTheAddress() {
        #expect(AccountAvatar.initials(name: "Ada Lovelace", email: "ada@example.com") == "AL")
        #expect(AccountAvatar.initials(name: "Ada Augusta King Lovelace", email: "x@example.com") == "AL")
        #expect(AccountAvatar.initials(name: "ada", email: "x@example.com") == "A")
        #expect(AccountAvatar.initials(name: nil, email: "john@actual.ai") == "J")
        #expect(AccountAvatar.initials(name: "  ", email: "zed@example.com") == "Z")
        #expect(AccountAvatar.initials(name: "123 !!", email: "q@example.com") == "Q", "no letters: use the address")
        #expect(AccountAvatar.initials(name: nil, email: "") == "?")
    }

    @Test func eachAddressKeepsItsColour() {
        let a = AccountAvatar.paletteIndex(for: "work@example.com")
        #expect(a == AccountAvatar.paletteIndex(for: "WORK@example.com"), "case-insensitive")
        let spread = Set(["a@x.com", "b@x.com", "c@x.com", "d@x.com", "e@x.com", "f@x.com"].map(AccountAvatar.paletteIndex(for:)))
        #expect(spread.count > 1, "different addresses usually differ")
    }
}

@MainActor
struct AccountMenuTests {
    private func summary(_ id: String, email: String, name: String? = nil, unread: UInt32 = 0,
                         kind: AccountKind = .gmail) -> AccountSummary {
        AccountSummary(id: id, kind: kind, email: email, displayName: name, avatarPath: nil, position: 0, inboxUnread: unread,
                       imapEnabled: false)
    }

    @Test func menuTitlesShowNameAddressAndUnread() {
        #expect(AccountMenuItems.title(summary("a", email: "a@example.com")) == "a@example.com")
        #expect(AccountMenuItems.title(summary("a", email: "a@example.com", name: "Ada", unread: 12)) == "Ada — a@example.com (12)")
    }

    @Test func menuAvatarsRenderForPicturelessAndArchiveAccounts() {
        for account in [summary("a", email: "a@example.com"), summary("b", email: "Old mail", kind: .archive)] {
            let image = AccountAvatar.menuImage(account, current: account.id == "a")
            #expect(image.size.width >= 18 && image.size.height >= 18)
        }
    }

    @Test func theAvatarMenuListsEveryAccountFromTheModel() async throws {
        let dir = CoreClient.testScratch()
        let core = try CoreClient(dataDirectory: dir)
        try await core.addDemoAccount("one", email: "one@example.com", name: "One", threads: 10)
        try await core.addDemoAccount("two", email: "two@example.com", threads: 10)
        try await core.moveAccount("two", to: 0)
        let model = AppModel(core: core, defaults: UserDefaults(suiteName: "openagc-tests-\(UUID().uuidString)")!)
        await model.start(openDemo: false)
        #expect(model.accounts.map(\.id) == ["two", "one"], "the user's order, which ⌃1 and ⌃2 follow")
        #expect(model.openAccountID == "two")
        await model.switchAccount(position: 1)
        #expect(model.openAccountID == "one")
    }
}

@MainActor
struct ToolbarAvatarTests {
    @Test func theToolbarAvatarKeepsItsColours() {
        let account = AccountSummary(id: "a", kind: .gmail, email: "a@example.com", displayName: "Ada", avatarPath: nil,
                                     position: 0, inboxUnread: 0, imapEnabled: false)
        let image = AccountAvatar.toolbarImage(account)
        #expect(!image.isTemplate, "a template image draws as a blank shape in the toolbar")
        #expect(image.size.width >= 20)
    }
}

struct AccountNameTests {
    @Test func theEditableNameIsTheProfileNameOrTheMailboxName() {
        let gmail = AccountSummary(id: "a", kind: .gmail, email: "me@x.com", displayName: "Me", avatarPath: nil,
                                   position: 0, inboxUnread: 0, imapEnabled: false)
        #expect(AccountRow.editableName(gmail) == "Me")
        let unnamed = AccountSummary(id: "b", kind: .gmail, email: "me@x.com", displayName: nil, avatarPath: nil,
                                     position: 0, inboxUnread: 0, imapEnabled: false)
        #expect(AccountRow.editableName(unnamed) == "")
        let archive = AccountSummary(id: "c", kind: .archive, email: "2019 archive", displayName: nil, avatarPath: nil,
                                     position: 0, inboxUnread: 0, imapEnabled: false)
        #expect(AccountRow.editableName(archive) == "2019 archive")
    }
}
