import AppKit
import SwiftUI
import WebKit

/// The locked-down web view that renders a thread (spec §14.4): no
/// JavaScript, no persistent storage, no navigation, links opened in the
/// user's browser after a phishing check, images only through our schemes.
struct MessageWebView: NSViewRepresentable {
    let html: String
    let allowRemoteImages: Bool
    /// Inline images by content id; the page reloads when more arrive.
    var inlineImages: [String: InlineImage] = [:]

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        config.preferences.isElementFullscreenEnabled = false
        config.setURLSchemeHandler(context.coordinator.remoteImages, forURLScheme: RemoteImageSchemeHandler.scheme)
        config.setURLSchemeHandler(context.coordinator.inlineImages, forURLScheme: CidSchemeHandler.scheme)
        let webView = WKWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = true
        webView.underPageBackgroundColor = .textBackgroundColor
        #if DEBUG
        webView.isInspectable = true
        #else
        webView.isInspectable = false
        #endif
        webView.setAccessibilityLabel("Message")
        return webView
    }

    func updateNSView(_ webView: WKWebView, context: Context) {
        let coordinator = context.coordinator
        let remoteChanged = coordinator.remoteImages.allowRemote != allowRemoteImages
        coordinator.remoteImages.allowRemote = allowRemoteImages
        let images = inlineImages
        coordinator.inlineImages.provider = { cid in images[cid].map { ($0.data, $0.mimeType) } }
        let imagesChanged = coordinator.loadedInlineImages != Set(images.keys)
        coordinator.loadedInlineImages = Set(images.keys)
        guard html != coordinator.loadedHTML || remoteChanged || imagesChanged else { return }
        coordinator.loadedHTML = html
        coordinator.suspiciousLinks = LinkSafety.mismatchedLinks(in: html)
        // A fixed, opaque base URL: nothing in the document can resolve
        // relative to a real origin.
        webView.loadHTMLString(html, baseURL: URL(string: "about:blank"))
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate {
        let remoteImages = RemoteImageSchemeHandler()
        let inlineImages = CidSchemeHandler()
        var loadedHTML = ""
        var loadedInlineImages: Set<String> = []
        var suspiciousLinks: [String: String] = [:]

        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction) async -> WKNavigationActionPolicy {
            // Our own loadHTMLString is the only navigation allowed.
            if action.navigationType == .other, action.request.url?.absoluteString == "about:blank" {
                return .allow
            }
            if action.navigationType == .linkActivated, let url = action.request.url {
                open(url)
            }
            return .cancel
        }

        /// `target=_blank` links arrive here; open them externally too.
        func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                     for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
            if let url = action.request.url { open(url) }
            return nil
        }

        private func open(_ url: URL) {
            guard let scheme = url.scheme?.lowercased(), ["http", "https", "mailto"].contains(scheme) else { return }
            if let shown = suspiciousLinks[url.absoluteString] {
                let alert = NSAlert()
                alert.messageText = "This link may not go where it says"
                alert.informativeText = "The link text shows “\(shown)”, but it opens \(url.host() ?? url.absoluteString)."
                alert.alertStyle = .warning
                alert.addButton(withTitle: "Don’t Open")
                alert.addButton(withTitle: "Open Anyway")
                guard alert.runModal() == .alertSecondButtonReturn else { return }
            }
            NSWorkspace.shared.open(url)
        }
    }
}
