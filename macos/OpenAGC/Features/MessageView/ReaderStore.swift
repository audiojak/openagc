import Foundation
import Observation

/// What the reader shows: the selected thread, its message bodies, and the
/// remote-image decision. Bodies are cached (LRU) so revisiting a thread
/// renders without touching the core (spec §13 rule 8).
@MainActor
@Observable
final class ReaderStore {
    static let cacheLimit = 200
    static let allowedSendersKey = "remoteImagesAllowedSenders"

    private(set) var threadID: String?
    private(set) var detail: ThreadDetail?
    private(set) var bodies: [String: RenderedBody] = [:]
    /// Inline (`cid:`) images for the thread, by content id.
    private(set) var inlineImages: [String: InlineImage] = [:]
    /// Remote images were requested for this thread in this session.
    private(set) var remoteImagesAllowedForThread = false

    @ObservationIgnored private let core: CoreClient?
    @ObservationIgnored private var cache: [String: RenderedBody] = [:]
    @ObservationIgnored private var cacheOrder: [String] = []
    @ObservationIgnored private var loadGeneration = 0

    init(core: CoreClient?) {
        self.core = core
    }

    var hasRemoteImages: Bool { bodies.values.contains { $0.hasRemoteImages } }

    /// Remote images load if allowed for this thread or for every sender in it.
    var allowsRemoteImages: Bool {
        if remoteImagesAllowedForThread { return true }
        guard let detail else { return false }
        let allowed = Set(UserDefaults.standard.stringArray(forKey: Self.allowedSendersKey) ?? [])
        let senders = detail.messages.compactMap { $0.from?.email.lowercased() }
        return !senders.isEmpty && senders.allSatisfy(allowed.contains)
    }

    func show(threadID: String?) async {
        guard threadID != self.threadID else { return }
        self.threadID = threadID
        remoteImagesAllowedForThread = false
        inlineImages = [:]
        loadGeneration += 1
        let generation = loadGeneration
        guard let threadID, let core else {
            detail = nil
            bodies = [:]
            return
        }
        guard let loaded = try? await core.thread(threadID), generation == loadGeneration else { return }
        // Show cached bodies immediately, then fill the rest.
        detail = loaded
        bodies = loaded.messages.reduce(into: [:]) { acc, m in
            if let cached = cache[m.id] { acc[m.id] = cached }
        }
        for message in loaded.messages where bodies[message.id] == nil && message.hasBody {
            // `try?` flattens: nil means an error or no body yet.
            guard let body = try? await core.renderedBody(message.id), generation == loadGeneration else { continue }
            remember(body)
            bodies[message.id] = body
        }
        await loadInlineImages(of: loaded, generation: generation)
    }

    /// Largest inline image fetched automatically.
    static let inlineImageLimit: UInt64 = 10_000_000

    /// Fetch the images a body references by content id. They come from
    /// the account's own mail, so unlike remote images they are not gated.
    private func loadInlineImages(of detail: ThreadDetail, generation: Int) async {
        guard let core else { return }
        for message in detail.messages where bodies[message.id]?.html?.contains("openagc-cid:") == true {
            for attachment in message.attachments {
                guard let cid = attachment.contentId.map(Self.normalizedContentID), !cid.isEmpty,
                      attachment.mimeType.hasPrefix("image/"), attachment.size <= Self.inlineImageLimit,
                      inlineImages[cid] == nil,
                      let file = try? await core.attachmentFile(attachment.id),
                      generation == loadGeneration,
                      let data = try? Data(contentsOf: file.url, options: .mappedIfSafe)
                else { continue }
                inlineImages[cid] = InlineImage(data: data, mimeType: attachment.mimeType)
            }
        }
    }

    static func normalizedContentID(_ cid: String) -> String {
        cid.trimmingCharacters(in: CharacterSet(charactersIn: "<> "))
    }

    /// Files attached to the thread (not inline images), oldest first.
    var attachments: [AttachmentInfo] {
        (detail?.messages ?? []).flatMap { $0.attachments.filter { !$0.isInline } }
    }

    func loadRemoteImagesForThread() {
        remoteImagesAllowedForThread = true
    }

    func alwaysLoadRemoteImagesFromSenders() {
        guard let detail else { return }
        var allowed = Set(UserDefaults.standard.stringArray(forKey: Self.allowedSendersKey) ?? [])
        for sender in detail.messages.compactMap({ $0.from?.email.lowercased() }) {
            allowed.insert(sender)
        }
        UserDefaults.standard.set(Array(allowed).sorted(), forKey: Self.allowedSendersKey)
        remoteImagesAllowedForThread = true
    }

    /// The thread as reader messages, ready for `EmailDocument`.
    var documentMessages: [EmailDocument.Message] {
        (detail?.messages ?? []).map { m in
            EmailDocument.Message(
                id: m.id,
                fromName: m.from.map { $0.name ?? $0.email } ?? "(unknown sender)",
                fromEmail: m.from?.email ?? "",
                recipients: (m.to + m.cc).map { $0.name ?? $0.email }.joined(separator: ", "),
                date: Date(timeIntervalSince1970: TimeInterval(m.date) / 1000),
                snippet: m.snippet,
                isRead: m.isRead,
                html: bodies[m.id]?.html)
        }
    }

    private func remember(_ body: RenderedBody) {
        if cache[body.messageId] == nil {
            cacheOrder.append(body.messageId)
            if cacheOrder.count > Self.cacheLimit {
                cache.removeValue(forKey: cacheOrder.removeFirst())
            }
        }
        cache[body.messageId] = body
    }
}
