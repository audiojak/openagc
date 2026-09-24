import Foundation
import OpenAGCCore

/// The app's handle on the Rust core. This is the only file that imports
/// `OpenAGCCore` (spec §14.2); everything else talks to `CoreClient`.
final class CoreClient: Sendable {
    private let core: Core

    convenience init(dataDirectory: URL) throws(CoreClientError) {
        try self.init(dataDirectoryPath: dataDirectory.path)
    }

    init(dataDirectoryPath: String) throws(CoreClientError) {
        do {
            core = try Core(config: CoreConfig(dataDir: dataDirectoryPath))
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
