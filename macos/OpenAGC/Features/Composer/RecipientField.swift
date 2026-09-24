import AppKit
import SwiftUI

/// A To/Cc/Bcc field: one token per address, completing from the contacts
/// the core has learned from mail (ranked by how often you write to them).
struct RecipientField: NSViewRepresentable {
    @Binding var addresses: [AddressInfo]
    let suggest: (String) -> [AddressInfo]
    var accessibilityLabel = "To"

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSTokenField {
        let field = NSTokenField()
        field.delegate = context.coordinator
        field.tokenStyle = .rounded
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: NSFont.systemFontSize)
        field.completionDelay = 0
        field.tokenizingCharacterSet = CharacterSet(charactersIn: ",;")
        field.cell?.wraps = true
        field.cell?.isScrollable = false
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        field.setAccessibilityLabel(accessibilityLabel)
        field.objectValue = addresses.map(Token.init)
        return field
    }

    func updateNSView(_ field: NSTokenField, context: Context) {
        context.coordinator.parent = self
        let current = (field.objectValue as? [Any] ?? []).compactMap { ($0 as? Token)?.address }
        if current != addresses {
            field.objectValue = addresses.map(Token.init)
        }
    }

    /// Wraps an address so the token field keeps it as one object.
    final class Token: NSObject {
        let address: AddressInfo
        init(_ address: AddressInfo) { self.address = address }
    }

    @MainActor
    final class Coordinator: NSObject, NSTokenFieldDelegate {
        var parent: RecipientField
        /// Completions from the last query, so a picked string maps back to
        /// the address with its display name.
        private var offered: [String: AddressInfo] = [:]

        init(_ parent: RecipientField) {
            self.parent = parent
        }

        func tokenField(_ tokenField: NSTokenField, completionsForSubstring substring: String,
                        indexOfToken tokenIndex: Int, indexOfSelectedItem selectedIndex: UnsafeMutablePointer<Int>?) -> [Any]? {
            let matches = parent.suggest(substring)
            offered = [:]
            let strings = matches.map { address in
                let s = Self.editingString(address)
                offered[s] = address
                return s
            }
            selectedIndex?.pointee = strings.isEmpty ? -1 : 0
            return strings
        }

        func tokenField(_ tokenField: NSTokenField, representedObjectForEditing editingString: String) -> Any? {
            let trimmed = editingString.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { return nil }
            return Token(offered[trimmed] ?? Self.parse(trimmed))
        }

        func tokenField(_ tokenField: NSTokenField, displayStringForRepresentedObject representedObject: Any) -> String? {
            guard let token = representedObject as? Token else { return representedObject as? String }
            return token.address.name.flatMap { $0.isEmpty ? nil : $0 } ?? token.address.email
        }

        func tokenField(_ tokenField: NSTokenField, editingStringForRepresentedObject representedObject: Any) -> String? {
            (representedObject as? Token).map { Self.editingString($0.address) }
        }

        func tokenField(_ tokenField: NSTokenField, hasMenuForRepresentedObject representedObject: Any) -> Bool { false }

        func controlTextDidChange(_ notification: Notification) { publish(notification) }
        func controlTextDidEndEditing(_ notification: Notification) { publish(notification) }

        private func publish(_ notification: Notification) {
            guard let field = notification.object as? NSTokenField else { return }
            let addresses = (field.objectValue as? [Any] ?? []).compactMap { item -> AddressInfo? in
                if let token = item as? Token { return token.address }
                if let string = item as? String, !string.trimmingCharacters(in: .whitespaces).isEmpty {
                    return Self.parse(string)
                }
                return nil
            }
            if addresses != parent.addresses { parent.addresses = addresses }
        }

        static func editingString(_ address: AddressInfo) -> String {
            guard let name = address.name, !name.isEmpty else { return address.email }
            return "\(name) <\(address.email)>"
        }

        /// "Name <email>", "<email>" or a bare address.
        static func parse(_ text: String) -> AddressInfo {
            let s = text.trimmingCharacters(in: .whitespacesAndNewlines)
            if let open = s.lastIndex(of: "<"), let close = s.lastIndex(of: ">"), open < close {
                let email = String(s[s.index(after: open)..<close]).trimmingCharacters(in: .whitespaces)
                let name = s[..<open].trimmingCharacters(in: CharacterSet.whitespaces.union(CharacterSet(charactersIn: "\"")))
                return AddressInfo(name: name.isEmpty ? nil : name, email: email)
            }
            return AddressInfo(name: nil, email: s)
        }
    }
}
