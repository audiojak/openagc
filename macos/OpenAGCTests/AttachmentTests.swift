import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct AttachmentTests {
    private func demo() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    @Test func readerListsAttachmentsAndFilesArriveQuarantined() async throws {
        let model = try await demo()
        let page = try await model.core!.search("has:attachment", limit: 5)
        let row = try #require(page.rows.first)
        await model.reader.show(threadID: row.id)
        let attachment = try #require(model.reader.attachments.first)
        #expect(attachment.filename.hasSuffix(".pdf"))

        let file = try await model.core!.attachmentFile(attachment.id)
        #expect(FileManager.default.fileExists(atPath: file.url.path))
        #expect(try Data(contentsOf: file.url).starts(with: Data("%PDF".utf8)))
        let quarantine = try file.url.resourceValues(forKeys: [.quarantinePropertiesKey]).quarantineProperties
        #expect(quarantine?[kLSQuarantineAgentNameKey as String] as? String == "OpenAGC")

        let again = try await model.core!.attachmentFile(attachment.id)
        #expect(again.url == file.url, "cached")
    }

    @Test func missingAttachmentsReportNotFound() async throws {
        let model = try await demo()
        do {
            _ = try await model.core!.attachmentFile("987654321")
            Issue.record("expected an error")
        } catch {
            #expect(error.kind == .notFound)
        }
    }

    @Test func contentIDsAreNormalized() {
        #expect(ReaderStore.normalizedContentID("<logo@example.com>") == "logo@example.com")
        #expect(ReaderStore.normalizedContentID("logo") == "logo")
    }
}

@MainActor
struct PDFTextTests {
    @Test func pdfKitExtractsTheDemoAttachmentsText() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let core = try CoreClient(dataDirectory: dir)
        try await core.openAccount(AppModel.demoAccountID)
        try await core.seedDemoMailbox(threads: 200)
        let row = try #require(try await core.search("has:attachment", limit: 1).rows.first)
        let detail = try #require(try await core.thread(row.id))
        let attachment = try #require(detail.messages.flatMap(\.attachments).first)
        let file = try await core.attachmentFile(attachment.id)
        let text = try #require(PDFTextExtractor().pdfText(path: file.url.path))
        let topic = attachment.filename.replacingOccurrences(of: ".pdf", with: "").replacingOccurrences(of: "-", with: " ")
        #expect(text.localizedCaseInsensitiveContains(topic), "\(text) vs \(topic)")
        #expect(PDFTextExtractor().pdfText(path: "/nonexistent.pdf") == nil)
    }
}
