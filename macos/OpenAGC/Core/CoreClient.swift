import Foundation
import OpenAGCCore
import os

/// The app's handle on the Rust core. This is the only file that imports
/// `OpenAGCCore` (spec §14.2); everything else talks to `CoreClient`.
final class CoreClient: Sendable {
    private let core: Core

    /// Events from the core, already coalesced in Rust (spec §4.3). One
    /// consumer; stores fan out on the main actor.
    let events: AsyncStream<CoreClientEvent>

    convenience init(dataDirectory: URL, logDirectory: URL? = nil) throws(CoreClientError) {
        try self.init(dataDirectoryPath: dataDirectory.path, logDirectoryPath: logDirectory?.path)
    }

    init(dataDirectoryPath: String, logDirectoryPath: String? = nil) throws(CoreClientError) {
        let (stream, continuation) = AsyncStream.makeStream(of: CoreClientEvent.self, bufferingPolicy: .unbounded)
        events = stream
        do {
            core = try Core(config: CoreConfig(dataDir: dataDirectoryPath, logDir: logDirectoryPath),
                            listener: EventBridge(continuation))
        } catch let error as CoreError {
            throw CoreClientError(error)
        } catch {
            throw CoreClientError(kind: .internalError, message: String(describing: error))
        }
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

    func thread(_ threadID: String) async throws(CoreClientError) -> ThreadDetail? {
        try await call { try await core.getThread(threadId: threadID) }
    }

    func renderedBody(_ messageID: String) async throws(CoreClientError) -> RenderedBody? {
        try await call { try await core.getRenderedBody(messageId: messageID) }
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
typealias AttachmentInfo = OpenAGCCore.AttachmentInfo
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

    case threadsChanged(mailboxID: String, hint: ThreadChangeHint)
    case syncStatus(SyncState, pending: UInt32)
    case outboxStatus(pending: UInt32, failed: UInt32)
    case error(CoreClientError)
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
        // already kept secrets and mail content out of these messages.
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
