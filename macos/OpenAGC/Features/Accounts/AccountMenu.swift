import AppKit
import SwiftUI

/// The account switcher: a small avatar in the sidebar's title bar, with
/// the account list in its menu (spec §7.7). The address is in the tooltip,
/// not beside the picture.
struct AccountMenuButton: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Menu {
            AccountMenuItems(openSettings: { openSettings() })
        } label: {
            // "Accounts" names the toolbar item, so ToolbarToolTips finds it.
            Label {
                Text("Accounts")
            } icon: {
                // Toolbar menus draw their label as a template (one colour),
                // which turns a photo into a blank shape: a full-colour image.
                if let current {
                    Image(nsImage: AccountAvatar.toolbarImage(current))
                        .renderingMode(.original)
                } else {
                    Image(systemName: "person.crop.circle")
                }
            }
            .labelStyle(.iconOnly)
        }
        .menuIndicator(.hidden)
        .fixedSize()
        .help(Self.helpText(model))
        .accessibilityLabel("Account: \(current?.email ?? "none"). Switch account")
        .task { await model.reloadAccounts() }
    }

    private var current: AccountSummary? {
        model.accounts.first { $0.id == model.openAccountID }
    }

    static func helpText(_ model: AppModel) -> String {
        if let current = model.accounts.first(where: { $0.id == model.openAccountID }) {
            return "\(current.displayName ?? current.email) — \(current.email)\nSwitch account"
        }
        return model.openAccountID == AppModel.demoAccountID ? "Demo mailbox" : "Accounts"
    }
}

/// The account list, shared by the avatar menu and the app menu's
/// Accounts submenu: ⌃1–⌃9 switch by position.
struct AccountMenuItems: View {
    @Environment(AppModel.self) private var model
    let openSettings: () -> Void

    var body: some View {
        ForEach(Array(model.accounts.enumerated()), id: \.element.id) { index, account in
            Button { // no-help: menu item
                Task { await model.switchAccount(to: account.id) }
            } label: {
                Image(nsImage: AccountAvatar.menuImage(account, current: account.id == model.openAccountID))
                Text(AccountMenuItems.title(account))
            }
            .keyboardShortcut(index < 9 ? KeyboardShortcut(KeyEquivalent(Character("\(index + 1)")), modifiers: .control) : nil)
        }
        if !model.accounts.isEmpty { Divider() } // menu
        Button("Add Account…") { Task { await model.addAccount() } } // no-help: menu item
            .disabled(!GoogleClientConfiguration.effective().isUsable)
        // An imported mailbox is an account of its own (spec §7.8); no
        // shortcut: ⌘⇧I is Load Remote Images.
        Button("Create an Account from an Archived Mailbox…") { Task { await model.beginImport() } } // no-help: menu item
            .disabled(model.runningImport != nil)
        Button("Accounts Settings…", action: openSettings) // no-help: menu item
    }

    /// "Work Me — work@example.com (12)".
    static func title(_ account: AccountSummary) -> String {
        var title = account.displayName.map { "\($0) — \(account.email)" } ?? account.email
        if account.inboxUnread > 0 { title += " (\(account.inboxUnread))" }
        return title
    }
}

extension AccountAvatar {
    /// The avatar at toolbar size, as a non-template image.
    @MainActor
    static func toolbarImage(_ account: AccountSummary) -> NSImage {
        let renderer = ImageRenderer(content: AccountAvatar(account: account, size: 20))
        renderer.scale = NSScreen.main?.backingScaleFactor ?? 2
        let image = renderer.nsImage ?? NSImage()
        image.isTemplate = false
        return image
    }

    /// The avatar as a menu image, with a check ring on the current account.
    @MainActor
    static func menuImage(_ account: AccountSummary, current: Bool) -> NSImage {
        let view = AccountAvatar(account: account, size: 18)
            .overlay(Circle().strokeBorder(Color.accentColor, lineWidth: current ? 2 : 0))
            .padding(Space.hair)
        let renderer = ImageRenderer(content: view)
        renderer.scale = NSScreen.main?.backingScaleFactor ?? 2
        let image = renderer.nsImage ?? NSImage()
        image.isTemplate = false
        return image
    }
}
