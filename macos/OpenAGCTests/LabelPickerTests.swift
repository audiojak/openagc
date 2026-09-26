import AppKit
import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct LabelPickerTests {
    private func label(_ name: String) -> MailboxInfo {
        MailboxInfo(id: "L:\(name)", kind: .label, labelId: "L:\(name)", name: name, unreadCount: 0, totalCount: 0)
    }

    private func row(_ id: String, labels: [String]) -> ThreadRow {
        ThreadRow(id: id, subject: "", snippet: "snippet", lastMessageAt: 0, messageCount: 1, unreadCount: 0,
                  hasAttachments: false, isStarred: false, participants: [], labelIds: labels)
    }

    private let labels = ["Work", "Work/Hiring", "Work/Hiring/Onsite", "Clients/Acme", "Receipts"]

    @Test func rowsFollowTheTreeWithStatesForTheTargets() {
        let targets = [row("t1", labels: ["L:Work/Hiring", "L:Receipts"]), row("t2", labels: ["L:Receipts"])]
        let picker = LabelPickerModel(labels: labels.map(label), targets: targets, filter: "")
        #expect(picker.rows.map(\.node.path) == ["Clients", "Clients/Acme", "Receipts", "Work", "Work/Hiring", "Work/Hiring/Onsite"])
        #expect(picker.rows.map(\.depth) == [0, 1, 0, 0, 1, 2])
        let state = Dictionary(uniqueKeysWithValues: picker.rows.map { ($0.node.path, $0.state) })
        #expect(state["Receipts"] == .on)
        #expect(state["Work/Hiring"] == .mixed)
        #expect(state["Work"] == .off)
        #expect(picker.createPath == nil)
    }

    @Test func filteringKeepsAncestorsAndReturnTogglesTheFirstLabel() {
        let picker = LabelPickerModel(labels: labels.map(label), targets: [], filter: "onsite")
        #expect(picker.rows.map(\.node.path) == ["Work", "Work/Hiring", "Work/Hiring/Onsite"])
        #expect(picker.createPath == "onsite", "no label is named exactly that")
        guard case .toggle(let first) = picker.returnAction else { Issue.record("expected toggle"); return }
        #expect(first.node.path == "Work", "Work is itself a label, first in order")
    }

    @Test func aNewPathIsOfferedForCreationAndReturnCreatesIt() {
        let picker = LabelPickerModel(labels: labels.map(label), targets: [], filter: " Clients / Globex ")
        #expect(picker.rows.isEmpty)
        #expect(picker.createPath == "Clients/Globex", "segments are trimmed")
        #expect(picker.returnAction == .create("Clients/Globex"))
        let existing = LabelPickerModel(labels: labels.map(label), targets: [], filter: "receipts")
        #expect(existing.createPath == nil, "case-insensitive match with an existing label")
    }

    @Test func groupRowsAreNeverTheReturnTarget() {
        let picker = LabelPickerModel(labels: [label("Clients/Acme")], targets: [], filter: "clients")
        guard case .toggle(let first) = picker.returnAction else { Issue.record("expected toggle"); return }
        #expect(first.node.path == "Clients/Acme")
    }

    @Test func chipsShowOtherUserLabelsAsLeafNames() {
        let chipLabels: [String: ThreadRowView.Chip] = [
            "L1": .init(path: "Marked Important/1-Daily", color: "#fb4c2f"),
            "L2": .init(path: "Receipts", color: nil),
            "L3": .init(path: "Clients/Acme", color: "#4a86e8"),
        ]
        let r = row("t", labels: ["INBOX", "L1", "L2", "L3", "UNREAD"])
        let chips = ThreadRowView.chips(for: r, labels: chipLabels, excluding: "L2")
        #expect(chips.map(\.path) == ["Clients/Acme", "Marked Important/1-Daily"], "sorted, current mailbox and system labels left out")
        let text = ThreadRowView.snippetLine("hello", chips: chips).string
        #expect(text.contains("Acme"))
        #expect(text.contains("1-Daily"))
        #expect(!text.contains("Marked Important"), "leaf names only")
        #expect(text.hasSuffix("hello"))
        #expect(ThreadRowView.chips(for: r, labels: chipLabels, excluding: nil, limit: 1).count == 1)
    }

    @Test func creatingANestedLabelMakesItsParentsAndAppliesIt() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        let first = try #require(model.threads.rows.first)
        model.selectedThreadID = first.id
        let error = await model.createLabel(path: "Vendors/Cloud/Invoices")
        #expect(error == nil)
        let names = Set(model.mailboxes.labels.map(\.name))
        #expect(names.isSuperset(of: ["Vendors", "Vendors/Cloud", "Vendors/Cloud/Invoices"]))
        let leaf = try #require(model.mailboxes.labels.first { $0.name == "Vendors/Cloud/Invoices" })
        #expect(leaf.totalCount == 1, "applied to the selected thread")
        #expect(await model.createLabel(path: "INBOX") != nil, "system names are refused with a message")
    }
}
