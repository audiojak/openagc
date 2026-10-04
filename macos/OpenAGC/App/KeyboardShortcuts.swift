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
            .init(keys: "⇧⌘J", action: "Mark as junk (Not junk in Spam)", inThreadList: false),
            .init(keys: "⇧⌘U", action: "Mark as read or unread", inThreadList: false),
            .init(keys: "⇧⌘L", action: "Star or unstar", inThreadList: false),
            .init(keys: "⇧⌘I", action: "Load remote images", inThreadList: false),
            .init(keys: "⇧⌘N", action: "Check for new mail", inThreadList: false),
            .init(keys: "⌘Z  ⇧⌘Z", action: "Undo or redo the last mail action", inThreadList: false),
        ]),
        Group(title: "Finding mail", shortcuts: [
            .init(keys: "⌘F", action: "Search mail", inThreadList: false),
            .init(keys: "⌘1 – ⌘6", action: "Inbox, Starred, Sent, Drafts, Archive, Trash", inThreadList: false),
            .init(keys: "⌃1 – ⌃9", action: "Switch account", inThreadList: false),
            .init(keys: "⇥", action: "From the sidebar to the list of messages; in a message you write, to writing help",
                  inThreadList: false),
        ]),
        Group(title: "In the thread list", shortcuts: [
            .init(keys: "↑ ↓  or  j k", action: "Previous or next thread", inThreadList: true),
            .init(keys: "e", action: "Archive", inThreadList: true),
            .init(keys: "#  or  ⌫", action: "Move to Trash", inThreadList: true),
            .init(keys: "!", action: "Mark as junk or not junk", inThreadList: true),
            .init(keys: "↩", action: "Open in a new window (a draft opens to edit)", inThreadList: true),
            .init(keys: "u", action: "Mark as read or unread", inThreadList: true),
            .init(keys: "s", action: "Star or unstar", inThreadList: true),
            .init(keys: "l", action: "Label…", inThreadList: true),
            .init(keys: "r", action: "Reply", inThreadList: true),
            .init(keys: "a", action: "Reply all", inThreadList: true),
            .init(keys: "f", action: "Forward", inThreadList: true),
            .init(keys: "c", action: "New message", inThreadList: true),
            .init(keys: "/", action: "Search mail", inThreadList: true),
            .init(keys: "t", action: "New task from the email (Claude suggests it)", inThreadList: true),
            .init(keys: "⇧T", action: "Create tasks for the highlighted emails, or the latest 20", inThreadList: true),
        ]),
        Group(title: "Writing", shortcuts: [
            .init(keys: "⇧⌘D  or  ⌘↩", action: "Send", inThreadList: false),
            .init(keys: "⇧⌘A", action: "Attach files", inThreadList: false),
            .init(keys: "⌘B  ⌘I  ⌘U", action: "Bold, italic, underline", inThreadList: false),
        ]),
        Group(title: "Agents and routines", shortcuts: [
            .init(keys: "⌘K", action: "Ask the agent", inThreadList: false),
            .init(keys: "⌥⌘I", action: "Show or hide the agent panel", inThreadList: false),
            .init(keys: "⌥⌘R", action: "Routines", inThreadList: false),
            .init(keys: "↑ ↓  Tab  ↩  esc", action: "In the prompt: choose a suggestion, or hide them", inThreadList: false),
            .init(keys: "⌘S", action: "Save a routine (in Routines)", inThreadList: false),
        ]),
        Group(title: "Windows", shortcuts: [
            .init(keys: "⌘0", action: "The mail window (after closing it)", inThreadList: false),
        ]),
        Group(title: "Help", shortcuts: [
            .init(keys: "⇧⌘/", action: "Keyboard shortcuts", inThreadList: false),
        ]),
    ]
}

/// Help › Keyboard Shortcuts.
struct KeyboardShortcutsView: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: Space.xxl) {
                ForEach(KeyboardShortcutGuide.groups) { group in
                    VStack(alignment: .leading, spacing: Space.s) {
                        Text(group.title).font(.headline)
                        Grid(alignment: .leading, horizontalSpacing: Space.xl, verticalSpacing: Space.xs) {
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
            .padding(Space.xxl)
        }
        .frame(minWidth: 460, minHeight: 520)
    }
}
