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

    /// The most messages seen waiting in the current sync, for the
    /// sidebar's progress bar; reset when sync goes idle.
    private(set) var syncTotal: UInt32 = 0

    /// How far the current download has got, 0…1.
    var syncProgress: Double {
        guard case let .syncing(pending, headers) = syncDisplay, syncTotal > 0 else { return 0 }
        return 1 - Double(pending + headers) / Double(syncTotal)
    }

    /// The sidebar's heading for the open account's own mailboxes.
    var accountSectionTitle: String {
        if openAccountID == Self.demoAccountID { return "Demo Mailbox" }
        let account = accounts.first { $0.id == openAccountID }
        return account.map { $0.displayName ?? $0.email } ?? accountEmail ?? "Mailboxes"
    }

    enum SyncDisplay: Equatable {
        /// `headers`: messages waiting for headers only (tiered download).
        case idle, syncing(pending: UInt32, headers: UInt32 = 0), offline, error
    }

    static let demoAccountID = "demo"
    static let demoThreadCount: UInt32 = 2_000

    private(set) var accountState: AccountState = .starting

    /// The Inbox shows only threads Gmail marked Important (a per-account
    /// switch at the top of the Inbox list).
    var inboxImportantOnly: Bool {
        get { inboxImportantOnlyLoaded }
        set {
            guard newValue != inboxImportantOnlyLoaded else { return }
            inboxImportantOnlyLoaded = newValue
            if let id = openAccountID { defaults.set(newValue, forKey: Self.importantOnlyKey(id)) }
            relist()
        }
    }

    private var inboxImportantOnlyLoaded = false

    static func importantOnlyKey(_ accountID: String) -> String { "inboxImportantOnly.\(accountID)" }

    /// The Inbox leaves out threads with an open task (they carry the
    /// account's `Task` label; spec §14.8, §14.3), remembered per account.
    var inboxHidesTasks: Bool {
        get { inboxHidesTasksLoaded }
        set {
            guard newValue != inboxHidesTasksLoaded else { return }
            inboxHidesTasksLoaded = newValue
            if let id = openAccountID { defaults.set(newValue, forKey: Self.hideTasksKey(id)) }
            relist()
        }
    }

    private var inboxHidesTasksLoaded = false
    /// The open account's `Task` label, once it has one.
    var taskLabelID: String?

    static func hideTasksKey(_ accountID: String) -> String { "inboxHideTasks.\(accountID)" }

    /// The narrowing that hides emails with tasks, when on and possible.
    var hiddenTaskLabel: String? { inboxHidesTasks ? taskLabelID : nil }

    /// The open account's addresses, aliases included: rows show other
    /// people, and "Me" only when it is just you.
    private(set) var ownAddresses: Set<String> = []

    /// Every Inbox category with its counts, narrowed like the list
    /// (Important only); the tabs are `InboxCategories.visible` of these.
    private(set) var inboxCategoryCounts: [InboxCategory] = []

    /// The Inbox's category tabs are on (per account; on by default).
    var showCategories: Bool {
        get { showCategoriesLoaded }
        set {
            guard newValue != showCategoriesLoaded else { return }
            showCategoriesLoaded = newValue
            if let id = openAccountID { defaults.set(newValue, forKey: Self.showCategoriesKey(id)) }
            relist()
        }
    }

    private var showCategoriesLoaded = true

    /// The category tab the user chose (per account); the list shows it
    /// while it has mail, otherwise Primary.
    var inboxCategory: String {
        get { inboxCategoryLoaded }
        set {
            guard newValue != inboxCategoryLoaded else { return }
            inboxCategoryLoaded = newValue
            if let id = openAccountID { defaults.set(newValue, forKey: Self.inboxCategoryKey(id)) }
            relist()
        }
    }

    private var inboxCategoryLoaded = InboxCategories.primary
    @ObservationIgnored private var categoryGeneration = 0

    static func showCategoriesKey(_ accountID: String) -> String { "inboxShowCategories.\(accountID)" }
    static let dismissedTipsKey = "dismissedTips"

    /// Tips the user acted on or put away; they never come back.
    private(set) var dismissedTips: Set<String> = []

    /// The tip over the Inbox now, if any.
    var currentTip: Tip? {
        Tip.next(dismissed: dismissedTips, context: Tip.Context(
            inInbox: selectedMailboxID == "INBOX",
            searching: threads.searchQuery != nil,
            categoriesAvailable: inboxCategoryCounts.contains { $0.id != InboxCategories.primary && $0.totalCount > 0 },
            categoriesShown: showCategories,
            importantOnly: inboxImportantOnly,
            agentShown: agent.isPresented))
    }

    /// Act on a tip (`accept`) or put it away; either way it is done.
    func finishTip(_ tip: Tip, accept: Bool) {
        switch (tip, accept) {
        case (.categories, false): showCategories = false
        case (.importantOnly, true): inboxImportantOnly = true
        case (.agent, true):
            agent.isPresented = true
            focusAgentPrompt()
        default: break
        }
        dismissedTips.insert(tip.rawValue)
        defaults.set(Array(dismissedTips).sorted(), forKey: Self.dismissedTipsKey)
    }
    static func inboxCategoryKey(_ accountID: String) -> String { "inboxCategory.\(accountID)" }

    /// The tabs above the Inbox list; empty when categories are off or
    /// the account has none.
    var inboxCategoryTabs: [InboxCategory] {
        showCategories ? InboxCategories.visible(inboxCategoryCounts) : []
    }

    /// The tab the Inbox list is narrowed to, if any.
    var activeInboxCategory: String? {
        InboxCategories.active(chosen: inboxCategory, visible: inboxCategoryTabs)
    }

    /// What the thread list shows: the selected mailbox; in the Inbox,
    /// narrowed to Important when that switch is on and to the category
    /// tab when there are tabs (`INBOX+IMPORTANT+CATEGORY_SOCIAL`).
    var listMailboxID: String? {
        guard let id = selectedMailboxID, id != Self.tasksMailboxID, id != Self.guideMailboxID else { return nil }
        var parts = [id]
        if id == "INBOX" {
            if inboxImportantOnly { parts.append("IMPORTANT") }
            if let hidden = hiddenTaskLabel { parts.append("!" + hidden) }
            if let category = activeInboxCategory { parts.append(category) }
        }
        parts += ListFilter.ordered(listFilters).map(\.rawValue)
        return parts.joined(separator: "+")
    }

    /// The list filters (spec §14.3 amendment, filters): per window, kept
    /// across mailboxes, not remembered between launches.
    var listFilters: Set<ListFilter> = [] {
        didSet { if listFilters != oldValue { relist() } }
    }

    /// Run the search, or with nothing typed go back to the listing as it
    /// is now: filters or tabs may have changed during the search.
    private func searchChanged() {
        let query = filteredSearch
        guard query.isEmpty else { threads.search(query); return }
        threads.search("")
        if case .open = accountState, let id = listMailboxID, threads.searchQuery != nil || id != threads.mailboxID {
            Task { await threads.show(mailboxID: id) }
        }
    }

    /// The search as the store runs it: the typed query plus the filters'
    /// operators. Empty when nothing is typed (the filters then narrow the
    /// mailbox instead).
    var filteredSearch: String {
        let typed = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !typed.isEmpty else { return "" }
        let filters = ListFilter.ordered(listFilters).map(\.searchOperator)
        // Grouped, so `a OR b` is filtered as a whole (AND binds tighter).
        return filters.isEmpty ? typed : (["(\(typed))"] + filters).joined(separator: " ")
    }

    /// Show the list again after the Inbox's narrowing changed; the
    /// selection goes, as when choosing another mailbox.
    private func relist() {
        selectedThreadID = nil
        selectedThreadIDs = []
        guard case .open = accountState else { return }
        Task {
            await reloadInboxCategories()
            if !filteredSearch.isEmpty {
                threads.search(filteredSearch)
            } else if let id = listMailboxID, id != threads.mailboxID || threads.searchQuery != nil {
                await threads.show(mailboxID: id)
            }
        }
    }

    /// Re-count the Inbox's categories. Returns whether the list's
    /// narrowing changed as a result (a tab emptied or appeared).
    @discardableResult
    func reloadInboxCategories() async -> Bool {
        guard let core, case .open = accountState else { return false }
        categoryGeneration += 1
        let generation = categoryGeneration
        let before = listMailboxID
        let counts = (try? await core.inboxCategories(importantOnly: inboxImportantOnly, hiddenLabel: hiddenTaskLabel)) ?? []
        // A newer reload (the switch toggled again) wins.
        guard generation == categoryGeneration else { return false }
        inboxCategoryCounts = counts
        return listMailboxID != before
    }

    /// The open account's id, if any (demo included).
    var openAccountID: String? {
        if case .open(let id) = accountState { return id }
        return nil
    }
    private(set) var syncDisplay: SyncDisplay = .idle {
        didSet {
            switch syncDisplay {
            case let .syncing(pending, headers): syncTotal = max(syncTotal, pending + headers)
            default: syncTotal = 0
            }
        }
    }
    /// How the open account's backfill downloads bodies ("imap", "rest",
    /// "imap-refused"), for the sidebar's sync line.
    private(set) var backfillTransport: String?
    /// Why the account is on the Gmail API when it should be on IMAP (it
    /// failed, or was refused); nil otherwise. A quiet line in the sync
    /// footer (maintainer decision 3, docs/plans/imap-first-sync.md).
    private(set) var transportNote: String?

    static func transportNote(_ d: SyncDiagnostics) -> String? {
        guard d.syncing else { return nil }
        if d.backfill.transport == "imap-refused" { return "IMAP was refused for this account" }
        if d.breakerOpenUntil != nil { return "IMAP paused after errors" }
        return nil
    }
    @ObservationIgnored private var transportCheckedAt: Date = .distantPast
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
        didSet { if searchText != oldValue { searchChanged() } }
    }
    /// Every selected thread; actions apply to all of them.
    var selectedThreadIDs: Set<String> = []
    /// Failed changes that were undone, shown as a banner.
    private(set) var failedChanges: UInt32 = 0

    /// Opens a composer window; set by the main window, which has SwiftUI's
    /// `openWindow` action.
    @ObservationIgnored var openComposer: ((ComposeRequest) -> Void)?
    /// Opens a thread in a window of its own; set by the main window.
    @ObservationIgnored var openThreadWindow: ((ThreadWindowRequest) -> Void)?
    /// Opens the Routines window; set by the main window.
    @ObservationIgnored var openRoutines: (() -> Void)?
    @ObservationIgnored var openSyncDebugger: (() -> Void)?

    let notifier: NewMailNotifier
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
    /// The task dialog, while open (spec §14.8).
    var taskDraft: TaskDraft?
    /// The bulk sheet (`⇧T`), while open.
    var bulkTasks: BulkTaskDraft?
    /// The Settings tab to show when Settings next opens.
    var settingsTab: SettingsTab = .general
    /// Asked after a task's reply is sent: is the task done?
    var taskDoneQuestion: TaskDoneQuestion?
    /// The user's accounts in their order, with Inbox unread counts.
    private(set) var accounts: [AccountSummary] = []
    /// Where each account's window was (mailbox, thread), restored on switch.
    @ObservationIgnored private var placeByAccount: [String: (mailbox: String?, thread: String?)] = [:]
    let routines: RoutinesStore
    /// The task list (spec §14.8).
    let tasks: TaskListStore
    /// The writing guide (spec §14.9).
    let guide: GuideStore
    /// Undo for the user's mail actions, one stack per account (spec §14.6a).
    let undo: MailUndo
    let core: CoreClient?

    private let logger = Logger(subsystem: "ai.actual.openagc", category: "app")
    private var eventTask: Task<Void, Never>?
    private var signInSession: String?
    private var lifecycleObservers: [NSObjectProtocol] = []
    private let networkMonitor = NWPathMonitor()

    /// The app's preferences; tests pass a throwaway suite so they never
    /// touch the real app's (the test host *is* the app).
    @ObservationIgnored let defaults: UserDefaults

    /// `defaults` defaults to the app's preferences, which are a throwaway
    /// suite under tests and scratch runs (`CoreClient.appDefaults`).
    init(core: CoreClient?, defaults: UserDefaults = CoreClient.appDefaults()) {
        self.core = core
        self.defaults = defaults
        accountEmail = defaults.string(forKey: "accountEmail")
        dismissedTips = Set(defaults.stringArray(forKey: Self.dismissedTipsKey) ?? [])
        undoSendSeconds = (defaults.object(forKey: Self.undoSendKey) as? Int).map { UInt32(clamping: $0) } ?? 10
        mailboxes = MailboxStore(core: core)
        threads = ThreadListStore(core: core)
        reader = ReaderStore(core: core, defaults: defaults)
        notifier = NewMailNotifier(defaults: defaults)
        fallbackAgent = AgentStore(core: core, defaults: defaults)
        routines = RoutinesStore(core: core)
        tasks = TaskListStore(core: core)
        guide = GuideStore(core: core)
        undo = MailUndo(core: core)
        undo.onError = { [weak self] message in
            self?.logger.error("undo failed: \(message, privacy: .private)")
            Task { await self?.threads.refresh() }
        }
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
        core.setSendDelay(seconds: undoSendSeconds)
        notifier.openThread = { [weak self] thread, account in
            guard let self else { return }
            Task { await self.reveal(threadID: thread, in: account) }
        }
        notifier.openGuideDecisions = { [weak self] account in
            guard let self else { return }
            Task {
                if let account, account != self.openAccountID { await self.switchAccount(to: account) }
                self.openGuideDecisionsNow()
            }
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
    /// Full mail access is asked for by default: IMAP is the default
    /// transport (docs/plans/imap-first-sync.md).
    func signIn(with client: GoogleClientConfiguration, adding: Bool = false, fullAccess: Bool = true) async {
        guard let core, client.isUsable else { return }
        let previous = openAccountID
        accountBeforeSignIn = previous
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
            if error.kind == .cancelled {
                // cancelSignIn already put the window back.
                return
            }
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

    /// Sign an account in again, which grants the full mail access IMAP
    /// needs (an account signed in before IMAP became the default).
    func signInAgainForIMAP(_ accountID: String) async {
        await switchAccount(to: accountID)
        await signIn(with: .effective())
    }

    /// Add another Gmail account (the avatar menu's Add Account…).
    func addAccount() async {
        await signIn(with: .effective(), adding: true)
    }

    /// Stop waiting for the browser. Adding an account returns to the one
    /// that was open; otherwise back to onboarding.
    func cancelSignIn() {
        if let session = signInSession { core?.cancelGmailSignIn(session) }
        signInSession = nil
        if let previous = accountBeforeSignIn {
            accountState = .open(accountID: previous)
        } else {
            accountState = .noAccount
        }
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
            if agentStores[accountID] == nil { agentStores[accountID] = makeAgentStore(core: core, accountID: accountID) }
            accountState = .open(accountID: accountID)
            await mailboxes.reload()
            await reloadAccounts()
            inboxImportantOnlyLoaded = defaults.bool(forKey: Self.importantOnlyKey(accountID))
            inboxHidesTasksLoaded = defaults.bool(forKey: Self.hideTasksKey(accountID))
            taskLabelID = try? await core.taskLabelID()
            showCategoriesLoaded = defaults.object(forKey: Self.showCategoriesKey(accountID)) as? Bool ?? true
            inboxCategoryLoaded = defaults.string(forKey: Self.inboxCategoryKey(accountID)) ?? InboxCategories.primary
            await reloadInboxCategories()
            ownAddresses = await core.ownAddresses()
            reader.ownAddresses = ownAddresses
            await threads.show(mailboxID: listMailboxID ?? "INBOX")
            await tasks.load()
            // A learning run the app quit in the middle of carries on
            // (spec §14.9); a paused one waits for the user.
            guideProgress = try? await core.guideProgress()
            if guideProgress?.run?.status == .running { _ = try? await core.resumeGuideRun() }
            await checkGuideInvite()
            if let summary = accounts.first(where: { $0.id == accountID }) {
                accountEmail = summary.email
                defaults.set(summary.email, forKey: "accountEmail")
            }
            needsReauthentication = false
            reauthenticationReason = nil
            backfillTransport = nil
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
    /// Bumped when a task was added, changed or removed (spec §14.8).
    private(set) var tasksRevision = 0
    /// Bumped when the writing guide changed (spec §14.9).
    private(set) var guideRevision = 0
    /// The learning run's progress and the decisions waiting (spec §14.9).
    var guideProgress: GuideProgress?
    /// A sheet of the Writing Guide section, while open.
    var guideSheet: GuideSheet?
    /// Why the last guide action failed, shown in the section.
    var guideError: String?
    /// The invitation to a first run, or decisions waiting after one.
    var guidePrompt: GuidePrompt?
    /// The account whose invitation was put off: its banner shows until a
    /// run starts or it is dismissed.
    var guideBannerAccount: String?
    @ObservationIgnored var guideInviteChecked: Set<String> = []
    /// Opens Settings on the Agents tab (set by the window).
    @ObservationIgnored var openAgentSettings: (() -> Void)?

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
        if let id = openAccountID {
            RecentPrompts(defaults: defaults).record(prompt, for: id)
            recentPromptsRevision += 1
        }
        await agent.send(prompt, context: context)
    }

    // MARK: Agent suggestions (spec §14.6b)

    /// The prompt capsule's text, shared so a suggestion can fill it.
    var agentPromptDraft = ""
    /// Bumped when recent prompts change, so chips recompute.
    private(set) var recentPromptsRevision = 0

    /// What is on screen, for the suggestions.
    var suggestionContext: SuggestionContext {
        let targets = actionTargets
        let mailbox = mailboxes.mailboxes.first { $0.id == selectedMailboxID }
        let attachment = targets.count == 1 && reader.detail?.thread.id == targets.first && !reader.attachments.isEmpty
        return SuggestionContext(selectedCount: targets.count, mailboxID: selectedMailboxID,
                                 unreadInMailbox: Int(mailbox?.unreadCount ?? 0), searchQuery: threads.searchQuery,
                                 hasAttachment: attachment, canDraft: !isArchive)
    }

    /// Up to four chips over the empty prompt field.
    var agentChips: [AgentSuggestion] {
        _ = recentPromptsRevision
        let recent = openAccountID.map { RecentPrompts(defaults: defaults).prompts(for: $0) } ?? []
        return AgentSuggestions.chips(for: suggestionContext, recent: recent, day: AgentSuggestions.today())
    }

    /// A suggestion was chosen: send it, or put it in the field when it
    /// needs the user's words.
    /// Put an example in the prompt for the user to send or change (the
    /// agent column's examples).
    func fillPrompt(_ suggestion: AgentSuggestion) {
        agentPromptDraft = suggestion.fillText
        focusAgentPrompt()
    }

    func choose(_ suggestion: AgentSuggestion) {
        if suggestion.fillsOnly {
            agentPromptDraft = suggestion.fillText
            focusAgentPrompt()
        } else {
            agentPromptDraft = ""
            Task { await askAgent(suggestion.text) }
        }
    }

    /// Settings › Agents › Clear Suggestions History.
    func clearSuggestionHistory() {
        RecentPrompts(defaults: defaults).clear(accounts.map(\.id) + [openAccountID].compactMap { $0 })
        recentPromptsRevision += 1
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

    /// Accounts › Create an Account from an Archived Mailbox…: pick an .mbox
    /// file or a folder of them.
    func beginImport() async {
        let panel = NSOpenPanel()
        panel.title = "Create an Account from an Archived Mailbox"
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

    /// Re-read the backfill transport at most every few seconds.
    private func refreshTransport(force: Bool = false) {
        guard let core, let id = openAccountID, force || Date().timeIntervalSince(transportCheckedAt) > 5 else { return }
        transportCheckedAt = Date()
        Task {
            let diagnostics = await core.syncDiagnostics(id)
            guard openAccountID == id else { return }
            if backfillTransport != diagnostics.backfill.transport { backfillTransport = diagnostics.backfill.transport }
            let note = Self.transportNote(diagnostics)
            if transportNote != note { transportNote = note }
        }
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
        // Its agent session ends with it, before its store goes.
        agentStores[accountID]?.newConversation()
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

    /// Show a thread from a notification: switch to the Inbox, and to its
    /// category tab, and select it.
    func reveal(threadID: String) {
        if selectedMailboxID != "INBOX" { selectedMailboxID = "INBOX" }
        listFilters = []
        searchText = ""
        selectedThreadIDs = []
        selectedThreadID = threadID
        guard let core else { return }
        Task {
            guard let labels = try? await core.thread(threadID)?.thread.labelIds else { return }
            // Hidden by Important only: show the whole Inbox.
            if inboxImportantOnly, !labels.contains("IMPORTANT") { inboxImportantOnly = false }
            // New mail may be the first in its tab: count before choosing.
            await reloadInboxCategories()
            let tab = InboxCategories.category(of: labels, in: inboxCategoryCounts.map(\.id))
            if !inboxCategoryTabs.isEmpty, tab != activeInboxCategory { inboxCategory = tab }
            // Changing the narrowing clears the selection; this one is the point.
            selectedThreadID = threadID
        }
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

    /// In the task list, the reply answers the selected task: sending it
    /// completes the task (spec §14.8).
    func reply(all: Bool) {
        guard let id = replyTargetMessageID else { return }
        compose(.reply(messageID: id, all: all, task: answeringTaskID))
    }

    func forward() {
        guard let id = replyTargetMessageID else { return }
        compose(.forward(messageID: id, task: answeringTaskID))
    }

    // MARK: Actions on the selection

    /// The threads an action applies to: the multi-selection, else the one
    /// being read.
    var actionTargets: [String] {
        if !selectedThreadIDs.isEmpty { return threads.rows.map(\.id).filter(selectedThreadIDs.contains) }
        return selectedThreadID.map { [$0] } ?? []
    }

    /// Archive (or trash): rows leave at once and the next row is selected.
    func archiveSelection() {
        removeFromList(.archive) { core, ids in try await core.archive(ids) }
    }

    func trashSelection() {
        removeFromList(.trash) { core, ids in try await core.trash(ids) }
    }

    /// The Spam mailbox is on screen: the junk action is Not Junk there.
    var isSpamMailbox: Bool { selectedMailboxID == "SPAM" && threads.searchQuery == nil }

    /// Junk is for received mail: not offered in Sent or Drafts.
    var canJunk: Bool { !["SENT", "DRAFT"].contains(selectedMailboxID ?? "") }

    /// Mark as Junk, or Not Junk in Spam (spec §14.3 amendment, junk).
    func toggleJunkSelection() {
        guard canJunk else { return }
        if isSpamMailbox {
            removeFromList(.notJunk) { core, ids in try await core.notJunk(ids) }
        } else {
            removeFromList(.junk) { core, ids in try await core.markJunk(ids) }
        }
    }

    func moveSelectionToInbox() {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        if selectedMailboxID != "INBOX" { dropFromList(ids) }
        Task { await perform(UndoableAction(kind: .moveToInbox, count: ids.count)) { try await core.moveToInbox(ids) } }
    }

    /// Toggle read: if any target is unread, mark all read; else all unread.
    func toggleReadSelection() {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        let anyUnread = threads.rows.contains { ids.contains($0.id) && $0.unreadCount > 0 }
        threads.optimisticallyUpdate(Set(ids)) { $0.unreadCount = anyUnread ? 0 : max($0.unreadCount, 1) }
        let action = UndoableAction(kind: anyUnread ? .read : .unread, count: ids.count)
        Task { await perform(action) { try await core.setRead(ids, anyUnread) } }
    }

    func toggleStarSelection() {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        let allStarred = threads.rows.filter { ids.contains($0.id) }.allSatisfy(\.isStarred)
        threads.optimisticallyUpdate(Set(ids)) { $0.isStarred = !allStarred }
        if selectedMailboxID == "STARRED", allStarred { dropFromList(ids) }
        let action = UndoableAction(kind: allStarred ? .unstar : .star, count: ids.count)
        Task { await perform(action) { try await core.setStarred(ids, !allStarred) } }
    }

    func setLabel(_ labelID: String, applied: Bool) {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        if !applied, selectedMailboxID == labelID { dropFromList(ids) }
        let name = labelName(labelID)
        let action = UndoableAction(kind: applied ? .label(name) : .unlabel(name), count: ids.count)
        Task {
            await perform(action) {
                try await core.modifyLabels(ids, add: applied ? [labelID] : [], remove: applied ? [] : [labelID])
            }
        }
    }

    /// A label's last path segment, for the notice.
    private func labelName(_ labelID: String) -> String {
        mailboxes.mailboxes.first { $0.labelId == labelID }.map { LabelTree.leafName($0.name) } ?? "label"
    }

    // MARK: Undo (spec §14.6a)

    static let undoSendKey = "undoSendSeconds"
    static let undoSendChoices: [UInt32] = [0, 5, 10, 20, 30]

    /// Settings › General › Undo send: how long a send waits (0 = off).
    var undoSendSeconds: UInt32 = 10 {
        didSet {
            defaults.set(Int(undoSendSeconds), forKey: Self.undoSendKey)
            core?.setSendDelay(seconds: undoSendSeconds)
        }
    }

    /// A composer sent a message that is held: offer to take it back.
    /// Open a thread's draft in a composer (the Drafts mailbox: Edit
    /// Draft, a double-click or Return). A draft written elsewhere becomes
    /// a local draft the first time, attachments included; saving it
    /// updates the same draft on Gmail.
    /// Open threads in windows of their own (Return or double-click in the
    /// list); a draft opens in the composer instead. At most ten at once.
    func openThreads(_ ids: [String]? = nil) {
        guard let account = openAccountID else { return }
        let chosen = ids ?? (selectedThreadIDs.isEmpty ? selectedThreadID.map { [$0] } ?? [] : Array(selectedThreadIDs))
        let drafts = Set(threads.rows.filter { $0.labelIds.contains("DRAFT") }.map(\.id))
        for id in chosen.prefix(10) {
            if drafts.contains(id) || selectedMailboxID == "DRAFT" {
                editDraft(threadID: id)
            } else {
                openThreadWindow?(ThreadWindowRequest(accountID: account, threadID: id))
            }
        }
    }

    func editDraft(threadID: String? = nil) {
        guard let core, let threadID = threadID ?? selectedThreadID, let account = openAccountID else { return }
        Task {
            guard let detail = try? await core.thread(threadID),
                  let message = detail.messages.last(where: \.isDraft) else { return }
            do {
                let draft = try await core.openDraft(message.id, in: account)
                compose(.draft(id: draft.id))
            } catch let error as CoreClientError {
                logger.error("open draft failed: \(error.message, privacy: .private)")
                undo.show("Couldn't open the draft: \(error.message)", accountID: account)
            } catch {}
        }
    }

    /// An account's agent panel; an approved send it holds can be taken
    /// back from its card (spec §14.6a), reopening the draft for review.
    private func makeAgentStore(core: CoreClient, accountID: String) -> AgentStore {
        let store = AgentStore(core: core, defaults: defaults)
        store.heldUntil = { draftID in await core.sendHeldUntil(draftID, in: accountID) }
        store.takeBack = { [weak self, weak store] draftID in
            guard await core.cancelSend(draftID, in: accountID) else { return false }
            guard let self else { return true }
            if accountID != self.openAccountID { await self.switchAccount(to: accountID) }
            self.compose(.review(draftID: draftID, agent: store?.providerName ?? "Agent"))
            return true
        }
        return store
    }

    /// A message was sent (or is held for Undo Send). A reply a task
    /// called for asks whether that task is done (spec §14.8): the answer
    /// is the user's, in `taskDoneQuestion`.
    func messageSent(heldDraftID: Int64?, taskID: Int64?, accountID: String) async {
        if let heldDraftID { sendHeld(draftID: heldDraftID, accountID: accountID, taskID: taskID) }
        guard let taskID, let core, let task = try? await core.listTasks(includeDone: true).first(where: { $0.id == taskID }),
              !task.done
        else { return }
        taskDoneQuestion = TaskDoneQuestion(task: task, accountID: accountID, held: heldDraftID != nil)
    }

    /// The answer to "Mark the task done?": done (undoable), or kept open.
    func answerTaskDone(_ done: Bool) async {
        guard let question = taskDoneQuestion else { return }
        taskDoneQuestion = nil
        guard done, let core else { return }
        let task = question.task
        guard (try? await core.setTaskDone(task.id, true)) != nil else { return }
        await tasks.load()
        // While a send is held, its Undo Send notice stays on screen; ⌘Z
        // still reopens the task first.
        undo.record(accountID: question.accountID, actionName: "Complete Task",
                    noticeText: "Task done: “\(task.title)”", showNotice: !question.held,
                    undo: { _ = try? await core.setTaskDone(task.id, false) },
                    redo: { _ = try? await core.setTaskDone(task.id, true) })
    }

    func sendHeld(draftID: Int64, accountID: String, taskID: Int64? = nil) {
        guard let core else { return }
        undo.recordSend(accountID: accountID, holdFor: .seconds(Int(undoSendSeconds))) { [weak self] in
            guard let self else { return }
            if await core.cancelSend(draftID, in: accountID) {
                // Taken back: its task is not done after all.
                if let taskID { _ = try? await core.setTaskDone(taskID, false) }
                if self.taskDoneQuestion?.task.id == taskID { self.taskDoneQuestion = nil }
                if accountID != self.openAccountID { await self.switchAccount(to: accountID) }
                // Sending it again still answers the task.
                self.compose(.draft(id: draftID, task: taskID))
            } else {
                self.undo.show("Already sent", accountID: accountID)
            }
        }
    }

    /// Undo the open account's last mail action (the notice's button, and
    /// ⌘Z when no text is being edited).
    func undoMailAction() {
        undo.undo(in: openAccountID)
    }

    func redoMailAction() {
        undo.redo(in: openAccountID)
    }

    /// Whether ⌘Z belongs to text being edited: a text view is first
    /// responder in the key window (a field, the composer), or the key
    /// window is not the mail window.
    static func textOwnsUndo() -> Bool {
        guard let window = NSApp.keyWindow else { return false }
        return window.firstResponder is NSText || window.firstResponder is NSTextView
    }

    /// Edit › Undo: text keeps its own undo while it is being edited.
    func undoCommand(mailWindowKey: Bool) {
        if Self.textOwnsUndo() || !mailWindowKey {
            NSApp.sendAction(Selector(("undo:")), to: nil, from: nil)
        } else {
            undoMailAction()
        }
    }

    func redoCommand(mailWindowKey: Bool) {
        if Self.textOwnsUndo() || !mailWindowKey {
            NSApp.sendAction(Selector(("redo:")), to: nil, from: nil)
        } else {
            redoMailAction()
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
                if let token = try await core.modifyLabels(ids, add: [label.id], remove: []) {
                    undo.record(token, UndoableAction(kind: .label(LabelTree.leafName(label.name)), count: ids.count))
                }
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
        let action = UndoableAction(kind: .label(labelName(labelID)), count: ids.count)
        Task { await perform(action) { try await core.modifyLabels(ids, add: [labelID], remove: []) } }
    }

    func dismissFailedChanges() {
        guard let core else { return }
        Task { try? await core.clearFailedChanges() }
    }

    private func removeFromList(
        _ kind: UndoableAction.Kind,
        action: @escaping @Sendable (CoreClient, [String]) async throws -> UndoToken?
    ) {
        let ids = actionTargets
        guard let core, !ids.isEmpty else { return }
        dropFromList(ids)
        Task { await perform(UndoableAction(kind: kind, count: ids.count)) { try await action(core, ids) } }
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

    /// Run an action; if it changed anything, put it on the undo stack and
    /// acknowledge it.
    private func perform(_ action: UndoableAction, _ body: () async throws -> UndoToken?) async {
        do {
            if let token = try await body() { undo.record(token, action) }
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
    /// The account open when a sign-in began, to return to on cancel.
    @ObservationIgnored private var accountBeforeSignIn: String?
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

        if isTaskList { Task { await tasks.load() } }
        if isGuide { Task { await guide.load() } }
        guard case .open = accountState, let id = listMailboxID else { return }
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

    /// Agent sessions that report somewhere other than the agent panel (a
    /// composer's writing help), by session id.
    @ObservationIgnored var agentSinks: [String: @MainActor ([AgentEventInfo]) async -> Void] = [:]

    private func handle(_ tagged: CoreClientEvent.Tagged) async {
        if case let .agent(sessionID, events) = tagged.event, let sink = agentSinks[sessionID] {
            await sink(events)
            return
        }
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
            // A category tab may have gained its first thread or lost its
            // last: then the Inbox shows another narrowing.
            if selectedMailboxID == "INBOX", mailboxID == "INBOX" || mailboxID.hasPrefix("CATEGORY_"),
               await reloadInboxCategories(), threads.searchQuery == nil, let id = listMailboxID {
                await threads.show(mailboxID: id)
            } else if mailboxID == threads.mailboxID || threads.searchQuery != nil {
                await threads.apply(hint)
            } else if let shown = threads.mailboxID, shown.split(separator: "+").contains(Substring(mailboxID)) {
                // A narrowed view (INBOX+IMPORTANT): a change to either
                // label may move a thread in or out; re-query it.
                await threads.apply(ThreadChangeHint(invalidate: true))
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
        case let .syncStatus(state, pending, headers):
            refreshTransport()
            switch state {
            case .idle:
                syncDisplay = .idle
                await checkGuideInvite()
            case .bootstrapping, .syncing:
                syncDisplay = pending + headers > 0 || state == .bootstrapping
                    ? .syncing(pending: pending, headers: headers) : .idle
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
        case .guideChanged:
            guideRevision += 1
            await guide.load()
        case let .guideProgress(progress):
            let finished = guideProgress?.run?.status == .running && progress.run?.status == .done
            guideProgress = progress
            if finished {
                await guide.load()
                let waiting = Int(progress.decisionsTotal) - Int(progress.decisionsDone)
                notifier.announceGuide(decisions: waiting, accountID: tagged.accountID)
                if waiting > 0, guideSheet == nil, !(isGuide && guide.showsDecisions) {
                    guidePrompt = .finished(decisions: waiting)
                }
            }
        case .tasksChanged:
            tasksRevision += 1
            await tasks.load()
            // The first task made the label: the Inbox can now hide by it.
            let label = try? await core?.taskLabelID()
            if label != taskLabelID {
                taskLabelID = label
                if inboxHidesTasks { relist() }
            }
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
