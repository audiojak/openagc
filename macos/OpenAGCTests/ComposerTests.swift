import AppKit
import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct ComposerTests {
    // MARK: HTML serializer

    private func styled(_ parts: [(String, [NSAttributedString.Key: Any])]) -> NSAttributedString {
        let out = NSMutableAttributedString()
        for (text, attrs) in parts {
            var attrs = attrs
            if attrs[.font] == nil { attrs[.font] = ComposerHTML.bodyFont }
            out.append(NSAttributedString(string: text, attributes: attrs))
        }
        return out
    }

    @Test func inlineStylesAndLinksSerialize() {
        let text = styled([
            ("Hi ", [:]),
            ("bold", [.font: ComposerHTML.font(bold: true, italic: false)]),
            (" and ", [:]),
            ("both", [.font: ComposerHTML.font(bold: true, italic: true)]),
            (", ", [:]),
            ("u", [.underlineStyle: NSUnderlineStyle.single.rawValue]),
            (" ", [:]),
            ("site", [.link: URL(string: "https://example.com/?a=1&b=2")!]),
            ("\nSecond <line> & more", [:]),
        ])
        #expect(ComposerHTML.html(from: text) ==
            "<p>Hi <b>bold</b> and <b><i>both</i></b>, <u>u</u> <a href=\"https://example.com/?a=1&amp;b=2\">site</a></p>"
            + "<p>Second &lt;line&gt; &amp; more</p>")
    }

    @Test func unsafeLinksBecomePlainText() {
        let text = styled([("click", [.link: URL(string: "javascript:alert(1)")!])])
        #expect(ComposerHTML.html(from: text) == "<p>click</p>")
    }

    @Test func emptyAndBlankParagraphs() {
        #expect(ComposerHTML.html(from: NSAttributedString()) == "")
        #expect(ComposerHTML.html(from: styled([("a\n\nb\n", [:])])) == "<p>a</p><p><br></p><p>b</p>")
    }

    @Test func listsSerializeAsListElements() {
        let bullets = NSMutableParagraphStyle()
        bullets.textLists = [NSTextList(markerFormat: .disc, options: 0)]
        let numbers = NSMutableParagraphStyle()
        numbers.textLists = [NSTextList(markerFormat: .decimal, options: 0)]
        let text = styled([
            ("Intro\n", [:]),
            ("\t•\tone\n", [.paragraphStyle: bullets]),
            ("\t•\ttwo\n", [.paragraphStyle: bullets]),
            ("\t1.\tfirst\n", [.paragraphStyle: numbers]),
            ("End", [:]),
        ])
        #expect(ComposerHTML.html(from: text) ==
            "<p>Intro</p><ul><li>one</li><li>two</li></ul><ol><li>first</li></ol><p>End</p>")
    }

    @Test func htmlRoundTripsThroughTheEditor() {
        let html = "<p>Hello <b>there</b>, <i>friend</i>.</p><p>Bye</p>"
        let text = ComposerHTML.attributedString(fromHTML: html)
        #expect(text.string == "Hello there, friend.\nBye")
        #expect(ComposerHTML.html(from: text) == html)
    }

    // MARK: Recipients

    @Test func recipientParsing() {
        let parse = RecipientField.Coordinator.parse
        #expect(parse("Alex Rivera <alex@example.com>") == AddressInfo(name: "Alex Rivera", email: "alex@example.com"))
        #expect(parse("\"Rivera, Alex\" <alex@example.com>") == AddressInfo(name: "Rivera, Alex", email: "alex@example.com"))
        #expect(parse(" sam@example.org ") == AddressInfo(name: nil, email: "sam@example.org"))
    }

    // MARK: Store

    private func demo() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    private func waitUntil(_ condition: () async -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(6)
        while !(await condition()) {
            guard ContinuousClock.now < deadline else { throw Timeout() }
            try await Task.sleep(for: .milliseconds(50))
        }
    }
    private struct Timeout: Error {}

    private func latestMessage(_ model: AppModel) async throws -> MessageInfo {
        let row = try #require(model.threads.rows.first)
        let detail = try #require(try await model.core!.thread(row.id))
        return try #require(detail.messages.last)
    }

    @Test func contactSuggestionsComeFromMail() async throws {
        let model = try await demo()
        let message = try await latestMessage(model)
        let sender = try #require(message.from)
        let prefix = String(sender.email.prefix(3))
        let suggestions = model.core!.suggestContactsNow(prefix)
        #expect(suggestions.contains { $0.email == sender.email })
    }

    @Test func replyStartsEmptyAboveTheQuoteAndAutosaves() async throws {
        let model = try await demo()
        let message = try await latestMessage(model)
        let store = ComposerStore(core: model.core, attachmentsDirectory: FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString))
        await store.load(.reply(messageID: message.id, all: false))
        #expect(store.phase == .editing)
        #expect(store.subject.hasPrefix("Re:"))
        #expect(store.to.count == 1)
        #expect(store.body.length == 0, "the cursor starts above the quote")
        #expect(store.quotedHTML.contains("<blockquote>"))
        #expect(!store.isDirty && store.draftID == 0, "an untouched reply is not saved")

        store.body = NSAttributedString(string: "Thursday works.", attributes: [.font: ComposerHTML.bodyFont])
        #expect(store.isDirty)
        try await waitUntil { store.draftID != 0 }
        let saved = try #require(try await model.core!.draft(store.draftID))
        #expect(saved.bodyHtml.hasPrefix("<p>Thursday works.</p>"))
        #expect(saved.bodyHtml.contains("<blockquote>"), "the quote is kept with the saved body")
        #expect(saved.inReplyToMessageId == message.id)

        // Further edits keep the quote exactly once.
        store.subject += " (edited)"
        await store.save()
        let again = try #require(try await model.core!.draft(store.draftID))
        #expect(again.bodyHtml.components(separatedBy: "<blockquote>").count == 2)
    }

    @Test func sendingDeletesTheDraftAndLandsInSent() async throws {
        let model = try await demo()
        let store = ComposerStore(core: model.core, attachmentsDirectory: FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString))
        await store.load(.new(to: "friend@example.com"))
        #expect(!store.canSend || store.to.count == 1)
        store.subject = "Composer test"
        store.body = NSAttributedString(string: "Hello", attributes: [.font: ComposerHTML.bodyFont])
        await store.send()
        #expect(store.phase == .sent)
        #expect(try await model.core!.drafts().isEmpty)
        let sent = try await model.core!.threads(in: "SENT", limit: 50).rows
        #expect(sent.contains { $0.subject == "Composer test" })
    }

    @Test func attachmentsAreCopiedAndRemovable() async throws {
        let model = try await demo()
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let store = ComposerStore(core: model.core, attachmentsDirectory: root.appending(path: "copies"))
        await store.load(.new(to: nil))
        let original = root.appending(path: "notes.txt")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try Data("hello".utf8).write(to: original)
        store.attach([original])
        let attachment = try #require(store.attachments.first)
        #expect(attachment.filename == "notes.txt")
        #expect(attachment.mimeType == "text/plain")
        #expect(attachment.size == 5)
        #expect(attachment.path.hasPrefix(root.appending(path: "copies").path))
        #expect(FileManager.default.fileExists(atPath: attachment.path))
        store.removeAttachment(attachment)
        #expect(store.attachments.isEmpty)
        #expect(!FileManager.default.fileExists(atPath: attachment.path))
        #expect(FileManager.default.fileExists(atPath: original.path), "the original is untouched")
    }

    @Test func discardDeletesASavedDraft() async throws {
        let model = try await demo()
        let store = ComposerStore(core: model.core, attachmentsDirectory: FileManager.default.temporaryDirectory
            .appending(path: UUID().uuidString))
        await store.load(.new(to: "friend@example.com"))
        store.subject = "Throwaway"
        await store.save()
        #expect(try await model.core!.drafts().count == 1)
        await store.discard()
        #expect(try await model.core!.drafts().isEmpty)
    }

    @Test func menuReplyTargetFollowsTheReader() async throws {
        let model = try await demo()
        #expect(model.replyTargetMessageID == nil)
        var opened: [ComposeRequest] = []
        model.openComposer = { opened.append($0) }
        let row = try #require(model.threads.rows.first)
        model.selectedThreadID = row.id
        await model.reader.show(threadID: row.id)
        let target = try #require(model.replyTargetMessageID)
        model.reply(all: true)
        model.forward()
        #expect(opened == [.reply(messageID: target, all: true), .forward(messageID: target)])
    }
}
