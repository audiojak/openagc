import AppKit
import Foundation
import Network
import Observation
import os
import UniformTypeIdentifiers

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

    /// The open account's id, if any (demo included).
    var openAccountID: String? {
        if case .open(let id) = accountState { return id }
        return nil
    }
    private(set) var syncDisplay: SyncDisplay = .idle
    /// Set when Google rejected the stored credentials; shows a banner.
    private(set) var needsReauthentication = false
    /// Why a sign-in is needed, so Settings can say what happened.
    enum ReauthenticationReason { case googleRejected, savedSignInUnavailable }
    private(set) var reauthenticationReason: ReauthenticationReason?
    /// Why the last sign-in attempt failed, for the onboarding screen.
    private(set) var signInError: String?
    private(set) var accountEmail: String?
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
    /// One agent panel per account, kept once opened so a conversation on
    /// one account carries on while the window shows another (spec §7.7).
    private var agentStores: [String: AgentStore] = [:]
    private let fallbackAgent: AgentStore
    var agent: AgentStore { openAccountID.flatMap { agentStores[$0] } ?? fallbackAgent }
    /// Imports by archive account, latest status (spec §7.8).
    private(set) var imports: [String: ImportStatus] = [:]
    /// The import being set up (the sheet), then the one running (the
    /// progress sheet).
    var importDraft: ImportDraft?
    var runningImport: String?
    /// The user's accounts in their order, with Inbox unread counts.
    private(set) var accounts: [AccountSummary] = []
    /// Where each account's window was (mailbox, thread), restored on switch.
    @ObservationIgnored private var placeByAccount: [String: (mailbox: String?, thread: String?)] = [:]
    let routines: RoutinesStore
    let core: CoreClient?

    private let logger = Logger(subsystem: "ai.actual.openagc", category: "app")
    private var eventTask: Task<Void, Never>?
    private var signInSession: String?
    private var lifecycleObservers: [NSObjectProtocol] = []
    private let networkMonitor = NWPathMonitor()

    /// The app's preferences; tests pass a throwaway suite so they never
    /// touch the real app's (the test host *is* the app).
    @ObservationIgnored let defaults: UserDefaults

    init(core: CoreClient?, defaults: UserDefaults = .standard) {
        self.core = core
        self.defaults = defaults
        accountEmail = defaults.string(forKey: "accountEmail")
        mailboxes = MailboxStore(core: core)
        threads = ThreadListStore(core: core)
        reader = ReaderStore(core: core)
        fallbackAgent = AgentStore(core: core)
        routines = RoutinesStore(core: core)
    }

    /// Open the remembered account, or the demo when asked for on launch.
    func start(openDemo: Bool? = nil) async {
        let openDemo = openDemo ?? defaults.bool(forKey: "OpenAGCDemo")
        guard let core else {
            accountState = .failed("The core failed to start. See ~/Library/Logs/OpenAGC/core.log.")
            return
        }
        listenForEvents(from: core)
        applyAgentPolicy()
        notifier.openThread = { [weak self] thread, account in
            guard let self else { return }
            Task { await self.reveal(threadID: thread, in: account) }
        }
        notifier.install()
        if openDemo {
            await openDemoMailbox()
        } else if let id = defaults.string(forKey: "accountID") {
            await open(accountID: id)
        } else if let first = try? await core.accounts().first {
            // No remembered choice (a fresh preference file): the first account.
            defaults.set(first.id, forKey: "accountID")
            await open(accountID: first.id)
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

    /// Sign in to Gmail: again as the open account, or (`adding`) as a new
    /// one, in which case Google shows its account chooser and a cancelled
    /// sign-in returns to the account that was open (spec §7.7).
    func signIn(with client: GoogleClientConfiguration, adding: Bool = false, fullAccess: Bool = false) async {
        guard let core, client.isUsable else { return }
        let previous = openAccountID
        if adding, let previous { placeByAccount[previous] = (selectedMailboxID, selectedThreadID) }
        signInError = nil
        accountState = .signingIn
        do {
            let start = try await core.beginGmailSignIn(clientID: client.clientID, clientSecret: client.clientSecret,
                                                        loginHint: adding ? nil : accountEmail,
                                                        fullAccess: fullAccess)
            signInSession = start.sessionID
            NSWorkspace.shared.open(start.authorizationURL)
            let account = try await core.completeGmailSignIn(start.sessionID)
            signInSession = nil
            defaults.set(account.accountID, forKey: "accountID")
            defaults.set(account.email, forKey: "accountEmail")
            accountEmail = account.email
            needsReauthentication = false
            reauthenticationReason = nil
            if adding {
                selectedThreadIDs = []
                selectedThreadID = nil
                selectedMailboxIDSilently("INBOX")
            }
            await open(accountID: account.accountID)
        } catch {
            signInSession = nil
            logger.error("sign-in failed: \(error.message, privacy: .private)")
            signInError = error.message
            // Adding an account and giving up leaves the open one as it was.
            if let previous {
                accountState = .open(accountID: previous)
            } else {
                accountState = .noAccount
            }
        }
    }

    /// Faster download over IMAP for an account (spec §7.4): on means
    /// signing in again with full mail access; off stops using it.
    func setFasterDownload(_ on: Bool, for accountID: String) async {
        guard let core else { return }
        if on {
            await switchAccount(to: accountID)
            await signIn(with: .effective(), fullAccess: true)
        } else {
            try? await core.disableIMAP(accountID)
            await reloadAccounts()
        }
    }

    /// Add another Gmail account (the avatar menu's Add Account…).
    func addAccount() async {
        await signIn(with: .effective(), adding: true)
    }

    func cancelSignIn() {
        if let session = signInSession { core?.cancelGmailSignIn(session) }
        signInSession = nil
        accountState = .noAccount
    }

    func signOut() async {
        guard let core, case let .open(accountID) = accountState else { return }
        try? await core.signOut(accountID)
        defaults.removeObject(forKey: "accountID")
        selectedThreadID = nil
        accountState = .noAccount
    }

    // MARK: Account

    private func open(accountID: String, startingSync: Bool = true) async {
        guard let core else { return }
        do {
            try await core.openAccount(accountID)
            if agentStores[accountID] == nil { agentStores[accountID] = AgentStore(core: core) }
            accountState = .open(accountID: accountID)
            await mailboxes.reload()
            await reloadAccounts()
            await threads.show(mailboxID: selectedMailboxID ?? "INBOX")
            if let summary = accounts.first(where: { $0.id == accountID }) {
                accountEmail = summary.email
                defaults.set(summary.email, forKey: "accountEmail")
            }
            needsReauthentication = false
            reauthenticationReason = nil
            // An imported mailbox has no server and no sign-in (spec §7.8).
            if accountID != Self.demoAccountID, !core.isArchive(accountID) {
                // A Keychain that will not hand over the sign-in (for example
                // after an unsigned rebuild) means "sign in again", not silence.
                let hasCredentials: Bool
                do {
                    hasCredentials = try core.accountHasCredentials(accountID)
                } catch {
                    logger.warning("stored sign-in unreadable: \(error.message, privacy: .public)")
                    hasCredentials = false
                }
                if hasCredentials {
                    if startingSync {
                        try core.startSync()
                        // The other accounts sync behind this one (spec §7.7).
                        Task { _ = try? await core.startAllSync() }
                    }
                    observeLifecycle()
                } else {
                    needsReauthentication = true
                    reauthenticationReason = .savedSignInUnavailable
                }
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
        let tools = defaults.stringArray(forKey: Self.agentApprovalKey) ?? []
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

    /// The Dock badge: Inbox unread across every account, or the open
    /// mailbox's when there are no accounts (the demo).
    func updateBadge() {
        let current = mailboxes.mailboxes.first { $0.kind == .inbox }?.unreadCount ?? 0
        let others = accounts.filter { $0.id != openAccountID }.reduce(UInt32(0)) { $0 + $1.inboxUnread }
        notifier.updateBadge(inboxUnread: current + others)
    }

    // MARK: Accounts

    func reloadAccounts() async {
        guard let core else { return }
        if let fresh = try? await core.accounts(), fresh != accounts { accounts = fresh }
        updateBadge()
    }

    /// Show another account (spec §7.7). Its sync is already running in the
    /// background; the window re-binds to its store, and returns to where
    /// that account was last left. Open composers keep their own account.
    func switchAccount(to accountID: String) async {
        guard accountID != openAccountID, core != nil else { return }
        if let current = openAccountID {
            placeByAccount[current] = (selectedMailboxID, selectedThreadID)
        }
        defaults.set(accountID, forKey: "accountID")
        let place = placeByAccount[accountID]
        selectedThreadIDs = []
        selectedThreadID = nil
        searchText = ""
        selectedMailboxIDSilently(place?.mailbox ?? "INBOX")
        await open(accountID: accountID, startingSync: false)
        routinesRevision += 1
        if let thread = place?.thread, threads.rows.contains(where: { $0.id == thread }) {
            selectedThreadID = thread
        }
    }

    // MARK: Import (spec §7.8)

    /// File › Import Mailbox…: pick an .mbox file or a folder of them.
    func beginImport() async {
        let panel = NSOpenPanel()
        panel.title = "Import Mailbox"
        panel.message = "Choose an .mbox file, or a folder of them (for example from Google Takeout)."
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [UTType(filenameExtension: "mbox") ?? .data, .folder]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        await prepareImport(path: url.path)
    }

    /// Look at the mailbox and open the import sheet with suggestions.
    func prepareImport(path: String) async {
        guard let core else { return }
        do {
            let scan = try await core.scanMailbox(path)
            importDraft = ImportDraft(path: path, scan: scan, name: scan.suggestedName,
                                      addresses: scan.suggestedAddress ?? "")
        } catch {
            importDraft = ImportDraft(path: path, scan: nil, name: "", addresses: "", error: error.message)
        }
    }

    /// Start the import the sheet describes; the progress sheet follows it
    /// and the new account opens when it is done.
    func confirmImport() async {
        guard let core, let draft = importDraft, draft.scan != nil else { return }
        do {
            let id = try await core.startImport(path: draft.path, name: draft.name, myAddresses: draft.addressList)
            importDraft = nil
            runningImport = id
        } catch {
            importDraft?.error = error.message
        }
    }

    func cancelRunningImport() {
        guard let id = runningImport else { return }
        core?.cancelImport(id)
    }

    /// Close the progress sheet; show the imported account if it has mail.
    func finishImport(show: Bool) async {
        guard let id = runningImport else { return }
        runningImport = nil
        if show { await switchAccount(to: id) }
    }

    /// Tag notifications with their account; the label only matters when
    /// the user has more than one.
    func notificationTag(for accountID: String?) -> NewMailNotifier.AccountTag? {
        guard let accountID else { return nil }
        let label = accounts.count > 1 ? accounts.first { $0.id == accountID }.map { $0.displayName ?? $0.email } : nil
        return .init(id: accountID, label: label)
    }

    /// Remove an account from this Mac (Settings). If it is on screen, the
    /// next account opens; with none left, onboarding.
    func removeAccount(_ accountID: String) async {
        guard let core else { return }
        do {
            try await core.removeAccount(accountID)
        } catch {
            logger.error("removing an account failed: \(error.message, privacy: .private)")
            return
        }
        agentStores[accountID] = nil
        placeByAccount[accountID] = nil
        let wasOpen = openAccountID == accountID
        await reloadAccounts()
        guard wasOpen else { return }
        if let next = accounts.first {
            accountState = .starting
            await switchAccount(to: next.id)
        } else {
            defaults.removeObject(forKey: "accountID")
            defaults.removeObject(forKey: "accountEmail")
            accountEmail = nil
            selectedThreadID = nil
            accountState = .noAccount
        }
    }

    /// Switch to the account at `position` in the list (⌃1–⌃9).
    func switchAccount(position: Int) async {
        guard accounts.indices.contains(position) else { return }
        await switchAccount(to: accounts[position].id)
    }

    /// Show a thread from a notification: switch to its account if need
    /// be, then to the Inbox, and select it.
    func reveal(threadID: String, in accountID: String?) async {
        if let accountID, accountID != openAccountID, accounts.contains(where: { $0.id == accountID }) {
            await switchAccount(to: accountID)
        }
        reveal(threadID: threadID)
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

    /// The account on screen is an imported mailbox (spec §7.8): it cannot
    /// compose, reply, forward or send. The core refuses too; this keeps
    /// the commands from being offered.
    var isArchive: Bool {
        accounts.first { $0.id == openAccountID }?.kind == .archive
    }

    static let cannotSendReason = "This is an imported mailbox; it cannot send mail."

    func compose(_ request: ComposeRequest) {
        guard !isArchive else { return }
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

    /// User labels by id, for the chips on thread rows.
    var chipLabels: [String: ThreadRowView.Chip] {
        model_chipLabels(mailboxes)
    }

    /// Create a label (a `/` path creates missing parents) and apply it to
    /// the threads the next action targets. Returns an error message to
    /// show, or nil on success.
    func createLabel(path: String, applyToTargets: Bool = true) async -> String? {
        guard let core else { return "OpenAGC is not ready." }
        let ids = actionTargets
        do {
            let label = try await core.createLabel(path)
            await mailboxes.reload()
            if applyToTargets, !ids.isEmpty {
                try await core.modifyLabels(ids, add: [label.id], remove: [])
                await mailboxes.reload()
                await threads.refresh()
            }
            return nil
        } catch {
            return error.message
        }
    }

    /// Label threads dropped on a sidebar label (they need not be selected).
    func addLabel(_ labelID: String, toThreads ids: [String]) {
        guard let core, !ids.isEmpty else { return }
        Task { await perform { try await core.modifyLabels(ids, add: [labelID], remove: []) } }
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

    /// Set the mailbox without loading it (a switch loads the new account's).
    private func selectedMailboxIDSilently(_ id: String) {
        suppressMailboxChange = true
        selectedMailboxID = id
        suppressMailboxChange = false
    }

    @ObservationIgnored private var suppressMailboxChange = false
    @ObservationIgnored private var accountsReload: Task<Void, Never>?

    private func scheduleAccountsReload() {
        guard accountsReload == nil else { return }
        accountsReload = Task { [weak self] in
            try? await Task.sleep(for: .seconds(3))
            await self?.reloadAccounts()
            self?.accountsReload = nil
        }
    }

    private func mailboxChanged() {
        if suppressMailboxChange { return }
        selectedThreadID = nil
        selectedThreadIDs = []
        searchText = ""

        guard case .open = accountState, let id = selectedMailboxID else { return }
        Task { await threads.show(mailboxID: id) }
    }

    private func listenForEvents(from core: CoreClient) {
        eventTask = Task { [weak self] in
            for await tagged in core.events {
                guard let self else { return }
                await self.handle(tagged)
            }
        }
    }

    /// Events for the account on screen, or app-wide ones, update the
    /// window. Another account's events only announce new mail; its
    /// counts are read when the avatar menu opens (spec §7.7).
    func isForWindow(_ tagged: CoreClientEvent.Tagged) -> Bool {
        guard let account = tagged.accountID else { return true }
        return account == openAccountID
    }

    private func handle(_ tagged: CoreClientEvent.Tagged) async {
        // An agent session reports to its own account's panel, shown or not.
        if case let .agent(sessionID, events) = tagged.event, let account = tagged.accountID,
           let store = agentStores[account] {
            await store.apply(sessionID: sessionID, events: events)
            return
        }
        guard isForWindow(tagged) else {
            switch tagged.event {
            case let .newMail(mail):
                notifier.announce(mail, account: notificationTag(for: tagged.accountID))
                await reloadAccounts()
            case let .importProgress(status):
                imports[tagged.accountID ?? ""] = status
                if status.done { await reloadAccounts() }
            case .threadsChanged:
                // Another account's counts moved: refresh the menu and Dock
                // at most every few seconds rather than on every batch.
                scheduleAccountsReload()
            default:
                break
            }
            return
        }
        let event = tagged.event
        switch event {
        case let .threadsChanged(mailboxID, hint):
            await mailboxes.reload()
            updateBadge()
            if mailboxID == threads.mailboxID || threads.searchQuery != nil {
                await threads.apply(hint)
            }
            // A thread shown with headers only gets its bodies: show them.
            if reader.isWaitingForBodies, let shown = reader.threadID,
               hint.invalidate || hint.updated.contains(shown) {
                await reader.reload()
            }
        case let .error(error):
            logger.error("core error: \(error.message, privacy: .private)")
            if error.kind == .auth {
                needsReauthentication = true
                reauthenticationReason = .googleRejected
            }
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
            notifier.announce(mail, account: notificationTag(for: tagged.accountID))
        case let .agent(sessionID, events):
            await agent.apply(sessionID: sessionID, events: events)
        case .routinesChanged:
            routinesRevision += 1
        case let .importProgress(status):
            imports[tagged.accountID ?? ""] = status
            if status.done {
                await mailboxes.reload()
                await threads.refresh()
                await reloadAccounts()
            }
        }
    }
}

@MainActor
private func model_chipLabels(_ mailboxes: MailboxStore) -> [String: ThreadRowView.Chip] {
    mailboxes.labels.reduce(into: [:]) { acc, m in
        guard let id = m.labelId else { return }
        acc[id] = ThreadRowView.Chip(path: m.name, color: mailboxes.labelColors[id])
    }
}
