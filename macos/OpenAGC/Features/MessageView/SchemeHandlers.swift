import Foundation
import UniformTypeIdentifiers
import WebKit

/// Serves `openagc-remote:<url>` images (spec §14.4). Blocked by default: a
/// transparent pixel is returned and nothing leaves the Mac. When allowed,
/// the image is fetched without cookies, referrer or a stored cache, so a
/// tracking pixel learns as little as possible.
@MainActor
final class RemoteImageSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "openagc-remote"
    var allowRemote = false

    private var tasks: [ObjectIdentifier: Task<Void, Never>] = [:]

    private static let session: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.httpShouldSetCookies = false
        config.httpCookieAcceptPolicy = .never
        config.urlCache = nil
        config.timeoutIntervalForRequest = 15
        config.httpAdditionalHeaders = ["User-Agent": "Mozilla/5.0"]
        return URLSession(configuration: config)
    }()

    static let transparentGIF = Data(base64Encoded: "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")!

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        let key = ObjectIdentifier(urlSchemeTask)
        guard allowRemote, let remote = Self.remoteURL(from: urlSchemeTask.request.url) else {
            Self.respond(urlSchemeTask, data: Self.transparentGIF, mimeType: "image/gif")
            return
        }
        tasks[key] = Task { [weak self] in
            defer { self?.tasks[key] = nil }
            do {
                var request = URLRequest(url: remote)
                request.setValue(nil, forHTTPHeaderField: "Referer")
                let (data, response) = try await Self.session.data(for: request)
                guard !Task.isCancelled else { return }
                let mime = (response as? HTTPURLResponse)?.mimeType ?? "application/octet-stream"
                // Only images may come back through an image scheme.
                Self.respond(urlSchemeTask, data: mime.hasPrefix("image/") ? data : Self.transparentGIF,
                             mimeType: mime.hasPrefix("image/") ? mime : "image/gif")
            } catch {
                guard !Task.isCancelled else { return }
                urlSchemeTask.didFailWithError(error)
            }
        }
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        tasks.removeValue(forKey: ObjectIdentifier(urlSchemeTask))?.cancel()
    }

    /// `openagc-remote:https://host/path` → `https://host/path`. Only http(s).
    static func remoteURL(from url: URL?) -> URL? {
        guard let url, url.scheme == scheme else { return nil }
        let rest = String(url.absoluteString.dropFirst(scheme.count + 1))
        guard let remote = URL(string: rest), ["http", "https"].contains(remote.scheme?.lowercased() ?? "") else {
            return nil
        }
        return remote
    }

    static func respond(_ task: any WKURLSchemeTask, data: Data, mimeType: String) {
        guard let url = task.request.url else { return }
        task.didReceive(URLResponse(url: url, mimeType: mimeType, expectedContentLength: data.count, textEncodingName: nil))
        task.didReceive(data)
        task.didFinish()
    }
}

/// Serves `openagc-cid:<content-id>` inline images from the message's
/// attachments. Until attachment bytes are fetched on demand (spec §14.3),
/// inline images render as transparent placeholders.
@MainActor
final class CidSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "openagc-cid"
    /// Looks up inline attachment bytes by content id; `nil` if unavailable.
    var provider: ((String) -> (Data, String)?)?

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        let cid = urlSchemeTask.request.url.map { String($0.absoluteString.dropFirst(Self.scheme.count + 1)) } ?? ""
        if let (data, mime) = provider?(cid.removingPercentEncoding ?? cid) {
            RemoteImageSchemeHandler.respond(urlSchemeTask, data: data, mimeType: mime)
        } else {
            RemoteImageSchemeHandler.respond(urlSchemeTask, data: RemoteImageSchemeHandler.transparentGIF, mimeType: "image/gif")
        }
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {}
}
