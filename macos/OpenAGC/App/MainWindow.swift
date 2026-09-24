import SwiftUI

/// The three-column main window: mailboxes, threads, message (spec §14.3).
struct MainWindow: View {
    @State private var columnVisibility = NavigationSplitViewVisibility.all

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            List {
                Label("Inbox", systemImage: "tray")
                Label("Drafts", systemImage: "doc")
                Label("Sent", systemImage: "paperplane")
                Label("Archive", systemImage: "archivebox")
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 220)
        } content: {
            ContentUnavailableView("No Account", systemImage: "envelope",
                                   description: Text("Connect a Gmail account to get started."))
                .navigationSplitViewColumnWidth(min: 300, ideal: 380)
        } detail: {
            ContentUnavailableView("No Message Selected", systemImage: "envelope.open")
        }
    }
}
