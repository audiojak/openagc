import AppKit
import Foundation
import Observation
import os
import UniformTypeIdentifiers

/// What a composer window was opened for. Codable so SwiftUI can restore
/// composer windows across launches.
enum ComposeRequest: Codable, Hashable, Sendable {
    case new(to: String?)
    case reply(messageID: String, all: Bool)
    case forward(messageID: String)
    case draft(id: Int64)
    /// A draft an agent wrote, opened for the user to check before
    /// approving the send.
    case review(draftID: Int64, agent: String)

    /// Who wrote the draft, for the composer's banner.
    var agentName: String? {
        if case let .review(_, agent) = self { agent } else { nil }
    }
}

/// One composer window's state (spec §14.5). Edits are autosaved 2 s after
/// the last change and when the window closes; sending saves first, then
/// hands the draft to the core, which queues it in the outbox.
@MainActor
@Observable
final class ComposerStore {
    enum Phase: Equatable {
        case loading, editing, sending, sent, failed(String)
    }

    static let autosaveDelay: Duration = .seconds(2)

    private(set) var phase: Phase = .loading
    private(set) var from = ""
    private(set) var draftID: Int64 = 0
    private(set) var threadID: String?
    private(set) var inReplyTo: String?
    /// The quoted original of a new reply or forward, shown read-only below
    /// the editor and appended to the body by the core.
    private(set) var quotedHTML = ""
    private(set) var attachments: [DraftAttachmentInfo] = []
    /// Set after a save fails; cleared by the next good one.
    private(set) var saveError: String?
    private(set) var lastSaved: Date?

    var to: [AddressInfo] = [] { didSet { edited() } }
    var cc: [AddressInfo] = [] { didSet { edited() } }
    var bcc: [AddressInfo] = [] { didSet { edited() } }
    var subject = "" { didSet { edited() } }
    /// The editor's text; loaded once, then owned by the text view.
    var body = NSAttributedString() { didSet { edited() } }
    var showsCcBcc = false

    private(set) var isDirty = false
    private let core: CoreClient?
    private let attachmentsDirectory: URL
    private let logger = Logger(subsystem: "ai.actual.openagc", category: "composer")
    private var autosaveTask: Task<Void, Never>?
    private var loading = true

    init(core: CoreClient?, attachmentsDirectory: URL? = nil) {
        self.core = core
        self.attachmentsDirectory = attachmentsDirectory
            ?? ((try? CoreClient.defaultDataDirectory()) ?? FileManager.default.temporaryDirectory)
                .appending(path: "DraftAttachments", directoryHint: .isDirectory)
    }

    var canSend: Bool {
        phase == .editing && !(to.isEmpty && cc.isEmpty && bcc.isEmpty)
    }

    var windowTitle: String {
        subject.trimmingCharacters(in: .whitespaces).isEmpty ? "New Message" : subject
    }

    // MARK: Loading

    func load(_ request: ComposeRequest) async {
        guard let core else {
            phase = .failed("The core is not running.")
            return
        }
        do {
            from = try await core.accountAddress()
            let draft: DraftInfo? = switch request {
            case let .new(to):
                to.map { DraftInfo.empty(to: [AddressInfo(name: nil, email: $0)]) } ?? .empty()
            case let .reply(messageID, all):
                try await core.replyDraft(to: messageID, all: all)
            case let .forward(messageID):
                try await core.forwardDraft(of: messageID)
            case let .draft(id), let .review(id, _):
                try await core.draft(id)
            }
            guard let draft else {
                phase = .failed("This draft no longer exists.")
                return
            }
            if draft.status == .sending {
                phase = .failed("This message is being sent.")
                return
            }
            apply(draft)
            phase = .editing
            // A failed send comes back as an editable draft with the reason.
            if draft.status == .failed { saveError = draft.error ?? "Sending failed." }
        } catch {
            phase = .failed(error.message)
        }
    }

    private func apply(_ draft: DraftInfo) {
        loading = true
        defer { loading = false }
        draftID = draft.id
        threadID = draft.threadId
        inReplyTo = draft.inReplyToMessageId
        to = draft.to
        cc = draft.cc
        bcc = draft.bcc
        showsCcBcc = !draft.cc.isEmpty || !draft.bcc.isEmpty
        subject = draft.subject
        body = ComposerHTML.attributedString(fromHTML: draft.bodyHtml)
        quotedHTML = draft.quotedHtml
        attachments = draft.attachments
        isDirty = false
    }

    // MARK: Editing and autosave

    private func edited() {
        guard !loading else { return }
        isDirty = true
        autosaveTask?.cancel()
        autosaveTask = Task { [weak self] in
            try? await Task.sleep(for: Self.autosaveDelay)
            guard !Task.isCancelled else { return }
            await self?.save()
        }
    }

    /// The draft as the core stores it.
    var record: DraftInfo {
        DraftInfo(id: draftID, threadId: threadID, inReplyToMessageId: inReplyTo,
                  to: to, cc: cc, bcc: bcc, subject: subject,
                  bodyHtml: ComposerHTML.html(from: body), quotedHtml: quotedHTML,
                  attachments: attachments, status: .editing, error: nil, updatedAt: 0)
    }

    /// Save now if anything changed. Safe to call repeatedly.
    func save() async {
        guard isDirty, phase == .editing, let core else { return }
        autosaveTask?.cancel()
        isDirty = false
        do {
            draftID = try await core.saveDraft(record)
            saveError = nil
            lastSaved = .now
        } catch {
            isDirty = true
            saveError = error.message
            logger.error("draft save failed: \(error.message, privacy: .private)")
        }
    }

    /// The window is closing: save, then push the draft to Gmail at once.
    func close() async {
        guard phase == .editing else { return }
        await save()
        core?.flushDrafts()
    }

    // MARK: Attachments

    /// Copies files into the app's data directory so the draft keeps them
    /// even if the originals move.
    func attach(_ urls: [URL]) {
        for url in urls {
            do {
                let folder = attachmentsDirectory.appending(path: UUID().uuidString, directoryHint: .isDirectory)
                try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                let copy = folder.appending(path: url.lastPathComponent)
                try FileManager.default.copyItem(at: url, to: copy)
                let size = (try? copy.resourceValues(forKeys: [.fileSizeKey]).fileSize).map(UInt64.init) ?? 0
                let type = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? "application/octet-stream"
                attachments.append(DraftAttachmentInfo(path: copy.path, filename: url.lastPathComponent,
                                                       mimeType: type, size: size))
            } catch {
                logger.error("attach failed: \(error.localizedDescription, privacy: .private)")
            }
        }
        edited()
    }

    func removeAttachment(_ attachment: DraftAttachmentInfo) {
        attachments.removeAll { $0.path == attachment.path }
        removeCopy(attachment)
        edited()
    }

    private func removeCopy(_ attachment: DraftAttachmentInfo) {
        let url = URL(filePath: attachment.path)
        // Only ever delete our own copies.
        guard url.path.hasPrefix(attachmentsDirectory.path) else { return }
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }

    // MARK: Send and discard

    func send() async {
        guard canSend, let core else { return }
        isDirty = true
        await save()
        guard saveError == nil, draftID != 0 else { return }
        phase = .sending
        do {
            try await core.sendDraft(draftID)
            phase = .sent
        } catch {
            phase = .editing
            saveError = error.message
        }
    }

    func discard() async {
        autosaveTask?.cancel()
        isDirty = false
        if draftID != 0 { try? await core?.deleteDraft(draftID) }
        attachments.forEach(removeCopy)
        phase = .sent
    }
}

extension DraftInfo {
    static func empty(to: [AddressInfo] = []) -> DraftInfo {
        DraftInfo(id: 0, threadId: nil, inReplyToMessageId: nil, to: to, cc: [], bcc: [], subject: "",
                  bodyHtml: "", quotedHtml: "", attachments: [], status: .editing, error: nil, updatedAt: 0)
    }
}
