import Foundation
import OpenAGCCore

/// The compose calls a composer makes, pinned to the account it opened on
/// (spec §7.7): switching the main window to another account must not make
/// an open composer save or send as that account.
struct DraftClient: Sendable {
    let core: CoreClient
    /// `nil` follows the window's current account (tests, the demo).
    private let pinned: AccountComposer?

    init(core: CoreClient, account: String?) {
        self.core = core
        pinned = account.map { core.composer(for: $0) }
    }

    var accountID: String? { pinned?.accountId() }

    func accountAddress() async throws(CoreClientError) -> String {
        guard let pinned else { return try await core.accountAddress() }
        return try await CoreClient.bridge { try await pinned.accountAddress() }
    }

    func replyDraft(to messageID: String, all: Bool) async throws(CoreClientError) -> DraftInfo {
        guard let pinned else { return try await core.replyDraft(to: messageID, all: all) }
        return try await CoreClient.bridge { try await pinned.replyDraft(messageId: messageID, replyAll: all) }
    }

    func forwardDraft(of messageID: String) async throws(CoreClientError) -> DraftInfo {
        guard let pinned else { return try await core.forwardDraft(of: messageID) }
        return try await CoreClient.bridge { try await pinned.forwardDraft(messageId: messageID) }
    }

    func draft(_ id: Int64) async throws(CoreClientError) -> DraftInfo? {
        guard let pinned else { return try await core.draft(id) }
        return try await CoreClient.bridge { try await pinned.getDraft(id: id) }
    }

    func saveDraft(_ draft: DraftInfo) async throws(CoreClientError) -> Int64 {
        guard let pinned else { return try await core.saveDraft(draft) }
        return try await CoreClient.bridge { try await pinned.saveDraft(draft: draft) }
    }

    func sendDraft(_ id: Int64) async throws(CoreClientError) {
        guard let pinned else { return try await core.sendDraft(id) }
        try await CoreClient.bridge { try await pinned.sendDraft(id: id) }
    }

    func deleteDraft(_ id: Int64) async throws(CoreClientError) {
        guard let pinned else { return try await core.deleteDraft(id) }
        try await CoreClient.bridge { try await pinned.deleteDraft(id: id) }
    }

    func flushDrafts() {
        if let pinned { pinned.flushDrafts() } else { core.flushDrafts() }
    }

    func suggestContactsNow(_ text: String, limit: UInt32 = 8) -> [AddressInfo] {
        pinned?.suggestContactsNow(text: text, limit: limit) ?? core.suggestContactsNow(text, limit: limit)
    }
}
