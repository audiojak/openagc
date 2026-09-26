import AppKit
import SwiftUI

/// An account's picture, or its initials on a colour derived from the
/// address so each account keeps the same colour everywhere (spec §7.7).
struct AccountAvatar: View {
    let account: AccountSummary
    var size: CGFloat = 24

    var body: some View {
        Group {
            if let path = account.avatarPath, let image = NSImage(contentsOfFile: path) {
                Image(nsImage: image).resizable().scaledToFill()
            } else {
                ZStack {
                    Circle().fill(AccountAvatar.color(for: account.email))
                    if account.kind == .archive {
                        Image(systemName: "archivebox.fill")
                            .font(.system(size: size * 0.45, weight: .semibold))
                            .foregroundStyle(.white)
                    } else {
                        Text(AccountAvatar.initials(name: account.displayName, email: account.email))
                            .font(.system(size: size * 0.42, weight: .semibold))
                            .foregroundStyle(.white)
                    }
                }
            }
        }
        .frame(width: size, height: size)
        .clipShape(Circle())
        .accessibilityHidden(true)
    }

    /// Up to two letters: first and last name, else the address's first
    /// letter.
    nonisolated static func initials(name: String?, email: String) -> String {
        let words = (name ?? "").split(whereSeparator: { $0.isWhitespace }).filter { $0.first?.isLetter == true }
        if let first = words.first?.first {
            let last = words.count > 1 ? words.last?.first.map(String.init) ?? "" : ""
            return (String(first) + last).uppercased()
        }
        return email.first.map { String($0).uppercased() } ?? "?"
    }

    /// A stable colour per address: FNV-1a over its lowercased bytes picks
    /// one of a palette chosen to carry white text in light and dark mode.
    static func color(for email: String) -> Color {
        Color(nsColor: palette[paletteIndex(for: email)])
    }

    nonisolated static func paletteIndex(for email: String) -> Int {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in email.lowercased().utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x0000_0100_0000_01b3
        }
        return Int(hash % UInt64(paletteCount))
    }

    private nonisolated static let paletteCount = 9

    private static let palette: [NSColor] = [
        .systemBlue, .systemIndigo, .systemPurple, .systemPink, .systemRed,
        .systemOrange, .systemTeal, .systemGreen, .systemBrown,
    ]
}
