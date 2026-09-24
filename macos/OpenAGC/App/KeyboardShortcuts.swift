import SwiftUI

/// Every keyboard shortcut, in one place (spec §14.3): shown in Help ›
/// Keyboard Shortcuts and checked for clashes by a test. The menus define
/// the working bindings; keep this list in step with them.
enum KeyboardShortcutGuide {
    struct Shortcut: Hashable {
        let keys: String
        let action: String
        /// Menu shortcuts share one namespace; single keys work in the list.
        let inThreadList: Bool
    }

    struct Group: Identifiable {
        let title: String
        let shortcuts: [Shortcut]
        var id: String { title }
    }

    static let groups: [Group] = [
        Group(title: "Mail", shortcuts: [
            .init(keys: "⌘N", action: "New message", inThreadList: false),
            .init(keys: "⌘R", action: "Reply", inThreadList: false),
            .init(keys: "⇧⌘R", action: "Reply all", inThreadList: false),
            .init(keys: "⇧⌘F", action: "Forward", inThreadList: false),
            .init(keys: "⌃⌘A", action: "Archive", inThreadList: false),
            .init(keys: "⌃⌘I", action: "Move to Inbox", inThreadList: false),
            .init(keys: "⌘⌫", action: "Move to Trash", inThreadList: false),
            .init(keys: "⇧⌘U", action: "Mark as read or unread", inThreadList: false),
            .init(keys: "⇧⌘L", action: "Star or unstar", inThreadList: false),
            .init(keys: "⇧⌘I", action: "Load remote images", inThreadList: false),
            .init(keys: "⇧⌘N", action: "Check for new mail", inThreadList: false),
        ]),
        Group(title: "Finding mail", shortcuts: [
            .init(keys: "⌘F", action: "Search mail", inThreadList: false),
            .init(keys: "⌘1 – ⌘6", action: "Inbox, Starred, Sent, Drafts, Archive, Trash", inThreadList: false),
        ]),
        Group(title: "In the thread list", shortcuts: [
            .init(keys: "↑ ↓  or  j k", action: "Previous or next thread", inThreadList: true),
            .init(keys: "e", action: "Archive", inThreadList: true),
            .init(keys: "#  or  ⌫", action: "Move to Trash", inThreadList: true),
            .init(keys: "u", action: "Mark as read or unread", inThreadList: true),
            .init(keys: "s", action: "Star or unstar", inThreadList: true),
            .init(keys: "l", action: "Label…", inThreadList: true),
            .init(keys: "r", action: "Reply", inThreadList: true),
            .init(keys: "a", action: "Reply all", inThreadList: true),
            .init(keys: "f", action: "Forward", inThreadList: true),
            .init(keys: "c", action: "New message", inThreadList: true),
            .init(keys: "/", action: "Search mail", inThreadList: true),
        ]),
        Group(title: "Writing", shortcuts: [
            .init(keys: "⇧⌘D", action: "Send", inThreadList: false),
            .init(keys: "⇧⌘A", action: "Attach files", inThreadList: false),
            .init(keys: "⌘B  ⌘I  ⌘U", action: "Bold, italic, underline", inThreadList: false),
        ]),
        Group(title: "Agents and routines", shortcuts: [
            .init(keys: "⌘K", action: "Ask the agent", inThreadList: false),
            .init(keys: "⌥⌘I", action: "Show or hide the agent panel", inThreadList: false),
            .init(keys: "⌥⌘R", action: "Routines", inThreadList: false),
        ]),
    ]
}

/// Help › Keyboard Shortcuts.
struct KeyboardShortcutsView: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                ForEach(KeyboardShortcutGuide.groups) { group in
                    VStack(alignment: .leading, spacing: 6) {
                        Text(group.title).font(.headline)
                        Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 4) {
                            ForEach(group.shortcuts, id: \.self) { s in
                                GridRow {
                                    Text(s.keys).font(.body.monospaced()).gridColumnAlignment(.trailing)
                                    Text(s.action)
                                }
                            }
                        }
                    }
                }
                Text("With Full Keyboard Access on (System Settings › Keyboard), Tab reaches every button, including approvals in the agent panel.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            .padding(20)
        }
        .frame(minWidth: 460, minHeight: 520)
    }
}
