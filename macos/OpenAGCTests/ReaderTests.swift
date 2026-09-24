import Foundation
import Testing
@testable import OpenAGC

struct EmailDocumentTests {
    private func message(_ id: String, read: Bool = true, html: String? = "<p>x</p>", from: String = "Ann") -> EmailDocument.Message {
        EmailDocument.Message(id: id, fromName: from, fromEmail: "ann@example.com", recipients: "Me",
                              date: Date(timeIntervalSince1970: 0), snippet: "snip", isRead: read, html: html)
    }

    @Test func documentCarriesTheCSPAndEscapesHeaderText() {
        let html = EmailDocument.thread([message("1", from: "<script>alert(1)</script>")], isDark: false)
        #expect(html.contains(#"<meta http-equiv="Content-Security-Policy" content="default-src 'none'"#))
        #expect(!html.contains("<script>alert(1)</script>"))
        #expect(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"))
    }

    @Test func latestAndUnreadMessagesStartExpanded() {
        let html = EmailDocument.thread([message("a"), message("b", read: false), message("c"), message("d")], isDark: false)
        #expect(html.contains(#"<details class="msg" id="m-a">"#))
        #expect(html.contains(#"<details class="msg" open id="m-b""#))
        #expect(html.contains(#"<details class="msg" id="m-c""#))
        #expect(html.contains(#"<details class="msg" open id="m-d""#))
    }

    @Test func styledMailGetsPaperInDarkModeOnly() {
        let styled = message("s", html: ##"<table bgcolor="#fff"><tr><td>x</td></tr></table>"##)
        #expect(EmailDocument.thread([styled], isDark: true).contains(#"class="body paper""#))
        #expect(!EmailDocument.thread([styled], isDark: false).contains("paper\""))
        #expect(!EmailDocument.thread([message("u")], isDark: true).contains(#"class="body paper""#))
    }

    @Test func missingBodyShowsAPlaceholder() {
        #expect(EmailDocument.thread([message("p", html: nil)], isDark: false).contains("Downloading message"))
    }
}

struct LinkSafetyTests {
    @Test func flagsLinksWhoseTextNamesAnotherHost() {
        let html = #"<a href="https://evil.example.net/login" target="_blank">https://bank.example.com/login</a>"#
        #expect(LinkSafety.mismatchedLinks(in: html) == ["https://evil.example.net/login": "https://bank.example.com/login"])
    }

    @Test func allowsMatchingHostsSubdomainsAndOrdinaryText() {
        let html = """
        <a href="https://www.example.com/a">example.com</a>
        <a href="https://mail.example.com/x">example.com</a>
        <a href="https://example.org/y">Read more</a>
        <a href="https://example.org/z"><b>example.org</b></a>
        """
        #expect(LinkSafety.mismatchedLinks(in: html).isEmpty)
    }

    @Test func decodesEntitiesInHrefs() {
        let html = #"<a href="https://evil.example.net/?a=1&amp;b=2">paypal.example.com</a>"#
        #expect(LinkSafety.mismatchedLinks(in: html).keys.first == "https://evil.example.net/?a=1&b=2")
    }
}

@MainActor
struct SchemeHandlerTests {
    @Test func remoteURLsAreUnwrappedOnlyForHTTP() {
        #expect(RemoteImageSchemeHandler.remoteURL(from: URL(string: "openagc-remote:https://img.example.com/a.png?x=1"))
            == URL(string: "https://img.example.com/a.png?x=1"))
        #expect(RemoteImageSchemeHandler.remoteURL(from: URL(string: "openagc-remote:file:///etc/passwd")) == nil)
        #expect(RemoteImageSchemeHandler.remoteURL(from: URL(string: "openagc-remote:javascript:alert(1)")) == nil)
        #expect(RemoteImageSchemeHandler.remoteURL(from: URL(string: "https://img.example.com/a.png")) == nil)
    }
}

@MainActor
struct ReaderStoreTests {
    @Test func loadsAThreadWithBodiesAndDecidesRemoteImages() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        let first = try #require(model.threads.rows.first)
        await model.reader.show(threadID: first.id)
        let detail = try #require(model.reader.detail)
        #expect(detail.thread.id == first.id)
        #expect(model.reader.bodies.count == detail.messages.count)
        #expect(model.reader.documentMessages.allSatisfy { $0.html != nil })
        #expect(!model.reader.hasRemoteImages, "demo mail has no remote images")
        #expect(!model.reader.remoteImagesAllowedForThread)
        model.reader.loadRemoteImagesForThread()
        #expect(model.reader.allowsRemoteImages)
        await model.reader.show(threadID: nil)
        #expect(model.reader.detail == nil)
    }
}
