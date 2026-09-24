import SwiftUI

/// Sidebar footer: what sync is doing, in one line.
struct SyncStatusView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 6) {
            switch model.syncDisplay {
            case let .syncing(pending):
                ProgressView().controlSize(.mini)
                Text(pending > 0 ? "Syncing — \(pending.formatted()) left" : "Syncing…")
            case .offline:
                Image(systemName: "wifi.slash")
                Text("Offline")
            case .error:
                Image(systemName: "exclamationmark.triangle")
                Text("Sync paused")
            case .idle:
                if let email = model.accountEmail, case .open = model.accountState, model.core?.currentAccountID != AppModel.demoAccountID {
                    Text(email).lineLimit(1).truncationMode(.middle)
                } else if case .open = model.accountState {
                    Text("Demo mailbox")
                }
            }
            Spacer(minLength: 0)
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}
