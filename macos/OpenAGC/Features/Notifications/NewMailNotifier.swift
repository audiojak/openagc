import AppKit
import Foundation
import os
import UserNotifications

/// New-mail notifications and the Dock badge (spec §14.7). Notifications
/// are only for mail that arrives while the app is not frontmost; clicking
/// one opens its thread. Both are on by default and can be turned off in
/// Settings.
@MainActor
final class NewMailNotifier: NSObject {
    static let notifyKey = "notifyNewMail"
    static let badgeKey = "showDockBadge"
    /// More new messages than this at once become one summary notification.
    static let summaryThreshold = 3

    /// Where notifications go; replaced in tests.
    var post: (UNNotificationRequest) -> Void
    /// Whether the app is frontmost; replaced in tests.
    var isAppActive: () -> Bool = { NSApp.isActive }
    /// Opens a thread (in an account, if the notification named one) when
    /// a notification is clicked.
    var openThread: ((String, String?) -> Void)?

    private let defaults: UserDefaults
    private let logger = Logger(subsystem: "ai.actual.openagc", category: "notifications")
    private var askedForPermission = false

    init(defaults: UserDefaults = .standard, post: ((UNNotificationRequest) -> Void)? = nil) {
        self.defaults = defaults
        defaults.register(defaults: [Self.notifyKey: true, Self.badgeKey: true])
        self.post = post ?? { _ in }
        super.init()
        if post == nil {
            self.post = { [weak self] request in self?.deliver(request) }
        }
    }

    /// Call once at launch so clicks on earlier notifications route here.
    func install() {
        UNUserNotificationCenter.current().delegate = self
    }

    // MARK: Notifications

    /// Which account the mail is for: its id, and a label shown only when
    /// there is more than one account (spec §7.7).
    struct AccountTag: Equatable {
        let id: String
        let label: String?
    }

    func announce(_ mail: [CoreClientEvent.NewMail], account: AccountTag? = nil) {
        guard !mail.isEmpty, defaults.bool(forKey: Self.notifyKey), !isAppActive() else { return }
        for request in Self.requests(for: mail, account: account) {
            post(request)
        }
    }

    /// One notification per message, or one summary for a burst. Pure.
    static func requests(for mail: [CoreClientEvent.NewMail], account: AccountTag? = nil) -> [UNNotificationRequest] {
        requests(untagged: mail).map { request in
            guard let account else { return request }
            let content = (request.content.mutableCopy() as? UNMutableNotificationContent) ?? UNMutableNotificationContent()
            var info = content.userInfo
            info["accountID"] = account.id
            content.userInfo = info
            if let label = account.label {
                content.subtitle = content.subtitle.isEmpty ? label : "\(label) · \(content.subtitle)"
                content.threadIdentifier = "\(account.id):\(content.threadIdentifier)"
            }
            return UNNotificationRequest(identifier: "\(account.id):\(request.identifier)", content: content, trigger: nil)
        }
    }

    private static func requests(untagged mail: [CoreClientEvent.NewMail]) -> [UNNotificationRequest] {
        if mail.count > summaryThreshold {
            let content = UNMutableNotificationContent()
            content.title = "\(mail.count) new messages"
            let senders = Array(NSOrderedSet(array: mail.map(\.senderName))) as? [String] ?? []
            content.body = ListFormatter.localizedString(byJoining: Array(senders.prefix(4)))
                + (senders.count > 4 ? " and others" : "")
            content.sound = .default
            content.threadIdentifier = "new-mail"
            content.userInfo = ["threadID": mail.last?.threadID ?? ""]
            return [UNNotificationRequest(identifier: "summary-\(mail.last?.messageID ?? "")", content: content, trigger: nil)]
        }
        return mail.map { m in
            let content = UNMutableNotificationContent()
            content.title = m.senderName
            content.subtitle = m.subject.isEmpty ? "(no subject)" : m.subject
            content.body = m.snippet
            content.sound = .default
            // Groups notifications of one conversation in Notification Center.
            content.threadIdentifier = m.threadID
            content.userInfo = ["threadID": m.threadID]
            return UNNotificationRequest(identifier: m.messageID, content: content, trigger: nil)
        }
    }

    private func deliver(_ request: UNNotificationRequest) {
        let center = UNUserNotificationCenter.current()
        Task {
            if !askedForPermission {
                askedForPermission = true
                // Asked the first time there is something to show, not at launch.
                _ = try? await center.requestAuthorization(options: [.alert, .sound, .badge])
            }
            do {
                try await center.add(request)
            } catch {
                logger.error("notification failed: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: Dock badge

    func updateBadge(inboxUnread: UInt32) {
        let show = defaults.bool(forKey: Self.badgeKey) && inboxUnread > 0
        let label = show ? (inboxUnread > 9_999 ? "9999+" : String(inboxUnread)) : nil
        if NSApp.dockTile.badgeLabel != label {
            NSApp.dockTile.badgeLabel = label
        }
    }
}

extension NewMailNotifier: UNUserNotificationCenterDelegate {
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        let threadID = info["threadID"] as? String
        let accountID = info["accountID"] as? String
        await MainActor.run {
            NSApp.activate()
            if let threadID, !threadID.isEmpty { openThread?(threadID, accountID) }
        }
    }

    /// Mail arriving while the app is frontmost is already visible.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter,
                                            willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        []
    }
}
