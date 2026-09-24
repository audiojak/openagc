import Foundation
import Observation

/// The loaded window of a mailbox's thread list (spec §13 rule 3): pages of
/// rows fetched with a keyset cursor, extended as the user scrolls.
@MainActor
@Observable
final class ThreadListStore {
    static let pageSize: UInt32 = 150
    /// Load the next page when the user is this close to the end.
    static let prefetchDistance = 60

    private(set) var rows: [ThreadRow] = []
    private(set) var mailboxID: String?
    /// Bumped whenever `rows` is replaced wholesale (new mailbox, refresh),
    /// as opposed to extended; the table view reloads instead of inserting.
    private(set) var generation = 0

    @ObservationIgnored private var nextCursor: String?
    @ObservationIgnored private var isLoadingMore = false
    @ObservationIgnored private let core: CoreClient?

    init(core: CoreClient?) {
        self.core = core
    }

    var hasMore: Bool { nextCursor != nil }

    func show(mailboxID: String) async {
        self.mailboxID = mailboxID
        nextCursor = nil
        await load(replacing: true, limit: Self.pageSize)
    }

    /// Called as rows become visible; fetches the next page near the end.
    func rowWillAppear(at index: Int) {
        guard hasMore, !isLoadingMore, index >= rows.count - Self.prefetchDistance else { return }
        isLoadingMore = true
        Task {
            await load(replacing: false, limit: Self.pageSize)
            isLoadingMore = false
        }
    }

    /// React to a coalesced change for this mailbox. Re-querying the loaded
    /// window is a single indexed read of a few hundred rows, so it is
    /// cheaper than it sounds and always correct.
    func apply(_ hint: ThreadChangeHint) async {
        await refresh()
    }

    /// Remove rows now, ahead of the core's change event (spec §13 rule 7).
    func optimisticallyRemove(_ ids: Set<String>) {
        let kept = rows.filter { !ids.contains($0.id) }
        guard kept.count != rows.count else { return }
        rows = kept
        generation += 1
    }

    /// Patch rows now, ahead of the core's change event.
    func optimisticallyUpdate(_ ids: Set<String>, _ transform: (inout ThreadRow) -> Void) {
        var changed = false
        var updated = rows
        for i in updated.indices where ids.contains(updated[i].id) {
            transform(&updated[i])
            changed = true
        }
        guard changed else { return }
        rows = updated
        generation += 1
    }

    func refresh() async {
        let count = UInt32(max(rows.count, Int(Self.pageSize)))
        nextCursor = nil
        await load(replacing: true, limit: min(count, 500))
    }

    private func load(replacing: Bool, limit: UInt32) async {
        guard let core, let mailboxID else { return }
        let cursor = replacing ? nil : nextCursor
        guard let page = try? await core.threads(in: mailboxID, after: cursor, limit: limit) else { return }
        // The user may have switched mailboxes while this was in flight.
        guard mailboxID == self.mailboxID else { return }
        if replacing {
            if page.rows != rows {
                rows = page.rows
                generation += 1
            }
        } else {
            rows.append(contentsOf: page.rows)
        }
        nextCursor = page.nextCursor
    }
}
