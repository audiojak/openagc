import SwiftUI

/// Mailboxes and labels with unread badges (spec §14.3).
struct SidebarView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        @Bindable var model = model
        List(selection: $model.selectedMailboxID) {
            Section {
                ForEach(model.mailboxes.systemMailboxes, id: \.id) { mailbox in
                    MailboxRow(mailbox: mailbox)
                }
            }
            if !model.mailboxes.labels.isEmpty {
                Section("Labels") {
                    ForEach(model.mailboxes.labels, id: \.id) { mailbox in
                        MailboxRow(mailbox: mailbox)
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
        .onChange(of: model.routinesRevision) { Task { await model.routines.load() } }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) { SyncStatusView() }
    }
}

private struct MailboxRow: View {
    let mailbox: MailboxInfo

    var body: some View {
        Label(mailbox.name, systemImage: mailbox.kind.symbolName)
            .badge(badgeCount)
            .tag(mailbox.id)
    }

    /// Unread for most mailboxes; Drafts shows its total like Mail does.
    private var badgeCount: Int {
        switch mailbox.kind {
        case .drafts: Int(mailbox.totalCount)
        case .sent, .archive, .trash, .spam: 0
        default: Int(mailbox.unreadCount)
        }
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
