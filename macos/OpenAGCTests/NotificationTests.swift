import AppKit
import Foundation
import Testing
import UserNotifications
@testable import OpenAGC

@MainActor
struct NotificationTests {
    private func mail(_ n: Int, thread: String = "t") -> [CoreClientEvent.NewMail] {
        (1...n).map {
            CoreClientEvent.NewMail(messageID: "m\($0)", threadID: "\(thread)\($0)", senderName: "Sender \($0)",
                                    subject: "Subject \($0)", snippet: "Snippet \($0)")
        }
    }

    private func notifier(active: Bool = false) -> (NewMailNotifier, () -> [UNNotificationRequest], UserDefaults) {
        let defaults = UserDefaults(suiteName: "openagc-tests-\(UUID().uuidString)")!
        var posted: [UNNotificationRequest] = []
        let notifier = NewMailNotifier(defaults: defaults) { posted.append($0) }
        notifier.isAppActive = { active }
        return (notifier, { posted }, defaults)
    }

    @Test func eachNewMessageIsAnnouncedWithItsThread() {
        let (notifier, posted, _) = notifier()
        notifier.announce(mail(2))
        let requests = posted()
        #expect(requests.map(\.identifier) == ["m1", "m2"])
        #expect(requests[0].content.title == "Sender 1")
        #expect(requests[0].content.subtitle == "Subject 1")
        #expect(requests[0].content.body == "Snippet 1")
        #expect(requests[0].content.threadIdentifier == "t1")
        #expect(requests[0].content.userInfo["threadID"] as? String == "t1")
    }

    @Test func aBurstBecomesOneSummary() {
        let (notifier, posted, _) = notifier()
        notifier.announce(mail(6))
        let requests = posted()
        #expect(requests.count == 1)
        #expect(requests[0].content.title == "6 new messages")
        #expect(requests[0].content.body.hasSuffix("and others"))
    }

    @Test func nothingIsShownWhileFrontmostOrWhenTurnedOff() {
        let (active, activePosted, _) = notifier(active: true)
        active.announce(mail(1))
        #expect(activePosted().isEmpty)

        let (off, offPosted, defaults) = notifier()
        defaults.set(false, forKey: NewMailNotifier.notifyKey)
        off.announce(mail(1))
        #expect(offPosted().isEmpty)
    }

    @Test func clickingANotificationRevealsTheThread() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        let row = try #require(model.threads.rows.dropFirst(3).first)
        model.selectedMailboxID = "STARRED"
        try await Task.sleep(for: .milliseconds(100))
        model.reveal(threadID: row.id)
        #expect(model.selectedMailboxID == "INBOX")
        #expect(model.selectedThreadID == row.id)
    }

    @Test func withSeveralAccountsNotificationsNameTheirAccount() {
        let (notifier, posted, _) = notifier()
        notifier.announce(mail(1), account: .init(id: "work", label: "Work Me"))
        notifier.announce(mail(4), account: .init(id: "home", label: nil))
        let requests = posted()
        #expect(requests[0].identifier == "work:m1")
        #expect(requests[0].content.subtitle == "Work Me · Subject 1")
        #expect(requests[0].content.userInfo["accountID"] as? String == "work")
        #expect(requests[0].content.userInfo["threadID"] as? String == "t1")
        #expect(requests[1].content.title == "4 new messages")
        #expect(requests[1].content.subtitle.isEmpty, "one account: no label")
        #expect(requests[1].content.userInfo["accountID"] as? String == "home")
    }

    @Test func clickingANotificationForAnotherAccountSwitchesToIt() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let core = try CoreClient(dataDirectory: dir)
        try await core.addDemoAccount("work", email: "work@example.com", threads: 20)
        try await core.addDemoAccount("home", email: "home@example.com", threads: 20)
        let model = AppModel(core: core, defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.start(openDemo: false)
        #expect(model.openAccountID == "work")
        #expect(model.notificationTag(for: "home") == .init(id: "home", label: "home@example.com"))
        let homeThread = try #require(try await core.threads(in: "INBOX", limit: 50).rows.first).id
        await model.reveal(threadID: homeThread, in: "home")
        #expect(model.openAccountID == "home")
        #expect(model.selectedThreadID == homeThread)
    }

    @Test func removingTheShownAccountOpensTheNextThenOnboarding() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let core = try CoreClient(dataDirectory: dir)
        try await core.addDemoAccount("work", email: "work@example.com", threads: 10)
        try await core.addDemoAccount("home", email: "home@example.com", threads: 10)
        let model = AppModel(core: core, defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.start(openDemo: false)
        try await core.setSyncWindow(.year, for: "home")
        #expect(try await core.syncWindow(for: "home") == .year)
        #expect(try await core.syncWindow(for: "work") == .halfYear, "per account")
        await model.removeAccount("work")
        #expect(model.openAccountID == "home")
        #expect(model.accounts.map(\.id) == ["home"])
        await model.removeAccount("home")
        #expect(model.accountState == .noAccount)
        #expect(model.defaults.string(forKey: "accountID") == nil)
    }

    @Test func dockBadgeFollowsInboxUnreadAndTheSetting() {
        let (notifier, _, defaults) = notifier()
        notifier.updateBadge(inboxUnread: 12)
        #expect(NSApp.dockTile.badgeLabel == "12")
        notifier.updateBadge(inboxUnread: 0)
        #expect(NSApp.dockTile.badgeLabel == nil)
        defaults.set(false, forKey: NewMailNotifier.badgeKey)
        notifier.updateBadge(inboxUnread: 5)
        #expect(NSApp.dockTile.badgeLabel == nil)
    }
}
