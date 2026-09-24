import Foundation

/// Phishing check for links (spec §14.4, §15): when a link's visible text
/// names a host different from where it actually goes, the reader asks
/// before opening it.
enum LinkSafety {
    /// Links whose visible text names a different host than their target,
    /// keyed by target URL string, valued by the visible text.
    static func mismatchedLinks(in html: String) -> [String: String] {
        var result: [String: String] = [:]
        let range = NSRange(html.startIndex..., in: html)
        anchor.enumerateMatches(in: html, range: range) { match, _, _ in
            guard let match,
                  let hrefRange = Range(match.range(at: 1), in: html),
                  let textRange = Range(match.range(at: 2), in: html)
            else { return }
            let href = decodeEntities(String(html[hrefRange]))
            let text = decodeEntities(stripTags(String(html[textRange]))).trimmingCharacters(in: .whitespacesAndNewlines)
            if isMismatch(href: href, visibleText: text) {
                result[href] = text
            }
        }
        return result
    }

    static func isMismatch(href: String, visibleText: String) -> Bool {
        guard let target = URL(string: href)?.host(), let shown = host(inVisibleText: visibleText) else {
            return false
        }
        return normalized(target) != normalized(shown)
            && !normalized(target).hasSuffix("." + normalized(shown))
    }

    /// The host a piece of link text claims, if it looks like a URL or a
    /// bare domain ("bank.example.com/login"). Ordinary words return nil.
    static func host(inVisibleText text: String) -> String? {
        guard !text.contains(" "), text.contains(".") else { return nil }
        let candidate = text.contains("://") ? text : "https://" + text
        guard let host = URL(string: candidate)?.host(), host.contains("."),
              host.split(separator: ".").last.map({ $0.count >= 2 && $0.allSatisfy(\.isLetter) }) == true
        else { return nil }
        return host
    }

    private static func normalized(_ host: String) -> String {
        let lower = host.lowercased()
        return lower.hasPrefix("www.") ? String(lower.dropFirst(4)) : lower
    }

    private static let anchor = try! NSRegularExpression(
        pattern: #"<a\s[^>]*?href="([^"]*)"[^>]*>(.*?)</a>"#,
        options: [.caseInsensitive, .dotMatchesLineSeparators])

    private static let tag = try! NSRegularExpression(pattern: "<[^>]+>")

    private static func stripTags(_ s: String) -> String {
        tag.stringByReplacingMatches(in: s, range: NSRange(s.startIndex..., in: s), withTemplate: "")
    }

    private static func decodeEntities(_ s: String) -> String {
        s.replacingOccurrences(of: "&lt;", with: "<")
            .replacingOccurrences(of: "&gt;", with: ">")
            .replacingOccurrences(of: "&quot;", with: "\"")
            .replacingOccurrences(of: "&#39;", with: "'")
            .replacingOccurrences(of: "&amp;", with: "&")
    }
}
