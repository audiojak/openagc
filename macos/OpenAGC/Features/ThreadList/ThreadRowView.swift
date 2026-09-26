import AppKit

/// One thread-list row. Laid out by hand (no Auto Layout) and reused by the
/// table, so configuring a row is a handful of property sets.
final class ThreadRowView: NSTableCellView {
    static let identifier = NSUserInterfaceItemIdentifier("ThreadRow")
    static let height: CGFloat = 70

    private let unreadDot = NSView()
    private let senders = ThreadRowView.label(size: 13)
    private let date = ThreadRowView.label(size: 11)
    private let subject = ThreadRowView.label(size: 12)
    private let snippet = ThreadRowView.label(size: 12)
    private let badges = NSImageView()

    private static let padding: CGFloat = 12
    private static let dotSize: CGFloat = 8

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.identifier
        unreadDot.wantsLayer = true
        unreadDot.layer?.cornerRadius = Self.dotSize / 2
        unreadDot.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        date.alignment = .right
        date.textColor = .secondaryLabelColor
        snippet.textColor = .secondaryLabelColor
        badges.imageScaling = .scaleProportionallyDown
        badges.contentTintColor = .secondaryLabelColor
        for view in [unreadDot, senders, date, subject, snippet, badges] {
            addSubview(view)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    /// A user label shown on a row: its full path and its color.
    struct Chip: Equatable {
        let path: String
        let color: String?
    }

    /// The labels to chip on a row: user labels other than the mailbox
    /// being shown, in path order, at most `limit`.
    static func chips(for row: ThreadRow, labels: [String: Chip], excluding mailboxID: String?, limit: Int = 3) -> [Chip] {
        row.labelIds
            .filter { $0 != mailboxID }
            .compactMap { labels[$0] }
            .sorted { $0.path.localizedStandardCompare($1.path) == .orderedAscending }
            .prefix(limit)
            .map { $0 }
    }

    /// Chips as text: each leaf name on a tint of its label color, then the
    /// snippet. Kept in the snippet line so rows keep their fixed height.
    static func snippetLine(_ snippet: String, chips: [Chip]) -> NSAttributedString {
        let out = NSMutableAttributedString()
        let font = NSFont.systemFont(ofSize: 11, weight: .medium)
        for chip in chips {
            let tint = chip.color.flatMap(NSColor.init(hex:)) ?? .tertiaryLabelColor
            out.append(NSAttributedString(string: "\u{2009}\(LabelTree.leafName(chip.path))\u{2009}", attributes: [
                .font: font,
                .foregroundColor: NSColor.labelColor,
                .backgroundColor: tint.withAlphaComponent(0.28),
            ]))
            out.append(NSAttributedString(string: " ", attributes: [.font: font]))
        }
        out.append(NSAttributedString(string: snippet, attributes: [
            .font: NSFont.systemFont(ofSize: 12),
            .foregroundColor: NSColor.secondaryLabelColor,
        ]))
        return out
    }

    func configure(with row: ThreadRow, chips: [Chip] = []) {
        let unread = row.unreadCount > 0
        unreadDot.isHidden = !unread
        senders.stringValue = Self.senderLine(row)
        senders.font = .systemFont(ofSize: 13, weight: unread ? .semibold : .regular)
        date.stringValue = RowDateFormatter.string(forMillis: row.lastMessageAt)
        subject.stringValue = row.subject.isEmpty ? "(no subject)" : row.subject
        subject.font = .systemFont(ofSize: 12, weight: unread ? .medium : .regular)
        if chips.isEmpty {
            snippet.stringValue = row.snippet
            toolTip = nil
        } else {
            snippet.attributedStringValue = Self.snippetLine(row.snippet, chips: chips)
            toolTip = chips.map(\.path).joined(separator: ", ")
        }
        badges.image = Self.badgeImage(row)
        badges.isHidden = badges.image == nil

        setAccessibilityLabel(
            [unread ? "Unread" : nil, senders.stringValue, subject.stringValue, date.stringValue,
             chips.isEmpty ? nil : "Labels: " + chips.map(\.path).joined(separator: ", "), row.snippet]
                .compactMap { $0 }.joined(separator: ", "))
        needsLayout = true
    }

    override func layout() {
        super.layout()
        let p = Self.padding
        let w = bounds.width
        let textX = p + Self.dotSize + 6
        let dateWidth: CGFloat = 76
        let lineHeight: CGFloat = 17
        // Flipped-agnostic: compute from the top.
        let top = bounds.height - 10
        unreadDot.frame = NSRect(x: p, y: top - lineHeight + 5, width: Self.dotSize, height: Self.dotSize)
        date.frame = NSRect(x: w - p - dateWidth, y: top - lineHeight, width: dateWidth, height: lineHeight)
        senders.frame = NSRect(x: textX, y: top - lineHeight, width: max(0, w - textX - dateWidth - p - 6), height: lineHeight)
        let badgeWidth: CGFloat = badges.isHidden ? 0 : 16
        badges.frame = NSRect(x: w - p - badgeWidth, y: top - 2 * lineHeight, width: badgeWidth, height: lineHeight)
        subject.frame = NSRect(x: textX, y: top - 2 * lineHeight, width: max(0, w - textX - p - badgeWidth - 4), height: lineHeight)
        snippet.frame = NSRect(x: textX, y: top - 3 * lineHeight, width: max(0, w - textX - p), height: lineHeight)
    }

    // MARK: - Content

    /// "Alex Rivera, Sam Chen (4)": up to three senders plus the count.
    static func senderLine(_ row: ThreadRow) -> String {
        let names = row.participants.prefix(3).map { $0.name ?? $0.email }
        var line = names.isEmpty ? "(unknown sender)" : names.joined(separator: ", ")
        if row.participants.count > 3 { line += " …" }
        if row.messageCount > 1 { line += " (\(row.messageCount))" }
        return line
    }

    private static let starImage = NSImage(systemSymbolName: "star.fill", accessibilityDescription: "Starred")
    private static let clipImage = NSImage(systemSymbolName: "paperclip", accessibilityDescription: "Has attachments")

    private static func badgeImage(_ row: ThreadRow) -> NSImage? {
        if row.isStarred { return starImage }
        if row.hasAttachments { return clipImage }
        return nil
    }

    private static func label(size: CGFloat) -> NSTextField {
        let field = NSTextField(labelWithString: "")
        field.font = .systemFont(ofSize: size)
        field.lineBreakMode = .byTruncatingTail
        field.maximumNumberOfLines = 1
        field.cell?.truncatesLastVisibleLine = true
        return field
    }
}

/// Mail-style dates: time today, "Mon" this week, "Sep 12" this year, else
/// a short date. Formatters are created once; making them per row stalls
/// scrolling.
enum RowDateFormatter {
    private static let time: DateFormatter = make("jmm")
    private static let weekday: DateFormatter = make("EEE")
    private static let monthDay: DateFormatter = make("MMMd")
    private static let full: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .short
        f.timeStyle = .none
        return f
    }()

    static func string(forMillis millis: Int64, now: Date = .now, calendar: Calendar = .current) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(millis) / 1000)
        if calendar.isDate(date, inSameDayAs: now) { return time.string(from: date) }
        if let days = calendar.dateComponents([.day], from: date, to: now).day, days < 7, date < now {
            return weekday.string(from: date)
        }
        if calendar.isDate(date, equalTo: now, toGranularity: .year) { return monthDay.string(from: date) }
        return full.string(from: date)
    }

    private static func make(_ template: String) -> DateFormatter {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate(template)
        return f
    }
}

extension NSColor {
    /// `#rrggbb` → NSColor.
    convenience init?(hex: String) {
        let h = hex.trimmingCharacters(in: .whitespaces).trimmingCharacters(in: CharacterSet(charactersIn: "#"))
        guard h.count == 6, let v = UInt32(h, radix: 16) else { return nil }
        self.init(srgbRed: CGFloat((v >> 16) & 0xff) / 255, green: CGFloat((v >> 8) & 0xff) / 255,
                  blue: CGFloat(v & 0xff) / 255, alpha: 1)
    }
}
