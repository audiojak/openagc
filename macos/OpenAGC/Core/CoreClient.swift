import Foundation
import OpenAGCCore
import os
import PDFKit

/// The app's handle on the Rust core. This is the only file that imports
/// `OpenAGCCore` (spec §14.2); everything else talks to `CoreClient`.
final class CoreClient: Sendable {
    private let core: Core

    /// Events from the core, already coalesced in Rust (spec §4.3). One
    /// consumer; stores fan out on the main actor.
    let events: AsyncStream<CoreClientEvent>

    convenience init(dataDirectory: URL, logDirectory: URL? = nil,
                     secrets: KeychainSecretStore = KeychainSecretStore()) throws(CoreClientError) {
        try self.init(dataDirectoryPath: dataDirectory.path, logDirectoryPath: logDirectory?.path, secrets: secrets)
    }

    init(dataDirectoryPath: String, logDirectoryPath: String? = nil,
         secrets: KeychainSecretStore = KeychainSecretStore()) throws(CoreClientError) {
        let (stream, continuation) = AsyncStream.makeStream(of: CoreClientEvent.self, bufferingPolicy: .unbounded)
        events = stream
        do {
            core = try Core(config: CoreConfig(dataDir: dataDirectoryPath, logDir: logDirectoryPath),
                            secrets: SecretBridge(secrets),
                            listener: EventBridge(continuation))
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
        core.setTextExtractor(extractor: PDFTextExtractor())
        let bundle = Bundle.main
        core.configureAgents(
            shimPath: bundle.bundleURL.appending(path: "Contents/MacOS/openagc-mcp").path,
            systemPromptPath: bundle.url(forResource: "agent-system-prompt", withExtension: "md")?.path ?? "")
        // Tests never run the user's real agent CLIs (which would use their
        // account); neither do UI runs that ask for fakes.
        if UserDefaults.standard.bool(forKey: "OpenAGCFakeAgents") || Self.isRunningTests {
            core.debugUseFakeAgents()
        }
    }

    static var isRunningTests: Bool {
        let env = ProcessInfo.processInfo.environment
        return env["XCTestConfigurationFilePath"] != nil || env["XCTestBundlePath"] != nil
            || env["XCTestSessionIdentifier"] != nil || NSClassFromString("XCTestCase") != nil
    }

    var version: String { core.version() }

    func ping(_ message: String) -> String {
        core.ping(message: message)
    }

    func pingAsync(_ message: String) async throws(CoreClientError) -> String {
        try await call { try await core.pingAsync(message: message) }
    }

    // MARK: Account and mail reads (SQLite only; never the network)

    func openAccount(_ accountID: String) async throws(CoreClientError) {
        try await call { try await core.openAccount(accountId: accountID) }
    }

    var currentAccountID: String? { core.currentAccountId() }

    func mailboxes() async throws(CoreClientError) -> [MailboxInfo] {
        try await call { try await core.listMailboxes() }
    }

    func labels() async throws(CoreClientError) -> [LabelInfo] {
        try await call { try await core.listLabels() }
    }

    func threads(in mailboxID: String, after cursor: String? = nil, limit: UInt32 = 100) async throws(CoreClientError) -> ThreadPage {
        try await call { try await core.listThreads(mailboxId: mailboxID, cursor: cursor, limit: limit) }
    }

    func search(_ query: String, after cursor: String? = nil, limit: UInt32 = 150) async throws(CoreClientError) -> ThreadPage {
        try await call { try await core.searchThreads(query: query, cursor: cursor, limit: limit) }
    }

    func thread(_ threadID: String) async throws(CoreClientError) -> ThreadDetail? {
        try await call { try await core.getThread(threadId: threadID) }
    }

    func renderedBody(_ messageID: String) async throws(CoreClientError) -> RenderedBody? {
        try await call { try await core.getRenderedBody(messageId: messageID) }
    }

    // MARK: Mutations (applied locally at once, then pushed to Gmail)

    func archive(_ threadIDs: [String]) async throws(CoreClientError) {
        try await call { try await core.archive(threadIds: threadIDs) }
    }

    func moveToInbox(_ threadIDs: [String]) async throws(CoreClientError) {
        try await call { try await core.moveToInbox(threadIds: threadIDs) }
    }

    func setRead(_ threadIDs: [String], _ read: Bool) async throws(CoreClientError) {
        try await call { try await core.setRead(threadIds: threadIDs, read: read) }
    }

    func setStarred(_ threadIDs: [String], _ starred: Bool) async throws(CoreClientError) {
        try await call { try await core.setStarred(threadIds: threadIDs, starred: starred) }
    }

    func modifyLabels(_ threadIDs: [String], add: [String], remove: [String]) async throws(CoreClientError) {
        try await call { try await core.modifyLabels(threadIds: threadIDs, add: add, remove: remove) }
    }

    func trash(_ threadIDs: [String]) async throws(CoreClientError) {
        try await call { try await core.trash(threadIds: threadIDs) }
    }

    func outboxStatus() async throws(CoreClientError) -> (pending: UInt32, failed: UInt32) {
        let status = try await call { try await core.outboxStatus() }
        return (status.pending, status.failed)
    }

    func clearFailedChanges() async throws(CoreClientError) {
        try await call { try await core.clearFailedChanges() }
    }

    // MARK: Attachments

    struct AttachmentFile: Sendable, Equatable {
        let url: URL
        let filename: String
        let mimeType: String
        let contentID: String?
    }

    /// The local copy of an attachment, downloaded first if needed. New
    /// downloads are quarantined like any browser download (spec §15.3), so
    /// Gatekeeper checks them before anything opens.
    func attachmentFile(_ attachmentID: String) async throws(CoreClientError) -> AttachmentFile {
        let info = try await call { try await core.attachmentFile(attachmentId: attachmentID) }
        let url = URL(filePath: info.path)
        if info.downloaded { Self.quarantine(url) }
        return AttachmentFile(url: url, filename: info.filename, mimeType: info.mimeType, contentID: info.contentId)
    }

    static func quarantine(_ url: URL) {
        var values = URLResourceValues()
        values.quarantineProperties = [
            kLSQuarantineAgentNameKey as String: "OpenAGC",
            kLSQuarantineTypeKey as String: kLSQuarantineTypeOtherAttachment as String,
        ]
        var url = url
        do {
            try url.setResourceValues(values)
        } catch {
            Logger(subsystem: "ai.actual.openagc", category: "attachments")
                .error("quarantine failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    // MARK: Drafts and sending

    func accountAddress() async throws(CoreClientError) -> String {
        try await call { try await core.accountAddress() }
    }

    func replyDraft(to messageID: String, all: Bool) async throws(CoreClientError) -> DraftInfo {
        try await call { try await core.replyDraft(messageId: messageID, replyAll: all) }
    }

    func forwardDraft(of messageID: String) async throws(CoreClientError) -> DraftInfo {
        try await call { try await core.forwardDraft(messageId: messageID) }
    }

    /// Insert or update a draft; returns its id.
    func saveDraft(_ draft: DraftInfo) async throws(CoreClientError) -> Int64 {
        try await call { try await core.saveDraft(draft: draft) }
    }

    func draft(_ id: Int64) async throws(CoreClientError) -> DraftInfo? {
        try await call { try await core.getDraft(id: id) }
    }

    func drafts() async throws(CoreClientError) -> [DraftInfo] {
        try await call { try await core.listDrafts() }
    }

    func deleteDraft(_ id: Int64) async throws(CoreClientError) {
        try await call { try await core.deleteDraft(id: id) }
    }

    func sendDraft(_ id: Int64) async throws(CoreClientError) {
        try await call { try await core.sendDraft(id: id) }
    }

    /// Mirror edited drafts to Gmail now instead of at the next 30 s tick.
    func flushDrafts() { core.flushDrafts() }

    /// Synchronous, for the recipient token field's completion callback.
    func suggestContactsNow(_ text: String, limit: UInt32 = 8) -> [AddressInfo] {
        core.suggestContactsNow(text: text, limit: limit)
    }

    // MARK: Agents

    func agentProviders(refresh: Bool = false) async -> [AgentProviderInfo] {
        await core.listAgentProviders(refresh: refresh)
    }

    /// Start a session; with `selection`, the agent sees only those threads.
    func startAgentSession(provider: String, selection: [String]? = nil,
                           resume: String? = nil) async throws(CoreClientError) -> String {
        try await call { try await core.startAgentSession(provider: provider, selection: selection, resume: resume) }
    }

    func sendAgentPrompt(_ sessionID: String, _ prompt: String,
                         context: PromptContextInfo = PromptContextInfo(mailboxId: nil, selectedThreadIds: [],
                                                                         searchQuery: nil)) async throws(CoreClientError) {
        try await call { try await core.sendAgentPrompt(sessionId: sessionID, prompt: prompt, context: context) }
    }

    func cancelAgentTurn(_ sessionID: String) async throws(CoreClientError) {
        try await call { try await core.cancelAgentTurn(sessionId: sessionID) }
    }

    func closeAgentSession(_ sessionID: String) async throws(CoreClientError) {
        try await call { try await core.closeAgentSession(sessionId: sessionID) }
    }

    func agentHistory(limit: UInt32 = 30) async throws(CoreClientError) -> [AgentSessionInfo] {
        try await call { try await core.listAgentHistory(limit: limit) }
    }

    func agentTranscript(_ sessionID: String) async throws(CoreClientError) -> [AgentTranscriptItem] {
        try await call { try await core.agentTranscript(sessionId: sessionID) }
    }

    /// Continue a stored conversation; returns its (unchanged) id.
    func resumeAgentSession(_ sessionID: String) async throws(CoreClientError) -> String {
        try await call { try await core.resumeAgentSession(sessionId: sessionID) }
    }

    /// Approve or reject an action the agent proposed.
    func resolveAgentAction(_ actionID: Int64, approve: Bool) throws(CoreClientError) {
        do {
            try core.resolveAgentAction(actionId: actionID, approve: approve)
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
    }

    func agentActions(limit: UInt32 = 200) async throws(CoreClientError) -> [AgentActionInfo] {
        try await call { try await core.listAgentActions(limit: limit) }
    }

    /// Reversible tools that need the user's approval (spec §10.3).
    func setAgentPolicy(_ approveTools: [String]) throws(CoreClientError) {
        do {
            try core.setAgentPolicy(approveTools: approveTools)
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
    }

    var agentPolicy: [String] { core.agentPolicy() }
    var configurableAgentTools: [String] { core.configurableAgentTools() }

    /// Development: scripted agents instead of the real CLIs.
    func useFakeAgents() { core.debugUseFakeAgents() }

    // MARK: Routines

    func routines() async throws(CoreClientError) -> [RoutineInfo] {
        try await call { try await core.listRoutines() }
    }

    func createRoutineFromTemplate(runner: String) async throws(CoreClientError) -> RoutineInfo {
        try await call { try await core.createRoutineFromTemplate(runner: runner) }
    }

    func saveRoutine(json: String) async throws(CoreClientError) -> RoutineInfo {
        try await call { try await core.saveRoutine(definitionJson: json) }
    }

    func deleteRoutine(_ id: String) async throws(CoreClientError) {
        try await call { try await core.deleteRoutine(id: id) }
    }

    func routinePrompt(_ id: String) async throws(CoreClientError) -> String {
        try await call { try await core.routinePrompt(id: id) }
    }

    func runRoutineNow(_ id: String) async throws(CoreClientError) -> String {
        try await call { try await core.runRoutineNow(id: id) }
    }

    func previewRoutine(_ id: String) async throws(CoreClientError) -> String {
        try await call { try await core.previewRoutine(id: id) }
    }

    func routinePreview(_ sessionID: String) -> [RoutinePreviewRow]? {
        core.routinePreview(sessionId: sessionID)
    }

    func routineRuns(_ id: String, limit: UInt32 = 20) async throws(CoreClientError) -> [RoutineRunInfo] {
        try await call { try await core.listRoutineRuns(id: id, limit: limit) }
    }

    /// Create or update the routine at claude.ai through the user's CLI.
    func publishRoutineToCloud(_ id: String) async throws(CoreClientError) -> RoutineInfo {
        try await call { try await core.publishRoutineToCloud(id: id) }
    }

    func setRoutineEnabled(_ id: String, _ enabled: Bool) async throws(CoreClientError) -> RoutineInfo {
        try await call { try await core.setRoutineEnabled(id: id, enabled: enabled) }
    }

    func runCloudRoutineNow(_ id: String) async throws(CoreClientError) -> String? {
        try await call { try await core.runCloudRoutineNow(id: id) }
    }

    func refreshCloudRuns(_ id: String) async throws(CoreClientError) {
        try await call { try await core.refreshCloudRuns(id: id) }
    }

    func routineHandoff(_ id: String) async throws(CoreClientError) -> RoutineHandoff {
        try await call { try await core.routineHandoff(id: id) }
    }

    func attachCloudRoutine(_ id: String, urlOrID: String) async throws(CoreClientError) -> RoutineInfo {
        try await call { try await core.attachCloudRoutine(id: id, urlOrId: urlOrID) }
    }

    /// Put a run's threads back in the inbox; returns how many.
    func undoRoutineRun(_ runID: Int64) async throws(CoreClientError) -> UInt32 {
        try await call { try await core.undoRoutineRun(runId: runID) }
    }

    func describeSchedule(_ rrule: String) -> String { core.describeSchedule(rrule: rrule) }
    func nextRunAt(_ rrule: String) -> Int64? { core.nextRunAt(rrule: rrule) }

    // MARK: Accounts and sync

    struct SignInStart: Sendable {
        let sessionID: String
        let authorizationURL: URL
    }

    struct ConnectedAccount: Sendable, Equatable {
        let accountID: String
        let email: String
    }

    /// Start Gmail sign-in; open the returned URL in the user's browser.
    func beginGmailSignIn(clientID: String, clientSecret: String?, loginHint: String? = nil) async throws(CoreClientError) -> SignInStart {
        let start = try await call {
            try await core.beginGmailSignIn(client: OAuthClientConfig(clientId: clientID, clientSecret: clientSecret),
                                            loginHint: loginHint)
        }
        guard let url = URL(string: start.authorizationUrl) else {
            throw CoreClientError(kind: .internalError, message: "invalid authorization URL")
        }
        return SignInStart(sessionID: start.sessionId, authorizationURL: url)
    }

    /// Wait for the browser to finish and create the account.
    func completeGmailSignIn(_ sessionID: String) async throws(CoreClientError) -> ConnectedAccount {
        let account = try await call { try await core.completeGmailSignIn(sessionId: sessionID) }
        return ConnectedAccount(accountID: account.accountId, email: account.email)
    }

    func cancelGmailSignIn(_ sessionID: String) {
        core.cancelGmailSignIn(sessionId: sessionID)
    }

    func accountHasCredentials(_ accountID: String) -> Bool {
        core.accountHasCredentials(accountId: accountID)
    }

    func startSync() throws(CoreClientError) {
        do {
            try core.startSync()
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
    }

    func stopSync() { core.stopSync() }
    func setAppActive(_ active: Bool) { core.setAppActive(active: active) }
    func syncNow() { core.syncNow() }

    func signOut(_ accountID: String) async throws(CoreClientError) {
        try await call { try await core.signOut(accountId: accountID) }
    }

    /// Development hook: fill the open account with a synthetic mailbox.
    @discardableResult
    func seedDemoMailbox(threads: UInt32) async throws(CoreClientError) -> UInt32 {
        try await call { try await core.debugSeedDemoMailbox(threads: threads) }
    }

    /// Runs a core call, converting generated errors to `CoreClientError`.
    private func call<T>(_ body: () async throws -> T) async throws(CoreClientError) -> T {
        do {
            return try await body()
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
    }

    /// Diagnostics hook: ask Rust to emit one ThreadsChanged per id.
    func debugEmitThreadsChanged(mailboxID: String, threadIDs: [String]) {
        core.debugEmitThreadsChanged(mailboxId: mailboxID, threadIds: threadIDs)
    }

    /// `~/Library/Logs/OpenAGC`, where the core writes `core.log`.
    static func defaultLogDirectory() -> URL {
        URL.libraryDirectory.appending(path: "Logs/OpenAGC", directoryHint: .isDirectory)
    }

    /// `~/Library/Application Support/OpenAGC`, created if missing.
    static func defaultDataDirectory() throws -> URL {
        let dir = URL.applicationSupportDirectory.appending(path: "OpenAGC", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}

/// Core errors as the app sees them, without exposing generated types.
struct CoreClientError: Error, Equatable {
    enum Kind: Equatable {
        case invalidInput, notFound, storage, network, auth, rateLimited
        case agent, permissionDenied, cancelled, internalError
    }

    let kind: Kind
    let message: String

    init(kind: Kind, message: String) {
        self.kind = kind
        self.message = message
    }

    fileprivate init(_ error: CoreError) {
        switch error {
        case let .Failed(kind, message):
            self.init(kind: Kind(kind), message: message)
        }
    }
}

private extension CoreClientError.Kind {
    init(_ kind: ErrorKind) {
        switch kind {
        case .invalidInput: self = .invalidInput
        case .notFound: self = .notFound
        case .storage: self = .storage
        case .network: self = .network
        case .auth: self = .auth
        case .rateLimited: self = .rateLimited
        case .agent: self = .agent
        case .permissionDenied: self = .permissionDenied
        case .cancelled: self = .cancelled
        case .internal: self = .internalError
        }
    }
}

// MARK: - Data records

// Plain data records generated from Rust (spec §4.2). Aliased here so the
// rest of the app can use them without importing OpenAGCCore; all calls
// into the core still go through CoreClient.
typealias AddressInfo = OpenAGCCore.AddressInfo
typealias AgentActionInfo = OpenAGCCore.AgentActionInfo
typealias AgentEventInfo = OpenAGCCore.AgentEventInfo
typealias AgentProviderInfo = OpenAGCCore.AgentProviderInfo
typealias AgentSessionInfo = OpenAGCCore.AgentSessionInfo
typealias AgentTranscriptItem = OpenAGCCore.AgentTranscriptItem
typealias AgentStatusInfo = OpenAGCCore.AgentStatusInfo
typealias AttachmentInfo = OpenAGCCore.AttachmentInfo
typealias PromptContextInfo = OpenAGCCore.PromptContextInfo
typealias RoutineHandoff = OpenAGCCore.RoutineHandoff
typealias RoutineInfo = OpenAGCCore.RoutineInfo
typealias RoutinePreviewRow = OpenAGCCore.RoutinePreviewRow
typealias RoutineRunInfo = OpenAGCCore.RoutineRunInfo
typealias DraftAttachmentInfo = OpenAGCCore.DraftAttachmentInfo
typealias DraftInfo = OpenAGCCore.DraftInfo
typealias DraftStatus = OpenAGCCore.DraftStatus
typealias LabelInfo = OpenAGCCore.LabelInfo
typealias MailboxInfo = OpenAGCCore.MailboxInfo
typealias MailboxKind = OpenAGCCore.MailboxKind
typealias MessageInfo = OpenAGCCore.MessageInfo
typealias RenderedBody = OpenAGCCore.RenderedBody
typealias ThreadDetail = OpenAGCCore.ThreadDetail
typealias ThreadPage = OpenAGCCore.ThreadPage
typealias ThreadRow = OpenAGCCore.ThreadRow

// MARK: - Events

/// What changed in a mailbox's thread list (spec §4.3).
struct ThreadChangeHint: Sendable, Equatable {
    var inserted: [String] = []
    var updated: [String] = []
    var removed: [String] = []
    /// Too much changed to describe; re-query the visible window.
    var invalidate = false
}

enum CoreClientEvent: Sendable, Equatable {
    enum SyncState: Sendable, Equatable { case idle, bootstrapping, syncing, offline, error }

    /// A message that just arrived, unread in the Inbox.
    struct NewMail: Sendable, Equatable {
        let messageID: String
        let threadID: String
        let senderName: String
        let subject: String
        let snippet: String
    }

    case threadsChanged(mailboxID: String, hint: ThreadChangeHint)
    case syncStatus(SyncState, pending: UInt32)
    case outboxStatus(pending: UInt32, failed: UInt32)
    case newMail([NewMail])
    case agent(sessionID: String, events: [AgentEventInfo])
    case routinesChanged
    case error(CoreClientError)
}

/// Rust's view of the Keychain (spec §12). Errors cross as `CoreError`.
private final class SecretBridge: SecretStore, Sendable {
    private let keychain: KeychainSecretStore

    init(_ keychain: KeychainSecretStore) {
        self.keychain = keychain
    }

    func get(key: String) throws -> String? {
        do { return try keychain.get(key) } catch { throw CoreError.Failed(kind: .storage, message: error.description) }
    }

    func set(key: String, value: String) throws {
        do { try keychain.set(key, value) } catch { throw CoreError.Failed(kind: .storage, message: error.description) }
    }

    func delete(key: String) throws {
        do { try keychain.delete(key) } catch { throw CoreError.Failed(kind: .storage, message: error.description) }
    }
}

/// PDF text for agents (spec §10.2), through PDFKit rather than a Rust
/// parser. Called on a core worker thread.
final class PDFTextExtractor: TextExtractor, Sendable {
    func pdfText(path: String) -> String? {
        guard let document = PDFDocument(url: URL(filePath: path)), !document.isLocked else { return nil }
        let text = document.string?.trimmingCharacters(in: .whitespacesAndNewlines)
        return text?.isEmpty == false ? text : nil
    }
}

/// Receives events on a Rust runtime thread and hands them to the stream.
private final class EventBridge: EventListener, Sendable {
    private let continuation: AsyncStream<CoreClientEvent>.Continuation

    init(_ continuation: AsyncStream<CoreClientEvent>.Continuation) {
        self.continuation = continuation
    }

    func onEvent(event: CoreEvent) {
        // Rust warn/error records are logged here rather than delivered to
        // stores; Swift owns unified-logging privacy (spec §17). Rust has
        // already kept secrets and mail content out, and scrubbed addresses
        // and tokens (logging::scrub), so they can be public. Messages from
        // core *errors* can quote user data and are logged `.private`.
        if case let .log(level, target, message) = event {
            let logger = Logger(subsystem: "ai.actual.openagc", category: target)
            switch level {
            case .warn: logger.warning("\(message, privacy: .public)")
            case .error: logger.error("\(message, privacy: .public)")
            }
            return
        }
        if let mapped = CoreClientEvent(event) {
            continuation.yield(mapped)
        }
    }
}

private extension CoreClientEvent {
    /// `nil` for events handled inside the bridge (log records).
    init?(_ event: CoreEvent) {
        switch event {
        case let .threadsChanged(mailboxId, hint):
            self = .threadsChanged(mailboxID: mailboxId, hint: ThreadChangeHint(
                inserted: hint.inserted, updated: hint.updated,
                removed: hint.removed, invalidate: hint.invalidate))
        case let .syncStatus(state, pending):
            self = .syncStatus(SyncState(state), pending: pending)
        case let .outboxStatus(pending, failed):
            self = .outboxStatus(pending: pending, failed: failed)
        case let .error(kind, message):
            self = .error(CoreClientError(kind: .init(kind), message: message))
        case .routinesChanged:
            self = .routinesChanged
        case let .agentEvents(sessionId, events):
            self = .agent(sessionID: sessionId, events: events)
        case let .newMail(messages):
            self = .newMail(messages.map {
                NewMail(messageID: $0.messageId, threadID: $0.threadId,
                        senderName: $0.from.map { $0.name ?? $0.email } ?? "Unknown sender",
                        subject: $0.subject, snippet: $0.snippet)
            })
        case .log:
            return nil
        }
    }
}

private extension CoreClientEvent.SyncState {
    init(_ state: OpenAGCCore.SyncState) {
        switch state {
        case .idle: self = .idle
        case .bootstrapping: self = .bootstrapping
        case .syncing: self = .syncing
        case .offline: self = .offline
        case .error: self = .error
        }
    }
}
