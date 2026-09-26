import SwiftUI

/// Mailboxes and labels with unread badges (spec §14.3).
struct SidebarView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow
    @State private var expansion = LabelExpansion()

    var body: some View {
        @Bindable var model = model
        // A plain footer rather than a safe-area inset: on macOS 26 the
        // inset does not push the list's last section up (it drew over it).
        VStack(spacing: 0) {
        AccountMenuButton()
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
        List(selection: $model.selectedMailboxID) {
            Section {
                ForEach(model.mailboxes.systemMailboxes, id: \.id) { mailbox in
                    MailboxRow(mailbox: mailbox)
                }
            }
            if !model.mailboxes.labels.isEmpty {
                Section("Labels") {
                    ForEach(LabelTree.build(model.mailboxes.labels)) { node in
                        LabelTreeRow(node: node, expansion: expansion)
                    }
                }
            }
            // Routines open their own window rather than a mailbox (spec §11.5).
            Section("Routines") {
                ForEach(model.routines.routines, id: \.id) { routine in
                    Button {
                        model.routines.selectedID = routine.id
                        openWindow(id: "routines")
                    } label: {
                        VStack(alignment: .leading, spacing: 1) {
                            Label(routine.name, systemImage: routine.enabled ? "clock.arrow.2.circlepath" : "pause.circle")
                            Text(RoutinesStore.activity(model.routines.latestRuns[routine.id]))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .padding(.leading, 26)
                        }
                    }
                    .buttonStyle(.plain)
                }
                Button {
                    openWindow(id: "routines")
                } label: {
                    Label(model.routines.routines.isEmpty ? "Set Up a Routine…" : "Manage Routines…", systemImage: "plus.circle")
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
            }
        }
        .task { await model.routines.load() }
        .task(id: model.openAccountID) { expansion.load(account: model.openAccountID) }
        .onChange(of: model.routinesRevision) { Task { await model.routines.load() } }
        .listStyle(.sidebar)
        Divider()
        SyncStatusView()
        }
    }
}

/// Which label paths are expanded, remembered per account.
@MainActor
@Observable
final class LabelExpansion {
    private(set) var expanded: Set<String> = []
    private var key: String?
    @ObservationIgnored private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    func load(account: String?) {
        key = account.map { "sidebar.expandedLabels.\($0)" }
        expanded = Set(key.flatMap { defaults.stringArray(forKey: $0) } ?? [])
    }

    func binding(_ path: String) -> Binding<Bool> {
        Binding(get: { self.expanded.contains(path) }, set: { self.set(path, $0) })
    }

    func set(_ path: String, _ open: Bool) {
        if open { expanded.insert(path) } else { expanded.remove(path) }
        if let key { defaults.set(expanded.sorted(), forKey: key) }
    }
}

/// One label (or prefix-only group) and, when expanded, its children.
private struct LabelTreeRow: View {
    @Environment(AppModel.self) private var model
    let node: LabelNode
    let expansion: LabelExpansion

    var body: some View {
        if node.children.isEmpty {
            row
        } else {
            DisclosureGroup(isExpanded: expansion.binding(node.path)) {
                ForEach(node.children) { child in
                    LabelTreeRow(node: child, expansion: expansion)
                }
            } label: {
                row
            }
        }
    }

    @ViewBuilder private var row: some View {
        let collapsed = !node.children.isEmpty && !expansion.expanded.contains(node.path)
        if let mailbox = node.mailbox {
            MailboxRow(mailbox: mailbox, title: node.name,
                       tint: mailbox.labelId.flatMap { model.mailboxes.labelColors[$0] }.flatMap(Color.init(hex:)),
                       unreadOverride: collapsed ? node.totalUnread : nil)
                .dropDestination(for: String.self) { items, _ in
                    let ids = ThreadDrag.threadIDs(in: items)
                    guard let labelID = mailbox.labelId, !ids.isEmpty else { return false }
                    model.addLabel(labelID, toThreads: ids)
                    return true
                }
                .accessibilityHint(node.depth > 0 ? "Inside \(node.path.split(separator: "/").dropLast().joined(separator: ", "))" : "")
        } else {
            Label(node.name, systemImage: "folder")
                .foregroundStyle(.secondary)
                .badge(collapsed ? node.totalUnread : 0)
                .selectionDisabled()
                .accessibilityHint("Group of labels")
        }
    }
}

private struct MailboxRow: View {
    let mailbox: MailboxInfo
    var title: String?
    var tint: Color?
    /// Shown instead of the mailbox's own unread count (a collapsed parent).
    var unreadOverride: Int?

    var body: some View {
        Label {
            Text(title ?? mailbox.name)
        } icon: {
            // Gmail's label colors are chosen to read on light and dark.
            Image(systemName: tint == nil ? mailbox.kind.symbolName : "tag.fill")
                .foregroundStyle(tint ?? .secondary)
        }
            .badge(badgeCount)
            .tag(mailbox.id)
    }

    /// Unread for most mailboxes; Drafts shows its total like Mail does.
    private var badgeCount: Int {
        if let unreadOverride { return unreadOverride }
        return switch mailbox.kind {
        case .drafts: Int(mailbox.totalCount)
        case .sent, .archive, .trash, .spam: 0
        default: Int(mailbox.unreadCount)
        }
    }
}

extension Color {
    /// `#rrggbb` → Color.
    init?(hex: String) {
        let h = hex.trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "#"))
        guard h.count == 6, let v = UInt32(h, radix: 16) else { return nil }
        self.init(red: Double((v >> 16) & 0xff) / 255, green: Double((v >> 8) & 0xff) / 255, blue: Double(v & 0xff) / 255)
    }
}

extension MailboxKind {
    var symbolName: String {
        switch self {
        case .inbox: "tray"
        case .starred: "star"
        case .important: "exclamationmark.circle"
        case .sent: "paperplane"
        case .drafts: "doc"
        case .archive: "archivebox"
        case .spam: "xmark.bin"
        case .trash: "trash"
        case .label: "tag"
        }
    }
}
