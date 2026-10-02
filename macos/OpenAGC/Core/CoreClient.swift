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
    /// Core events with the account each is about (`nil`: app-wide).
    let events: AsyncStream<CoreClientEvent.Tagged>

    convenience init(dataDirectory: URL, logDirectory: URL? = nil,
                     secrets: KeychainSecretStore = CoreClient.defaultSecrets()) throws(CoreClientError) {
        try self.init(dataDirectoryPath: dataDirectory.path, logDirectoryPath: logDirectory?.path, secrets: secrets)
    }

    init(dataDirectoryPath: String, logDirectoryPath: String? = nil,
         secrets: KeychainSecretStore = CoreClient.defaultSecrets()) throws(CoreClientError) {
        let (stream, continuation) = AsyncStream.makeStream(of: CoreClientEvent.Tagged.self, bufferingPolicy: .unbounded)
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

    /// The app's Keychain items, or a separate service when hosting tests
    /// or on a scratch data directory, so neither can read the user's
    /// sign-ins. Everything that touches the Keychain defaults to this.
    static func defaultSecrets() -> KeychainSecretStore {
        if isRunningTests { return KeychainSecretStore(service: testSecretsService) }
        return KeychainSecretStore(service: isScratchRun ? "ai.actual.openagc.scratch" : "ai.actual.openagc")
    }

    /// The test host's Keychain service; emptied when the host starts and
    /// quits (`AppDelegate`).
    static let testSecretsService = "ai.actual.openagc.tests"

    /// Where the app's tests put scratch data: one directory per test-host
    /// process, which scripts/test-macos.sh removes after the run (and the
    /// sweeper after an hour), so tests need not clean up one by one.
    static let testScratchRoot = FileManager.default.temporaryDirectory
        .appending(path: "openagc-apptests-\(ProcessInfo.processInfo.processIdentifier)", directoryHint: .isDirectory)

    /// A fresh scratch directory for a test.
    static func testScratch() -> URL {
        testScratchRoot.appending(path: UUID().uuidString, directoryHint: .isDirectory)
    }

    /// The app's preferences, or a throwaway suite when the app is hosting
    /// tests or running on a scratch data directory (snapshots,
    /// automation): those must never write the real app's preferences
    /// (the test host and snapshots share its bundle id).
    /// One suite per process, shared by everything that remembers a
    /// setting, so a test run or snapshot is consistent with itself.
    /// Launch arguments (`-OpenAGC…`) are still read from `.standard`:
    /// reading the argument domain writes nothing.
    static func appDefaults() -> UserDefaults { sharedDefaults }

    nonisolated(unsafe) private static let sharedDefaults: UserDefaults = {
        guard isRunningTests || isScratchRun else { return .standard }
        return UserDefaults(suiteName: "openagc-scratch-\(UUID().uuidString)") ?? .standard
    }()

    /// Pointed at a throwaway data directory (snapshots, automation).
    static var isScratchRun: Bool {
        !(UserDefaults.standard.string(forKey: "OpenAGCDataDirectory") ?? "").isEmpty
    }

    static var isRunningTests: Bool {
        let env = ProcessInfo.processInfo.environment
        return env["XCTestConfigurationFilePath"] != nil || env["XCTestBundlePath"] != nil
            || env["XCTestSessionIdentifier"] != nil || NSClassFromString("XCTestCase") != nil
    }

    var version: String { core.version() }
    /// Where this core keeps its accounts.
    var dataDirectory: String { core.dataDir() }

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

    /// The user's accounts in their order (spec §7.7).
    func accounts() async throws(CoreClientError) -> [AccountSummary] {
        try await call { try await core.listAccounts() }
    }

    func setCurrentAccount(_ accountID: String) async throws(CoreClientError) {
        try await call { try await core.setCurrentAccount(accountId: accountID) }
    }

    func removeAccount(_ accountID: String) async throws(CoreClientError) {
        try await call { try await core.removeAccount(accountId: accountID) }
    }

    // MARK: Archive accounts (spec §7.8)

    func scanMailbox(_ path: String) async throws(CoreClientError) -> MailboxScan {
        try await call { try await core.scanMailbox(path: path) }
    }

    /// Start importing; returns the archive account's id. Progress arrives
    /// as `importProgress` events tagged with it.
    func startImport(path: String, name: String, myAddresses: [String], into accountID: String? = nil) async throws(CoreClientError) -> String {
        try await call { try await core.startImport(path: path, name: name, myAddresses: myAddresses, accountId: accountID) }
    }

    func reimportArchive(_ accountID: String) async throws(CoreClientError) -> String {
        try await call { try await core.reimportArchive(accountId: accountID) }
    }

    func cancelImport(_ accountID: String) { core.cancelImport(accountId: accountID) }

    /// Ask Gmail for `query` and download missing matches; returns how many
    /// arrived (0 for accounts without a server).
    func searchServer(_ query: String, limit: UInt32) async throws(CoreClientError) -> UInt32 {
        try await call { try await core.searchServer(query: query, limit: limit) }
    }

    /// Some mail is stored with headers only (tiered download).
    func hasHeaderOnlyMail() async -> Bool {
        (try? await call { try await core.hasHeaderOnlyMail() }) ?? false
    }

    /// Download header-only bodies now (tiered download, spec §7.4); on
    /// failure the core queues them first instead.
    func ensureBodies(_ ids: [String]) async {
        _ = try? await call { try await core.ensureBodies(messageIds: ids) }
    }

    /// Download these messages' bodies next (opened with headers only).
    func prioritizeMessages(_ ids: [String]) async {
        try? await call { try await core.prioritizeMessages(messageIds: ids) }
    }

    /// Re-download an account's mail so labels and bodies match Gmail.
    func refreshFromServer(_ accountID: String) async throws(CoreClientError) -> UInt64 {
        try await call { try await core.refreshFromServer(accountId: accountID) }
    }

    /// How an account's backfill fetches bodies ("rest", "imap", …).
    func backfillStatus(_ accountID: String) async -> BackfillStatus {
        await core.backfillStatus(accountId: accountID)
    }

    /// Which transport serves each sync job and why, the breaker, recent
    /// operations (the Sync Debugger, the sync footer).
    func syncDiagnostics(_ accountID: String) async -> SyncDiagnostics {
        await core.syncDiagnostics(accountId: accountID)
    }

    /// Time each sync job over IMAP and over the API (the Sync Debugger).
    /// Reads only; changes no mail.
    func compareTransports(_ accountID: String) async throws(CoreClientError) -> [TransportComparison] {
        try await call { try await core.compareTransports(accountId: accountID) }
    }

    func isArchive(_ accountID: String) -> Bool { core.accountIsArchive(accountId: accountID) }

    /// Development/test hook: a listed account with a synthetic mailbox and
    /// no sign-in.
    func addDemoAccount(_ accountID: String, email: String, name: String? = nil, threads: UInt32 = 60) async throws(CoreClientError) {
        try await call {
            try await core.debugAddDemoAccount(accountId: accountID, email: email, displayName: name, threads: threads)
        }
    }

    /// Account stores no listed account owns (spec §7.7).
    func orphanedStores() async throws(CoreClientError) -> [OrphanedStore] {
        try await call { try await core.orphanedStores() }
    }

    func removeOrphanedStore(_ id: String) async throws(CoreClientError) {
        try await call { try await core.removeOrphanedStore(accountId: id) }
    }

    /// Rename an account; for Gmail an empty name goes back to the profile's.
    func renameAccount(_ accountID: String, to name: String) async throws(CoreClientError) {
        try await call { try await core.renameAccount(accountId: accountID, name: name) }
    }

    func moveAccount(_ accountID: String, to position: Int) async throws(CoreClientError) {
        try await call { try await core.moveAccount(accountId: accountID, position: UInt32(max(0, position))) }
    }

    var currentAccountID: String? { core.currentAccountId() }

    func mailboxes() async throws(CoreClientError) -> [MailboxInfo] {
        try await call { try await core.listMailboxes() }
    }

    func labels() async throws(CoreClientError) -> [LabelInfo] {
        try await call { try await core.listLabels() }
    }

    /// Create a label; a `/` path creates missing parents.
    func createLabel(_ path: String, color: String? = nil) async throws(CoreClientError) -> LabelInfo {
        try await call { try await core.createLabel(name: path, color: color) }
    }

    /// The Inbox's category tabs with their counts, Primary first.
    func inboxCategories(importantOnly: Bool, hiddenLabel: String? = nil) async throws(CoreClientError) -> [InboxCategory] {
        try await call { try await core.inboxCategories(importantOnly: importantOnly, hiddenLabel: hiddenLabel) }
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

    // Each returns a token for undo (spec §14.6a), or nil if nothing changed.

    @discardableResult
    func archive(_ threadIDs: [String]) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.archive(threadIds: threadIDs) }
    }

    @discardableResult
    func moveToInbox(_ threadIDs: [String]) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.moveToInbox(threadIds: threadIDs) }
    }

    @discardableResult
    func setRead(_ threadIDs: [String], _ read: Bool) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.setRead(threadIds: threadIDs, read: read) }
    }

    @discardableResult
    func setStarred(_ threadIDs: [String], _ starred: Bool) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.setStarred(threadIds: threadIDs, starred: starred) }
    }

    @discardableResult
    func modifyLabels(_ threadIDs: [String], add: [String], remove: [String]) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.modifyLabels(threadIds: threadIDs, add: add, remove: remove) }
    }

    @discardableResult
    func trash(_ threadIDs: [String]) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.trash(threadIds: threadIDs) }
    }

    /// Mark as Junk: to Spam, out of the Inbox (spec §14.3 amendment, junk).
    func markJunk(_ threadIDs: [String]) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.markJunk(threadIds: threadIDs) }
    }

    /// Not Junk: out of Spam, into the Inbox.
    func notJunk(_ threadIDs: [String]) async throws(CoreClientError) -> UndoToken? {
        try await call { try await core.notJunk(threadIds: threadIDs) }
    }

    /// Reverse a recorded action exactly, in its own account.
    func undo(_ token: UndoToken) async throws(CoreClientError) {
        try await call { try await core.undoAction(token: token) }
    }

    func redo(_ token: UndoToken) async throws(CoreClientError) {
        try await call { try await core.redoAction(token: token) }
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

    /// The open account's own addresses (aliases too), lowercased.
    func ownAddresses() async -> Set<String> {
        Set((try? await call { try await core.ownAddresses() }) ?? [])
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

    /// Returns whether the send is held for Undo Send.
    @discardableResult
    func sendDraft(_ id: Int64) async throws(CoreClientError) -> Bool {
        try await call { try await core.sendDraft(id: id) }
    }

    /// Undo Send: take back a held send in `accountID`; false if it went.
    func cancelSend(_ draftID: Int64, in accountID: String) async -> Bool {
        let composer = composer(for: accountID)
        return (try? await CoreClient.bridge { try await composer.cancelSend(draftId: draftID) }) ?? false
    }

    /// The local draft that edits a draft from the Drafts mailbox (made on
    /// first open, with its attachments).
    func openDraft(_ messageID: String, in accountID: String) async throws(CoreClientError) -> DraftInfo {
        let composer = composer(for: accountID)
        return try await CoreClient.bridge { try await composer.openDraft(messageId: messageID) }
    }

    /// When the draft's send stops being held, if it is still held.
    func sendHeldUntil(_ draftID: Int64, in accountID: String) async -> Date? {
        let composer = composer(for: accountID)
        let until = try? await CoreClient.bridge { try await composer.sendHeldUntil(draftId: draftID) }
        return until.flatMap { $0 }.map { Date(timeIntervalSince1970: TimeInterval($0) / 1000) }
    }

    /// How long sends wait so they can be undone (0 = off).
    func setSendDelay(seconds: UInt32) { core.setSendDelay(seconds: seconds) }

    /// Quitting: send every held message now; waits up to `timeout` and
    /// returns how many are still going.
    func sendHeldNow(timeout: Duration) async -> UInt32 {
        await core.sendHeldNow(timeoutMs: UInt32(timeout.components.seconds * 1000))
    }

    func heldSendCount() async -> UInt32 { await core.heldSendCount() }

    /// Sends quitting should wait for (held, due or on their way),
    /// answered at once: the quit handler cannot wait to ask.
    func unsentSendCountNow() -> UInt32 { core.unsentSendCountNow() }

    /// Mirror edited drafts to Gmail now instead of at the next 30 s tick.
    func flushDrafts() { core.flushDrafts() }

    /// Synchronous, for the recipient token field's completion callback.
    func suggestContactsNow(_ text: String, limit: UInt32 = 8) -> [AddressInfo] {
        core.suggestContactsNow(text: text, limit: limit)
    }

    // MARK: Tasks (spec §14.8)

    func listTasks(includeDone: Bool = false) async throws(CoreClientError) -> [TaskItem] {
        try await call { try await core.listTasks(includeDone: includeDone) }
    }

    /// Adds the tasks and labels their threads `Task`.
    func createTasks(_ tasks: [NewTask]) async throws(CoreClientError) -> [TaskItem] {
        try await call { try await core.createTasks(new: tasks) }
    }

    func updateTask(_ id: Int64, _ edit: TaskEdit) async throws(CoreClientError) -> TaskItem {
        try await call { try await core.updateTask(id: id, edit: edit) }
    }

    func setTaskDone(_ id: Int64, _ done: Bool) async throws(CoreClientError) -> TaskItem {
        try await call { try await core.setTaskDone(id: id, done: done) }
    }

    /// Returns the task as it was, for `restoreTask` (undo).
    func deleteTask(_ id: Int64) async throws(CoreClientError) -> TaskItem {
        try await call { try await core.deleteTask(id: id) }
    }

    func restoreTask(_ task: TaskItem) async throws(CoreClientError) -> TaskItem {
        try await call { try await core.restoreTask(task: task) }
    }

    func threadsWithOpenTasks(_ threadIDs: [String]) async throws(CoreClientError) -> [String] {
        try await call { try await core.threadsWithOpenTasks(threadIds: threadIDs) }
    }

    func taskLabelID() async throws(CoreClientError) -> String? {
        try await call { try await core.taskLabelId() }
    }

    func taskCategories() async throws(CoreClientError) -> [String] {
        try await call { try await core.taskCategories() }
    }

    func setTaskCategories(_ names: [String]) async throws(CoreClientError) -> [String] {
        try await call { try await core.setTaskCategories(names: names) }
    }

    func resetTaskCategories() async throws(CoreClientError) -> [String] {
        try await call { try await core.resetTaskCategories() }
    }

    /// Claude's request for task suggestions about these threads, with
    /// `today` as `YYYY-MM-DD` in the user's calendar.
    func taskPrompt(_ threadIDs: [String], today: String) async throws(CoreClientError) -> String {
        try await call { try await core.taskPrompt(threadIds: threadIDs, today: today) }
    }

    func parseTaskSuggestions(_ text: String, threadIDs: [String]) async throws(CoreClientError) -> [TaskSuggestion] {
        try await call { try await core.parseTaskSuggestions(text: text, threadIds: threadIDs) }
    }

    // MARK: Writing guide (spec §14.9)

    func guideCategories() async throws(CoreClientError) -> [GuideCategoryInfo] {
        try await call { try await core.guideCategories() }
    }

    /// Entries with any of `statuses`, or every entry when empty.
    func guideEntries(_ statuses: [GuideStatus] = []) async throws(CoreClientError) -> [GuideEntry] {
        try await call { try await core.listGuideEntries(statuses: statuses) }
    }

    func guideEntry(_ id: Int64) async throws(CoreClientError) -> GuideEntry? {
        try await call { try await core.guideEntry(id: id) }
    }

    /// Edits applied as one change; its id undoes and redoes it.
    func applyGuideEdits(_ edits: [GuideEdit], reason: String) async throws(CoreClientError) -> GuideChange {
        try await call { try await core.applyGuideEdits(edits: edits, reason: reason) }
    }

    func undoGuideChange(_ id: Int64) async throws(CoreClientError) {
        try await call { try await core.undoGuideChange(changeId: id) }
    }

    func redoGuideChange(_ id: Int64) async throws(CoreClientError) {
        try await call { try await core.redoGuideChange(changeId: id) }
    }

    func guideVersion() async throws(CoreClientError) -> Int64 {
        try await call { try await core.guideVersion() }
    }

    func audienceGroups() async throws(CoreClientError) -> [AudienceGroup] {
        try await call { try await core.listAudienceGroups() }
    }

    func saveAudienceGroup(_ group: AudienceGroup) async throws(CoreClientError) -> [AudienceGroup] {
        try await call { try await core.saveAudienceGroup(group: group) }
    }

    func renameAudienceGroup(_ id: Int64, to name: String) async throws(CoreClientError) -> [AudienceGroup] {
        try await call { try await core.renameAudienceGroup(id: id, name: name) }
    }

    /// Returns the change that re-scoped entries, if any, for Undo.
    func mergeAudienceGroups(into: Int64, from: Int64) async throws(CoreClientError) -> Int64? {
        try await call { try await core.mergeAudienceGroups(into: into, from: from) }
    }

    /// Suggest groups for the obvious gaps until there are five.
    func fillAudienceGroups() async throws(CoreClientError) -> [AudienceGroup] {
        try await call { try await core.fillAudienceGroups() }
    }

    /// The confirmed groups these recipients belong to.
    func audienceFor(_ addresses: [String]) async throws(CoreClientError) -> [String] {
        try await call { try await core.audienceFor(addresses: addresses) }
    }

    func deleteAudienceGroup(_ id: Int64) async throws(CoreClientError) {
        try await call { try await core.deleteAudienceGroup(id: id) }
    }

    /// Markdown to read or share, or JSON to import or merge elsewhere.
    func exportGuide(json: Bool, withEvidence: Bool = false) async throws(CoreClientError) -> String {
        try await call { try await core.exportGuide(json: json, withEvidence: withEvidence) }
    }

    func readGuideExport(_ json: String) throws(CoreClientError) -> GuideImport {
        do { return try core.readGuideExport(json: json) } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
    }

    /// For the learning dialog: sent mail, analysed before, and what a run
    /// of `count` would analyse.
    func guideSampleInfo(count: UInt32, filter: GuideSampleFilter) async throws(CoreClientError) -> GuideSampleInfo {
        try await call { try await core.guideSampleInfo(count: count, filter: filter) }
    }

    /// The writing guide for a message being drafted (spec §14.9).
    func guideForMessage(recipients: [String], messageType: String?,
                         audiences: [String]?) async throws(CoreClientError) -> GuideRendered {
        try await call { try await core.guideForMessage(recipients: recipients, messageType: messageType, audiences: audiences) }
    }

    /// Check an AI draft's own text against the guide for its message.
    func checkGuideDraft(_ text: String, recipients: [String], messageType: String?,
                         audiences: [String]?) async throws(CoreClientError) -> [GuideCheckFailure] {
        try await call {
            try await core.checkGuideDraft(text: text, recipients: recipients, messageType: messageType, audiences: audiences)
        }
    }

    func setDraftGuideVersion(_ draftID: Int64, _ version: Int64) async throws(CoreClientError) {
        try await call { try await core.setDraftGuideVersion(draftId: draftID, version: version) }
    }

    func draftGuideVersion(_ draftID: Int64) async throws(CoreClientError) -> Int64? {
        try await call { try await core.draftGuideVersion(draftId: draftID) }
    }

    /// Ask the agent how to change the guide; nothing changes until the
    /// user answers the questions.
    func proposeGuideChange(_ request: String, agent: String) async throws(CoreClientError) -> [GuideChangeQuestion] {
        try await call { try await core.proposeGuideChange(request: request, agent: agent) }
    }

    /// What merging a guide from another account or a file would do.
    func planGuideMerge(fromAccount: String?, json: String?, agent: String) async throws(CoreClientError) -> GuideMergePlan {
        try await call { try await core.planGuideMerge(fromAccount: fromAccount, json: json, agent: agent) }
    }

    /// The signature block the analysis found in sent mail, if any.
    func guideSignature() async throws(CoreClientError) -> String? {
        try await call { try await core.guideSignature() }
    }

    /// How many messages a run of this request would analyse.
    func guideRunPreview(_ request: GuideRunRequest) async throws(CoreClientError) -> UInt32 {
        try await call { try await core.guideRunPreview(request: request) }
    }

    func startGuideRun(_ request: GuideRunRequest) async throws(CoreClientError) -> GuideRunInfo {
        try await call { try await core.startGuideRun(request: request) }
    }

    func pauseGuideRun() async throws(CoreClientError) {
        try await call { try await core.pauseGuideRun() }
    }

    /// Resume a paused run, or one the app quit in the middle of.
    @discardableResult
    func resumeGuideRun() async throws(CoreClientError) -> GuideRunInfo? {
        try await call { try await core.resumeGuideRun() }
    }

    func cancelGuideRun() async throws(CoreClientError) {
        try await call { try await core.cancelGuideRun() }
    }

    func guideProgress() async throws(CoreClientError) -> GuideProgress {
        try await call { try await core.guideProgress() }
    }

    /// Proposals ready to decide (from finished runs only).
    func guideDecisions() async throws(CoreClientError) -> [GuideEntry] {
        try await call { try await core.guideDecisions() }
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

    /// A session whose tools may only read: writing help and task
    /// suggestions, which must not change mail whatever the agent is told.
    func startReadOnlyAgentSession(provider: String, selection: [String]) async throws(CoreClientError) -> String {
        try await call { try await core.startReadOnlyAgentSession(provider: provider, selection: selection) }
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
    /// `fullAccess` also asks Google for full mail access, which faster
    /// download over IMAP needs (spec §7.4).
    func beginGmailSignIn(clientID: String, clientSecret: String?, loginHint: String? = nil,
                          fullAccess: Bool = false) async throws(CoreClientError) -> SignInStart {
        let start = try await call {
            try await core.beginGmailSignIn(client: OAuthClientConfig(clientId: clientID, clientSecret: clientSecret),
                                            loginHint: loginHint, fullAccess: fullAccess)
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

    func accountHasCredentials(_ accountID: String) throws(CoreClientError) -> Bool {
        do {
            return try core.accountHasCredentials(accountId: accountID)
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
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

    /// Start every signed-in account's sync in the background; returns the
    /// accounts whose sign-in could not be read (spec §7.7).
    func startAllSync() async throws(CoreClientError) -> [String] {
        try await call { try await core.startAllSync() }
    }

    func stopSync() { core.stopSync() }
    func setAppActive(_ active: Bool) { core.setAppActive(active: active) }
    func syncNow() { core.syncNow() }

    /// How far back mail is downloaded (spec §7.4).
    func syncWindow() async throws(CoreClientError) -> SyncWindow {
        try await call { try await core.syncWindow() }
    }

    func syncWindow(for accountID: String) async throws(CoreClientError) -> SyncWindow {
        try await call { try await core.syncWindowFor(accountId: accountID) }
    }

    func setSyncWindow(_ window: SyncWindow, for accountID: String) async throws(CoreClientError) {
        try await call { try await core.setSyncWindowFor(accountId: accountID, window: window) }
    }

    func setSyncWindow(_ window: SyncWindow) async throws(CoreClientError) {
        try await call { try await core.setSyncWindow(window: window) }
    }

    /// Which part of the download range gets full messages over IMAP.
    func bodyWindow(for accountID: String) async throws(CoreClientError) -> BodyWindow {
        try await call { try await core.bodyWindowFor(accountId: accountID) }
    }

    func setBodyWindow(_ window: BodyWindow, for accountID: String) async throws(CoreClientError) {
        try await call { try await core.setBodyWindowFor(accountId: accountID, bodyWindow: window) }
    }

    func signOut(_ accountID: String) async throws(CoreClientError) {
        try await call { try await core.signOut(accountId: accountID) }
    }

    /// Development hook: fill the open account with a synthetic mailbox.
    @discardableResult
    func seedDemoMailbox(threads: UInt32) async throws(CoreClientError) -> UInt32 {
        try await call { try await core.debugSeedDemoMailbox(threads: threads) }
    }

    /// Runs a core call, converting generated errors to `CoreClientError`.
    /// Compose operations pinned to one account (spec §7.7).
    nonisolated func composer(for accountID: String) -> AccountComposer {
        core.composerFor(accountId: accountID)
    }

    /// Map a core call's errors like `call` does, for handles other than
    /// `Core` (e.g. `AccountComposer`).
    static func bridge<T>(_ body: () async throws -> T) async throws(CoreClientError) -> T {
        do {
            return try await body()
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
    }

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
typealias InboxCategory = OpenAGCCore.InboxCategory
typealias SyncDiagnostics = OpenAGCCore.SyncDiagnostics
typealias TransportOp = OpenAGCCore.TransportOp
typealias TransportComparison = OpenAGCCore.TransportComparison
typealias MailboxInfo = OpenAGCCore.MailboxInfo
typealias SyncWindow = OpenAGCCore.SyncWindow
typealias BodyWindow = OpenAGCCore.BodyWindow
typealias UndoToken = OpenAGCCore.UndoToken
typealias AccountSummary = OpenAGCCore.AccountSummary
typealias AccountKind = OpenAGCCore.AccountKind
typealias ImportStatus = OpenAGCCore.ImportStatus
typealias BackfillStatus = OpenAGCCore.BackfillStatus
typealias OrphanedStore = OpenAGCCore.OrphanedStore
typealias MailboxScan = OpenAGCCore.MailboxScan
typealias MailboxKind = OpenAGCCore.MailboxKind
typealias MessageInfo = OpenAGCCore.MessageInfo
typealias RenderedBody = OpenAGCCore.RenderedBody
typealias ThreadDetail = OpenAGCCore.ThreadDetail
typealias ThreadPage = OpenAGCCore.ThreadPage
typealias ThreadRow = OpenAGCCore.ThreadRow
typealias TaskItem = OpenAGCCore.TaskItem
typealias NewTask = OpenAGCCore.NewTask
typealias TaskEdit = OpenAGCCore.TaskEdit
typealias TaskAction = OpenAGCCore.TaskAction
typealias TaskSuggestion = OpenAGCCore.TaskSuggestion
typealias GuideCategoryInfo = OpenAGCCore.GuideCategoryInfo
typealias GuideEntry = OpenAGCCore.GuideEntry
typealias GuideEntryFields = OpenAGCCore.GuideEntryFields
typealias GuideEdit = OpenAGCCore.GuideEdit
typealias GuideChange = OpenAGCCore.GuideChange
typealias GuideKind = OpenAGCCore.GuideKind
typealias GuideStatus = OpenAGCCore.GuideStatus
typealias GuideSource = OpenAGCCore.GuideSource
typealias GuideScope = OpenAGCCore.GuideScope
typealias GuideCheck = OpenAGCCore.GuideCheck
typealias GuideCheckKind = OpenAGCCore.GuideCheckKind
typealias GuideQuote = OpenAGCCore.GuideQuote
typealias GuideImport = OpenAGCCore.GuideImport
typealias AudienceGroup = OpenAGCCore.AudienceGroup
typealias AudienceStatus = OpenAGCCore.AudienceStatus
typealias GuideProgress = OpenAGCCore.GuideProgress
typealias GuideRunInfo = OpenAGCCore.GuideRunInfo
typealias GuideRunKind = OpenAGCCore.GuideRunKind
typealias GuideRunStatus = OpenAGCCore.GuideRunStatus
typealias GuideRunRequest = OpenAGCCore.GuideRunRequest
typealias GuideSampleFilter = OpenAGCCore.GuideSampleFilter
typealias GuideSampleInfo = OpenAGCCore.GuideSampleInfo
typealias GuideRendered = OpenAGCCore.GuideRendered
typealias GuideCheckFailure = OpenAGCCore.GuideCheckFailure
typealias GuideChangeQuestion = OpenAGCCore.GuideChangeQuestion
typealias GuideMergePlan = OpenAGCCore.GuideMergePlan
typealias GuideMergeDecision = OpenAGCCore.GuideMergeDecision

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
    /// An event and the account it is about (spec §7.7).
    struct Tagged: Sendable, Equatable {
        let accountID: String?
        let event: CoreClientEvent
    }

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
    case syncStatus(SyncState, pending: UInt32, headers: UInt32)
    case outboxStatus(pending: UInt32, failed: UInt32)
    case newMail([NewMail])
    case agent(sessionID: String, events: [AgentEventInfo])
    case routinesChanged
    case tasksChanged
    case guideChanged
    case guideProgress(GuideProgress)
    case importProgress(ImportStatus)
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
    private let continuation: AsyncStream<CoreClientEvent.Tagged>.Continuation

    init(_ continuation: AsyncStream<CoreClientEvent.Tagged>.Continuation) {
        self.continuation = continuation
    }

    func onEvent(accountId: String?, event: CoreEvent) {
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
            continuation.yield(.init(accountID: accountId, event: mapped))
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
        case let .syncStatus(state, pending, pendingHeaders):
            self = .syncStatus(SyncState(state), pending: pending, headers: pendingHeaders)
        case let .outboxStatus(pending, failed):
            self = .outboxStatus(pending: pending, failed: failed)
        case let .error(kind, message):
            self = .error(CoreClientError(kind: .init(kind), message: message))
        case .routinesChanged:
            self = .routinesChanged
        case .tasksChanged:
            self = .tasksChanged
        case .guideChanged:
            self = .guideChanged
        case let .guideProgress(progress):
            self = .guideProgress(progress)
        case let .agentEvents(sessionId, events):
            self = .agent(sessionID: sessionId, events: events)
        case let .newMail(messages):
            self = .newMail(messages.map {
                NewMail(messageID: $0.messageId, threadID: $0.threadId,
                        senderName: $0.from.map { $0.name ?? $0.email } ?? "Unknown sender",
                        subject: $0.subject, snippet: $0.snippet)
            })
        case let .importProgress(status):
            self = .importProgress(status)
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
