import Foundation
import Testing
@testable import OpenAGC

struct LabelTreeTests {
    private func label(_ name: String, unread: UInt32 = 0) -> MailboxInfo {
        MailboxInfo(id: "Label_\(name)", kind: .label, labelId: "Label_\(name)", name: name,
                    unreadCount: unread, totalCount: unread)
    }

    @Test func nestedLabelsBecomeATreeSortedNumericallyAtEachLevel() {
        let tree = LabelTree.build([
            label("Marked Important/10-Monthly"),
            label("Receipts"),
            label("Marked Important"),
            label("Marked Important/2-Weekly"),
            label("Marked Important/1-Daily"),
        ])
        #expect(tree.map(\.name) == ["Marked Important", "Receipts"])
        let parent = tree[0]
        #expect(!parent.isGroup, "a label that is also a parent stays selectable")
        #expect(parent.children.map(\.name) == ["1-Daily", "2-Weekly", "10-Monthly"])
        #expect(parent.children[0].path == "Marked Important/1-Daily")
        #expect(parent.children[0].depth == 1)
        #expect(parent.children[0].id == "Label_Marked Important/1-Daily")
    }

    @Test func aPrefixWithoutItsOwnLabelIsAGroupRow() {
        let tree = LabelTree.build([label("Clients/Acme/Invoices"), label("Clients/Globex")])
        #expect(tree.count == 1)
        let clients = tree[0]
        #expect(clients.isGroup)
        #expect(clients.id == "group:Clients")
        #expect(clients.children.map(\.name) == ["Acme", "Globex"])
        #expect(clients.children[0].isGroup)
        #expect(clients.children[0].children.map(\.path) == ["Clients/Acme/Invoices"])
    }

    @Test func unreadRollsUpForCollapsedRows() {
        let tree = LabelTree.build([
            label("Work", unread: 1),
            label("Work/Hiring", unread: 3),
            label("Work/Hiring/Onsite", unread: 2),
            label("Team/Ops", unread: 4),
        ])
        let work = tree.first { $0.name == "Work" }!
        #expect(work.ownUnread == 1)
        #expect(work.totalUnread == 6)
        let team = tree.first { $0.name == "Team" }!
        #expect(team.ownUnread == 0, "a group row has no unread of its own")
        #expect(team.totalUnread == 4)
    }

    @Test func oddNamesStayWholeRatherThanInventingGroups() {
        let tree = LabelTree.build([label("a//b"), label("/lead"), label("trail/")])
        #expect(Set(tree.map(\.name)) == ["a//b", "/lead", "trail/"])
        #expect(tree.allSatisfy { $0.children.isEmpty && !$0.isGroup })
    }

    @Test func filteringKeepsAncestorsOfMatches() {
        let tree = LabelTree.build([label("Work/Hiring/Onsite"), label("Work/Travel"), label("Personal")])
        let hits = tree.compactMap { $0.filtered("onsite") }
        #expect(hits.map(\.path) == ["Work"])
        #expect(hits[0].flattened.map(\.path) == ["Work", "Work/Hiring", "Work/Hiring/Onsite"])
        #expect(tree.compactMap { $0.filtered("zzz") }.isEmpty)
    }

    @Test func chipsShowTheLeafName() {
        #expect(LabelTree.leafName("Marked Important/1-Daily") == "1-Daily")
        #expect(LabelTree.leafName("Receipts") == "Receipts")
        #expect(LabelTree.leafName("trail/") == "trail/")
    }
}
