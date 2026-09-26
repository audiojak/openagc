import Foundation

/// Gmail nests labels by `/` in the name (`Marked Important/1-Daily`); the
/// sidebar, the label popover and the label menus show them as a tree
/// (spec §14.3, amended 2026-09-26).
struct LabelNode: Identifiable, Equatable {
    /// The full path, unique per account (`Marked Important/1-Daily`).
    let path: String
    /// The last path segment (`1-Daily`).
    let name: String
    /// The label at this path, or nil for a prefix-only group row (Gmail
    /// allows `a/b` without a label `a`).
    let mailbox: MailboxInfo?
    let children: [LabelNode]

    var id: String { mailbox?.id ?? "group:\(path)" }
    var depth: Int { path.split(separator: "/", omittingEmptySubsequences: false).count - 1 }
    var isGroup: Bool { mailbox == nil }

    /// Unread threads carrying this label itself.
    var ownUnread: Int { mailbox.map { Int($0.unreadCount) } ?? 0 }

    /// Own plus every descendant's unread count, shown when collapsed. A
    /// thread labelled at two levels counts twice; label stats are kept per
    /// label, and an exact union would need a query per row.
    var totalUnread: Int { ownUnread + children.reduce(0) { $0 + $1.totalUnread } }

    /// Nodes in display order with their children flattened in, for menus.
    var flattened: [LabelNode] { [self] + children.flatMap(\.flattened) }

    /// This node and its descendants whose path contains `query` (case- and
    /// diacritic-insensitive), keeping ancestors so matches stay in context.
    func filtered(_ query: String) -> LabelNode? {
        let kids = children.compactMap { $0.filtered(query) }
        if path.range(of: query, options: [.caseInsensitive, .diacriticInsensitive]) != nil || !kids.isEmpty {
            return LabelNode(path: path, name: name, mailbox: mailbox, children: kids)
        }
        return nil
    }
}

enum LabelTree {
    /// Build the forest from label mailboxes. Sorted by name at each level,
    /// numerically aware so `2-Weekly` sorts before `10-Monthly`.
    static func build(_ labels: [MailboxInfo]) -> [LabelNode] {
        final class Draft {
            var mailbox: MailboxInfo?
            var children: [String: Draft] = [:]
        }
        let root = Draft()
        for label in labels {
            let segments = label.name.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
            // A name with empty segments ("a//b", "/a", "a/") is not a path:
            // keep it whole as a top-level label rather than inventing groups.
            let path = segments.contains(where: \.isEmpty) ? [label.name] : segments
            var node = root
            for segment in path {
                if let next = node.children[segment] {
                    node = next
                } else {
                    let next = Draft()
                    node.children[segment] = next
                    node = next
                }
            }
            node.mailbox = label
        }
        func finish(_ draft: Draft, prefix: String?) -> [LabelNode] {
            draft.children
                .sorted { $0.key.localizedStandardCompare($1.key) == .orderedAscending }
                .map { name, child in
                    let path = prefix.map { "\($0)/\(name)" } ?? name
                    return LabelNode(path: path, name: name, mailbox: child.mailbox,
                                     children: finish(child, prefix: path))
                }
        }
        return finish(root, prefix: nil)
    }

    /// The leaf name for a chip: the last path segment.
    static func leafName(_ name: String) -> String {
        guard !name.hasSuffix("/"), let slash = name.lastIndex(of: "/") else { return name }
        return String(name[name.index(after: slash)...])
    }
}
