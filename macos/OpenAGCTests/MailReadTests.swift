import Foundation
import Testing
@testable import OpenAGC

struct MailReadTests {
    private func seededClient(threads: UInt32 = 120) async throws -> CoreClient {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let client = try CoreClient(dataDirectory: dir)
        try await client.openAccount("test-account")
        try await client.seedDemoMailbox(threads: threads)
        return client
    }

    @Test func readsBeforeAnAccountIsOpenFailWithNotFound() async throws {
        let client = try CoreClient(dataDirectory: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString))
        await #expect(throws: CoreClientError.self) { try await client.mailboxes() }
        do {
            _ = try await client.mailboxes()
        } catch {
            #expect(error.kind == .notFound)
        }
    }

    @Test func mailboxesThreadsAndBodiesComeBackFromRust() async throws {
        let client = try await seededClient()
        let mailboxes = try await client.mailboxes()
        let inbox = try #require(mailboxes.first { $0.kind == .inbox })
        #expect(inbox.id == "INBOX")
        #expect(inbox.totalCount > 0)

        let page = try await client.threads(in: inbox.id, limit: 20)
        #expect(!page.rows.isEmpty)
        #expect(zip(page.rows, page.rows.dropFirst()).allSatisfy { $0.lastMessageAt >= $1.lastMessageAt })

        let detail = try #require(try await client.thread(page.rows[0].id))
        #expect(detail.thread.id == page.rows[0].id)
        let body = try #require(try await client.renderedBody(detail.messages[0].id))
        #expect(body.html?.hasPrefix("<p>") == true)
    }

    @Test func pagingWithTheCursorNeverRepeatsARow() async throws {
        let client = try await seededClient(threads: 400)
        var seen = Set<String>()
        var cursor: String?
        repeat {
            let page = try await client.threads(in: "@archive", after: cursor, limit: 50)
            for row in page.rows { #expect(seen.insert(row.id).inserted) }
            cursor = page.nextCursor
        } while cursor != nil
        let archive = try #require(try await client.mailboxes().first { $0.kind == .archive })
        #expect(seen.count == Int(archive.totalCount))
    }
}
