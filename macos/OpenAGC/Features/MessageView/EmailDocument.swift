import Foundation

/// Builds the single HTML document the reader shows for a thread: one
/// `<details>` block per message (collapsed without JavaScript), each body
/// a fragment sanitized in Rust (spec §14.4). Pure, so it is unit-tested.
enum EmailDocument {
    /// Everything but inline styles and our own image schemes is blocked.
    /// Remote images are still gated by the scheme handler.
    static let contentSecurityPolicy =
        "default-src 'none'; img-src openagc-cid: openagc-remote: data:; style-src 'unsafe-inline'"

    struct Message: Equatable {
        let id: String
        let fromName: String
        let fromEmail: String
        let recipients: String
        let date: Date
        let snippet: String
        let isRead: Bool
        /// Sanitized fragment; `nil` until the body has been synced.
        let html: String?
    }

    static func thread(_ messages: [Message], isDark: Bool) -> String {
        var out = """
        <!doctype html><html><head><meta charset="utf-8">
        <meta http-equiv="Content-Security-Policy" content="\(contentSecurityPolicy)">
        <meta name="color-scheme" content="light dark">
        <style>\(stylesheet)</style></head><body>
        """
        for (index, message) in messages.enumerated() {
            let expanded = isExpanded(message, index: index, count: messages.count)
            out += "<details class=\"msg\"\(expanded ? " open" : "") id=\"m-\(escape(message.id))\"><summary>"
            out += "<div class=\"hdr\"><span class=\"from\">\(escape(message.fromName))</span>"
            if message.fromEmail != message.fromName {
                out += "<span class=\"addr\">\(escape(message.fromEmail))</span>"
            }
            out += "<span class=\"date\">\(escape(dateString(message.date)))</span></div>"
            if !message.recipients.isEmpty {
                out += "<div class=\"to\">To: \(escape(message.recipients))</div>"
            }
            out += "<div class=\"snippet\">\(escape(message.snippet))</div></summary>"
            if let html = message.html {
                let paper = isDark && looksStyled(html)
                out += "<div class=\"body\(paper ? " paper" : "")\">\(html)</div>"
            } else {
                out += "<div class=\"body pending\">Downloading message…</div>"
            }
            out += "</details>"
        }
        out += "</body></html>"
        return out
    }

    /// The latest message and every unread one start open, like Mail.
    static func isExpanded(_ message: Message, index: Int, count: Int) -> Bool {
        index == count - 1 || !message.isRead
    }

    /// In dark mode, mail that sets its own colors is shown on a light
    /// "paper" so dark text never lands on a dark background; unstyled mail
    /// adopts the system colors.
    static func looksStyled(_ html: String) -> Bool {
        let lower = html.lowercased()
        return ["bgcolor", "background", "color:", "color=", "<font"].contains { lower.contains($0) }
    }

    static func escape(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.count)
        for c in s {
            switch c {
            case "&": out += "&amp;"
            case "<": out += "&lt;"
            case ">": out += "&gt;"
            case "\"": out += "&quot;"
            case "'": out += "&#39;"
            default: out.append(c)
            }
        }
        return out
    }

    private static let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .medium
        f.timeStyle = .short
        return f
    }()

    private static func dateString(_ date: Date) -> String {
        dateFormatter.string(from: date)
    }

    private static let stylesheet = """
    :root { color-scheme: light dark; }
    html, body { margin: 0; background: Canvas; color: CanvasText; }
    body { font: 14px/1.45 -apple-system, system-ui, sans-serif; padding: 4px 20px 24px; overflow-wrap: anywhere; }
    .msg { border-bottom: 1px solid color-mix(in srgb, CanvasText 12%, transparent); padding: 12px 0; }
    .msg:last-child { border-bottom: none; }
    summary { list-style: none; cursor: default; }
    summary::-webkit-details-marker { display: none; }
    .hdr { display: flex; gap: 8px; align-items: baseline; }
    .from { font-weight: 600; }
    .addr, .to, .date, .snippet { color: GrayText; }
    .addr, .to { font-size: 12px; }
    .date { margin-left: auto; font-size: 12px; white-space: nowrap; }
    .to { margin-top: 2px; }
    .snippet { margin-top: 4px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    details[open] .snippet { display: none; }
    .body { margin-top: 12px; }
    .body img { max-width: 100%; height: auto; }
    .body table { max-width: 100%; }
    .body.paper { background: #ffffff; color: #111111; color-scheme: light; border-radius: 8px; padding: 12px; }
    .body.pending { color: GrayText; font-style: italic; }
    blockquote { margin: 8px 0; padding-left: 10px; border-left: 2px solid color-mix(in srgb, CanvasText 25%, transparent); color: GrayText; }
    a { color: LinkText; }
    """
}
