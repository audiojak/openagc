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
