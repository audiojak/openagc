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

    @Test func theImportSheetSuggestsANameAndAddressAndTheNewAccountOpensWhenDone() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appending(path: "Work 2019.mbox")
        try Self.mbox(at: file)
        let model = AppModel(core: try CoreClient(dataDirectory: dir.appending(path: "data")),
                             defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.start(openDemo: true)

        await model.prepareImport(path: file.path)
        let draft = try #require(model.importDraft)
        #expect(draft.name == "Work 2019")
        #expect(draft.addressList == ["owner@example.com"])
        model.importDraft?.name = "Work archive"
        await model.confirmImport()
        #expect(model.importDraft == nil)
        let id = try #require(model.runningImport)
        try await waitUntil { model.imports[id]?.done == true }
        await model.finishImport(show: true)
        #expect(model.runningImport == nil)
        #expect(model.openAccountID == id)
        #expect(model.accounts.first { $0.id == id }?.email == "Work archive")
    }

    @Test func aPathWithNoMailboxesExplainsWhy() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let model = AppModel(core: try CoreClient(dataDirectory: dir.appending(path: "data")),
                             defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.prepareImport(path: dir.path)
        #expect(model.importDraft?.scan == nil)
        #expect(model.importDraft?.error?.contains("no .mbox files") == true)
    }

    @Test func draftsParseAddressesAndEstimateTime() {
        let draft = ImportDraft(path: "/x", scan: nil, name: "x", addresses: "a@example.com, b@example.com; not-an-address")
        #expect(draft.addressList == ["a@example.com", "b@example.com"])
        #expect(ImportDraft.estimate(bytes: 10_000_000) == "under a minute")
        #expect(ImportDraft.estimate(bytes: 400_000_000) == "about 10 minutes")
        #expect(ImportDraft.estimate(bytes: 9_600_000_000) == "about 4 hours")
    }
}

@MainActor
struct ArchiveCannotSendTests {
    @Test func inAnArchiveTheWindowOffersNoComposingAndTheCoreRefusesAnyway() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appending(path: "old.mbox")
        try ArchiveAccountTests.mbox(at: file)
        let core = try CoreClient(dataDirectory: dir.appending(path: "data"))
        let model = AppModel(core: core, defaults: UserDefaults(suiteName: "test-\(UUID().uuidString)")!)
        await model.start(openDemo: true)
        #expect(!model.isArchive)
        let id = try await core.startImport(path: file.path, name: "Old", myAddresses: ["owner@example.com"])
        let deadline = ContinuousClock.now + .seconds(10)
        while model.imports[id]?.done != true, ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(25)) }
        await model.switchAccount(to: id)
        #expect(model.isArchive)

        var opened: [ComposeRequest] = []
        model.openComposer = { opened.append($0) }
        model.compose(.new(to: nil))
        model.selectedThreadID = model.threads.rows.first?.id
        model.reply(all: false)
        model.forward()
        #expect(opened.isEmpty, "no composer opens in an archive")

        let message = try #require(try await core.thread(model.threads.rows[0].id)?.messages.first)
        await #expect(throws: CoreClientError.self) { try await core.replyDraft(to: message.id, all: false) }
        await #expect(throws: CoreClientError.self) { try await core.sendDraft(1) }
    }
}
