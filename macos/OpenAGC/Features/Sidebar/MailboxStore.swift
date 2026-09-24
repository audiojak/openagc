import Foundation
import Observation

/// Sidebar data: system mailboxes then labels, with unread counts.
@MainActor
@Observable
final class MailboxStore {
    private(set) var mailboxes: [MailboxInfo] = []
    private let core: CoreClient?

    init(core: CoreClient?) {
        self.core = core
    }

    var systemMailboxes: [MailboxInfo] { mailboxes.filter { $0.kind != .label && $0.kind != .important } }
    var labels: [MailboxInfo] { mailboxes.filter { $0.kind == .label } }

    func reload() async {
        guard let core, let fresh = try? await core.mailboxes() else { return }
        if fresh != mailboxes { mailboxes = fresh }
    }
}
