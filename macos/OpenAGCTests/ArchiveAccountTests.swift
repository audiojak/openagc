import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct ArchiveAccountTests {
    struct Timeout: Error {}

    /// A synthetic two-message mbox (never real mail).
    static func mbox(at url: URL) throws {
        let text = """
        From a@example.com Mon Sep  1 10:00:00 2025
        From: Alice <alice@example.com>
        To: owner@example.com
        Subject: Hello from the archive
        Date: Mon, 01 Sep 2025 10:00:00 +0000
        Message-ID: <one@example.com>

        First message.

        From b@example.com Mon Sep  1 11:00:00 2025
        From: Owner <owner@example.com>
        To: alice@example.com
        Subject: Re: Hello from the archive
        Date: Mon, 01 Sep 2025 11:00:00 +0000
        Message-ID: <two@example.com>
        In-Reply-To: <one@example.com>

        A reply.

        """
        try Data(text.utf8).write(to: url)
    }

    private func waitUntil(_ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { throw Timeout() }
            try await Task.sleep(for: .milliseconds(25))
        }
    }

    @Test func anImportedMailboxIsAListedAccountThatNeedsNoSignIn() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appending(path: "Old Mail.mbox")
        try Self.mbox(at: file)
        let core = try CoreClient(dataDirectory: dir.appending(path: "data"))
        let model = AppModel(core: core, defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.start(openDemo: true)

        let scan = try await core.scanMailbox(file.path)
        #expect(scan.suggestedName == "Old Mail")
        #expect(scan.suggestedAddress == "owner@example.com")
        let id = try await core.startImport(path: file.path, name: scan.suggestedName,
                                            myAddresses: [scan.suggestedAddress ?? ""])
        try await waitUntil { model.imports[id]?.done == true }
        #expect(model.imports[id]?.imported == 2)
        #expect(model.accounts.contains { $0.id == id && $0.kind == .archive && $0.email == "Old Mail" })

        await model.switchAccount(to: id)
        #expect(model.openAccountID == id)
        #expect(!model.needsReauthentication, "an archive has no sign-in to miss")
        #expect(model.threads.rows.count == 1, "the received message is in the Inbox")
        #expect(core.isArchive(id))
    }
}
