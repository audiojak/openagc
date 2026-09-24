import SwiftUI

/// The three-column main window: mailboxes, threads, message (spec §14.3).
struct MainWindow: View {
    @Environment(AppModel.self) private var model
    @State private var columnVisibility = NavigationSplitViewVisibility.all

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 180, ideal: 220)
        } content: {
            content
                .navigationSplitViewColumnWidth(min: 300, ideal: 380)
        } detail: {
            detail
        }
        .task { if model.accountState == .starting { await model.start() } }
    }

    @ViewBuilder private var content: some View {
        switch model.accountState {
        case .starting:
            ProgressView().controlSize(.small)
        case .noAccount:
            ContentUnavailableView {
                Label("No Account", systemImage: "envelope")
            } description: {
                Text("Connect a Gmail account to get started.")
            } actions: {
                Button("Explore a Demo Mailbox") { Task { await model.openDemoMailbox() } }
            }
        case let .failed(message):
            ContentUnavailableView("Something Went Wrong", systemImage: "exclamationmark.triangle", description: Text(message))
        case .open:
            if model.threads.rows.isEmpty {
                ContentUnavailableView("No Conversations", systemImage: "tray")
            } else {
                ThreadListView()
            }
        }
    }

    @ViewBuilder private var detail: some View {
        if let id = model.selectedThreadID {
            ThreadDetailView(threadID: id)
        } else {
            ContentUnavailableView("No Message Selected", systemImage: "envelope.open")
        }
    }
}
