import AppKit
import Foundation
import Network
import Observation
import os

/// App-wide state: the core, the open account, and what is selected.
/// Routes core events to the stores that care (spec §14.2).
@MainActor
@Observable
final class AppModel {
    enum AccountState: Equatable {
        case starting
        case noAccount
        case signingIn
        case open(accountID: String)
        case failed(String)
    }

    enum SyncDisplay: Equatable {
        case idle, syncing(pending: UInt32), offline, error
    }

    static let demoAccountID = "demo"
    static let demoThreadCount: UInt32 = 2_000

    private(set) var accountState: AccountState = .starting
    private(set) var syncDisplay: SyncDisplay = .idle
    /// Set when Google rejected the stored credentials; shows a banner.
    private(set) var needsReauthentication = false
    /// Why the last sign-in attempt failed, for the onboarding screen.
    private(set) var signInError: String?
    private(set) var accountEmail: String? = UserDefaults.standard.string(forKey: "accountEmail")
    var selectedMailboxID: String? = "INBOX" {
        didSet { if selectedMailboxID != oldValue { mailboxChanged() } }
    }
    var selectedThreadID: String?
    /// The toolbar search field's text.
    var searchText = "" {
        didSet { if searchText != oldValue { threads.search(searchText) } }
    }
    /// Every selected thread; actions apply to all of them.
    var selectedThreadIDs: Set<String> = []
    /// Failed changes that were undone, shown as a banner.
    private(set) var failedChanges: UInt32 = 0

    /// Opens a composer window; set by the main window, which has SwiftUI's
    /// `openWindow` action.
    @ObservationIgnored var openComposer: ((ComposeRequest) -> Void)?
    /// Opens the Routines window; set by the main window.
    @ObservationIgnored var openRoutines: (() -> Void)?

    let notifier = NewMailNotifier()
    let mailboxes: MailboxStore
    let threads: ThreadListStore
    let reader: ReaderStore
    let agent: AgentStore
    let routines: RoutinesStore
    let core: CoreClient?

    private let logger = Logger(subsystem: "ai.actual.openagc", category: "app")
    private var eventTask: Task<Void, Never>?
    private var signInSession: String?
    private var lifecycleObservers: [NSObjectProtocol] = []
    private let networkMonitor = NWPathMonitor()

    init(core: CoreClient?) {
        self.core = core
        mailboxes = MailboxStore(core: core)
        threads = ThreadListStore(core: core)
        reader = ReaderStore(core: core)
        agent = AgentStore(core: core)
        routines = RoutinesStore(core: core)
    }

    /// Open the remembered account, or the demo when asked for on launch.
    func start(openDemo: Bool = UserDefaults.standard.bool(forKey: "OpenAGCDemo")) async {
        guard let core else {
            accountState = .failed("The core failed to start. See ~/Library/Logs/OpenAGC/core.log.")
            return
        }
        listenForEvents(from: core)
        applyAgentPolicy()
        notifier.openThread = { [weak self] in self?.reveal(threadID: $0) }
        notifier.install()
        if openDemo {
            await openDemoMailbox()
        } else if let id = UserDefaults.standard.string(forKey: "accountID") {
            await open(accountID: id)
        } else {
            accountState = .noAccount
        }
    }

    func openDemoMailbox() async {
        guard let core else { return }
        do {
            try await core.openAccount(Self.demoAccountID)
            let inbox = try await core.mailboxes().first { $0.kind == .inbox }
            if (inbox?.totalCount ?? 0) == 0 {
                try await core.seedDemoMailbox(threads: Self.demoThreadCount)
            }
            await open(accountID: Self.demoAccountID)
        } catch {
            accountState = .failed(error.message)
        }
    }

    // MARK: Sign-in

    func signIn(with client: GoogleClientConfiguration) async {
        guard let core, client.isUsable else { return }
        signInError = nil
        accountState = .signingIn
        do {
            let start = try await core.beginGmailSignIn(clientID: client.clientID, clientSecret: client.clientSecret,
                                                        loginHint: accountEmail)
            signInSession = start.sessionID
            NSWorkspace.shared.open(start.authorizationURL)
            let account = try await core.completeGmailSignIn(start.sessionID)
            signInSession = nil
            UserDefaults.standard.set(account.accountID, forKey: "accountID")
            UserDefaults.standard.set(account.email, forKey: "accountEmail")
            accountEmail = account.email
            needsReauthentication = false
            await open(accountID: account.accountID)
        } catch {
            signInSession = nil
            logger.error("sign-in failed: \(error.message, privacy: .private)")
            signInError = error.message
            accountState = .noAccount
        }
    }

    func cancelSignIn() {
        if let session = signInSession { core?.cancelGmailSignIn(session) }
        signInSession = nil
        accountState = .noAccount
    }

    func signOut() async {
        guard let core, case let .open(accountID) = accountState else { return }
        try? await core.signOut(accountID)
        UserDefaults.standard.removeObject(forKey: "accountID")
        selectedThreadID = nil
        accountState = .noAccount
    }

    // MARK: Account

    private func open(accountID: String) async {
        guard let core else { return }
        do {
            try await core.openAccount(accountID)
            accountState = .open(accountID: accountID)
            await mailboxes.reload()
            updateBadge()
            await threads.show(mailboxID: selectedMailboxID ?? "INBOX")
            if accountID != Self.demoAccountID, core.accountHasCredentials(accountID) {
                try core.startSync()
                observeLifecycle()
            }
        } catch {
            logger.error("opening account failed: \(error.message, privacy: .private)")
            accountState = .failed(error.message)
        }
    }

    // MARK: Menus

    var isMailOpen: Bool {
        if case .open = accountState { return true }
        return false
    }

    /// Bumped to move focus to the toolbar search field.
    private(set) var searchFocusRequests = 0

    func focusSearch() {
        searchFocusRequests += 1
    }

    /// Bumped when routines change, so their views reload.
    private(set) var routinesRevision = 0

    // MARK: Agent

    static let agentApprovalKey = "agentApproveTools"

    /// Push the user's approval choices to the core (they live in defaults).
    func applyAgentPolicy() {
        let tools = UserDefaults.standard.stringArray(forKey: Self.agentApprovalKey) ?? []
        do {
            try core?.setAgentPolicy(tools)
        } catch {
            logger.error("agent policy rejected: \(error.message, privacy: .private)")
        }
    }

    /// Bumped to move focus to the agent prompt (⌘K).
    private(set) var agentFocusRequests = 0

    func focusAgentPrompt() {
        agentFocusRequests += 1
    }

    /// Ask the agent, with references to what the user is looking at.
    func askAgent(_ prompt: String) async {
        let context = PromptContextInfo(mailboxId: selectedMailboxID, selectedThreadIds: actionTargets,
                                        searchQuery: threads.searchQuery)
        await agent.send(prompt, context: context)
    }

    // MARK: Notifications

    func updateBadge() {
        notifier.updateBadge(inboxUnread: mailboxes.mailboxes.first { $0.kind == .inbox }?.unreadCount ?? 0)
    }

    /// Show a thread from a notification: switch to the Inbox and select it.
    func reveal(threadID: String) {
        if selectedMailboxID != "INBOX" { selectedMailboxID = "INBOX" }
        searchText = ""
        selectedThreadIDs = []
        selectedThreadID = threadID
    }

    // MARK: Compose

    /// The message Reply and Forward act on: the latest one in the thread
    /// being read that is not a draft.
    var replyTargetMessageID: String? {
        guard let detail = reader.detail, detail.thread.id == selectedThreadID else { return nil }
        return detail.messages.last { !$0.isDraft }?.id
    }

    func compose(_ request: ComposeRequest) {
        openComposer?(request)
    }

    func reply(all: Bool) {
        guard let id = replyTargetMessageID else { return }
        compose(.reply(messageID: id, all: all))
    }

    func forward() {
        guard let id = replyTargetMessageID else { return }
        compose(.forward(messageID: id))
    }

    // MARK: Actions on the selection

    /// The threads an action applies to: the multi-selection, else the one
    /// being read.
    var actionTargets: [String] {
        if !selectedThreadIDs.isEmpty { return threads.rows.map(\.id).filter(selectedThreadIDs.contains) }
        return selectedThreadID.map { [$0] } ?? []
    }

    /// Archive (or trash): rows leave at once and the next row is selected.
    func archiveSelection() { removeFromList(action: { core, ids in try await core.archive(ids) }) }
    func trashSelection() { removeFromList(action: { core, ids in try await core.trash(ids) }) }

    func moveSelectionToInbox() {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        if selectedMailboxID != "INBOX" { dropFromList(ids) }
        Task { await perform { try await core.moveToInbox(ids) } }
    }

    /// Toggle read: if any target is unread, mark all read; else all unread.
    func toggleReadSelection() {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        let anyUnread = threads.rows.contains { ids.contains($0.id) && $0.unreadCount > 0 }
        threads.optimisticallyUpdate(Set(ids)) { $0.unreadCount = anyUnread ? 0 : max($0.unreadCount, 1) }
        Task { await perform { try await core.setRead(ids, anyUnread) } }
    }

    func toggleStarSelection() {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        let allStarred = threads.rows.filter { ids.contains($0.id) }.allSatisfy(\.isStarred)
        threads.optimisticallyUpdate(Set(ids)) { $0.isStarred = !allStarred }
        if selectedMailboxID == "STARRED", allStarred { dropFromList(ids) }
        Task { await perform { try await core.setStarred(ids, !allStarred) } }
    }

    func setLabel(_ labelID: String, applied: Bool) {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        if !applied, selectedMailboxID == labelID { dropFromList(ids) }
        Task {
            await perform {
                try await core.modifyLabels(ids, add: applied ? [labelID] : [], remove: applied ? [] : [labelID])
            }
        }
    }

    func dismissFailedChanges() {
        guard let core else { return }
        Task { try? await core.clearFailedChanges() }
    }

    private func removeFromList(action: @escaping @Sendable (CoreClient, [String]) async throws -> Void) {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        dropFromList(ids)
        Task { await perform { try await action(core, ids) } }
    }

    /// Remove rows and move the selection to the row after the last one
    /// removed, like Mail.
    private func dropFromList(_ ids: [String]) {
        let removed = Set(ids)
        let rows = threads.rows
        let lastIndex = rows.lastIndex { removed.contains($0.id) } ?? 0
        let next = rows[(lastIndex + 1)...].first { !removed.contains($0.id) }
            ?? rows[..<lastIndex].last { !removed.contains($0.id) }
        threads.optimisticallyRemove(removed)
        selectedThreadIDs = []
        selectedThreadID = next?.id
    }

    private func perform(_ body: () async throws -> Void) async {
        do {
            try await body()
        } catch {
            logger.error("action failed: \(String(describing: error), privacy: .private)")
            // The store was not changed; bring the list back in line.
            await threads.refresh()
        }
    }

    /// Poll faster while active; sync at once on activation, wake from
    /// sleep, and when the network comes back (spec §7.4).
    private func observeLifecycle() {
        guard lifecycleObservers.isEmpty, let core else { return }
        let center = NotificationCenter.default
        lifecycleObservers.append(center.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { _ in
            core.setAppActive(true)
        })
        lifecycleObservers.append(center.addObserver(forName: NSApplication.didResignActiveNotification, object: nil, queue: .main) { _ in
            core.setAppActive(false)
        })
        lifecycleObservers.append(NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { _ in core.syncNow() })
        networkMonitor.pathUpdateHandler = { path in
            if path.status == .satisfied { core.syncNow() }
        }
        networkMonitor.start(queue: DispatchQueue(label: "ai.actual.openagc.network"))
    }

    private func mailboxChanged() {
        selectedThreadID = nil
        selectedThreadIDs = []
        searchText = ""

        guard case .open = accountState, let id = selectedMailboxID else { return }
        Task { await threads.show(mailboxID: id) }
    }

    private func listenForEvents(from core: CoreClient) {
        eventTask = Task { [weak self] in
            for await event in core.events {
                guard let self else { return }
                await self.handle(event)
            }
        }
    }

    private func handle(_ event: CoreClientEvent) async {
        switch event {
        case let .threadsChanged(mailboxID, hint):
            await mailboxes.reload()
            updateBadge()
            if mailboxID == threads.mailboxID || threads.searchQuery != nil {
                await threads.apply(hint)
            }
        case let .error(error):
            logger.error("core error: \(error.message, privacy: .private)")
            if error.kind == .auth { needsReauthentication = true }
        case let .syncStatus(state, pending):
            switch state {
            case .idle: syncDisplay = .idle
            case .bootstrapping, .syncing: syncDisplay = pending > 0 || state == .bootstrapping ? .syncing(pending: pending) : .idle
            case .offline: syncDisplay = .offline
            case .error: syncDisplay = .error
            }
        case let .outboxStatus(_, failed):
            failedChanges = failed
        case let .newMail(mail):
            notifier.announce(mail)
        case let .agent(sessionID, events):
            await agent.apply(sessionID: sessionID, events: events)
        case .routinesChanged:
            routinesRevision += 1
        }
    }
}
