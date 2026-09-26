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
    /// Non-nil while showing search results instead of the mailbox.
    private(set) var searchQuery: String?
    /// Why the current query could not run (shown under the field).
    private(set) var searchError: String?
    /// Bumped whenever `rows` is replaced wholesale (new mailbox, refresh),
    /// as opposed to extended; the table view reloads instead of inserting.
    private(set) var generation = 0

    @ObservationIgnored private var nextCursor: String?
    @ObservationIgnored private var isLoadingMore = false
    @ObservationIgnored private var searchTask: Task<Void, Never>?
    @ObservationIgnored private var loadGeneration = 0
    static let searchDebounce: Duration = .milliseconds(40)
    @ObservationIgnored private let core: CoreClient?

    init(core: CoreClient?) {
        self.core = core
    }

    var hasMore: Bool { nextCursor != nil }

    func show(mailboxID: String) async {
        self.mailboxID = mailboxID
        searchTask?.cancel()
        searchQuery = nil
        searchError = nil
        nextCursor = nil
        await load(replacing: true, limit: Self.pageSize)
    }

    /// As-you-type search: waits briefly for typing to pause, and drops
    /// results for queries that were superseded. An empty query returns to
    /// the mailbox.
    func search(_ text: String) {
        searchTask?.cancel()
        let query = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty else {
            if searchQuery != nil, let mailboxID {
                searchTask = Task { await show(mailboxID: mailboxID) }
            }
            return
        }
        searchTask = Task {
            try? await Task.sleep(for: Self.searchDebounce)
            guard !Task.isCancelled else { return }
            searchQuery = query
            nextCursor = nil
            await load(replacing: true, limit: Self.pageSize)
            await searchServerIfFew(query)
        }
    }

    /// Few local results: ask Gmail too, for mail outside the sync window,
    /// after a pause so typing does not spend quota (spec §7.4 follow-up).
    private func searchServerIfFew(_ query: String) async {
        guard let core, searchError == nil, rows.count < Self.serverSearchBelow else { return }
        try? await Task.sleep(for: Self.serverSearchDelay)
        guard !Task.isCancelled, searchQuery == query else { return }
        isSearchingServer = true
        defer { isSearchingServer = false }
        let arrived = (try? await core.searchServer(query, limit: Self.serverSearchLimit)) ?? 0
        guard !Task.isCancelled, searchQuery == query, arrived > 0 else { return }
        nextCursor = nil
        await load(replacing: true, limit: Self.pageSize)
    }

    static let serverSearchBelow = 20
    static let serverSearchDelay: Duration = .milliseconds(600)
    static let serverSearchLimit: UInt32 = 50
    /// Gmail is being searched for older mail.
    private(set) var isSearchingServer = false

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
        loadGeneration += 1
        let token = loadGeneration
        let query = searchQuery
        let page: ThreadPage
        do {
            if let query {
                page = try await core.search(query, after: cursor, limit: limit)
            } else {
                page = try await core.threads(in: mailboxID, after: cursor, limit: limit)
            }
        } catch {
            if query != nil, token == loadGeneration {
                searchError = error.message
            }
            return
        }
        // A newer load (another mailbox or query) started meanwhile.
        guard token == loadGeneration, mailboxID == self.mailboxID, query == searchQuery else { return }
        searchError = nil
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
