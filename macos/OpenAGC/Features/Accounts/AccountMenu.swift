import AppKit
import SwiftUI

/// The avatar button at the top of the sidebar: the account on screen, and
/// a menu to switch, add or manage accounts (spec §7.7).
struct AccountMenuButton: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Menu {
            AccountMenuItems(openSettings: { openSettings() })
        } label: {
            HStack(spacing: 8) {
                if let current {
                    AccountAvatar(account: current, size: 22)
                    VStack(alignment: .leading, spacing: 0) {
                        Text(current.displayName ?? current.email).font(.callout.weight(.medium)).lineLimit(1)
                        if current.displayName != nil {
                            Text(current.email).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        }
                    }
                } else {
                    Image(systemName: "person.crop.circle").font(.system(size: 20)).foregroundStyle(.secondary)
                    Text(model.openAccountID == AppModel.demoAccountID ? "Demo mailbox" : "Accounts")
                        .font(.callout.weight(.medium))
                }
            }
            .contentShape(Rectangle())
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.visible)
        .fixedSize(horizontal: false, vertical: true)
        .accessibilityLabel("Account: \(current?.email ?? "none"). Switch account")
        .task { await model.reloadAccounts() }
    }

    private var current: AccountSummary? {
        model.accounts.first { $0.id == model.openAccountID }
    }
}

/// The account list, shared by the avatar menu and the app menu's
/// Accounts submenu: ⌃1–⌃9 switch by position.
struct AccountMenuItems: View {
    @Environment(AppModel.self) private var model
    let openSettings: () -> Void

    var body: some View {
        ForEach(Array(model.accounts.enumerated()), id: \.element.id) { index, account in
            Button {
                Task { await model.switchAccount(to: account.id) }
            } label: {
                Image(nsImage: AccountAvatar.menuImage(account, current: account.id == model.openAccountID))
                Text(AccountMenuItems.title(account))
            }
            .keyboardShortcut(index < 9 ? KeyboardShortcut(KeyEquivalent(Character("\(index + 1)")), modifiers: .control) : nil)
        }
        if !model.accounts.isEmpty { Divider() }
        Button("Add Account…") { Task { await model.addAccount() } }
            .disabled(!GoogleClientConfiguration.effective().isUsable)
        Button("Accounts Settings…", action: openSettings)
    }

    /// "Work Me — work@example.com (12)".
    static func title(_ account: AccountSummary) -> String {
        var title = account.displayName.map { "\($0) — \(account.email)" } ?? account.email
        if account.inboxUnread > 0 { title += " (\(account.inboxUnread))" }
        return title
    }
}

extension AccountAvatar {
    /// The avatar as a menu image, with a check ring on the current account.
    @MainActor
    static func menuImage(_ account: AccountSummary, current: Bool) -> NSImage {
        let view = AccountAvatar(account: account, size: 18)
            .overlay(Circle().strokeBorder(Color.accentColor, lineWidth: current ? 2 : 0))
            .padding(1)
        let renderer = ImageRenderer(content: view)
        renderer.scale = NSScreen.main?.backingScaleFactor ?? 2
        return renderer.nsImage ?? NSImage()
    }
}
