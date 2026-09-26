import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct SidebarTreeTests {
    struct Timeout: Error {}

    private func demo() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    private func waitUntil(_ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !condition() {
            guard ContinuousClock.now < deadline else { throw Timeout() }
            try await Task.sleep(for: .milliseconds(20))
        }
    }

    @Test func theDemoMailboxShowsNestedLabelsAsATree() async throws {
        let model = try await demo()
        let tree = LabelTree.build(model.mailboxes.labels)
        let customers = try #require(tree.first { $0.name == "Customers" })
        #expect(!customers.isGroup, "Customers is a label with children")
        #expect(customers.children.map(\.name) == ["Acme", "Globex"])
        let projects = try #require(tree.first { $0.name == "Projects" })
        #expect(projects.isGroup, "Projects exists only as a prefix")
        #expect(projects.flattened.map(\.path) == ["Projects", "Projects/Launch", "Projects/Launch/Press"])
    }

    @Test func expansionIsRememberedPerAccount() {
        let a = "test-\(UUID().uuidString)", b = "test-\(UUID().uuidString)"
        defer {
            UserDefaults.standard.removeObject(forKey: "sidebar.expandedLabels.\(a)")
            UserDefaults.standard.removeObject(forKey: "sidebar.expandedLabels.\(b)")
        }
        let expansion = LabelExpansion()
        expansion.load(account: a)
        expansion.set("Customers", true)
        expansion.set("Projects/Launch", true)
        expansion.set("Customers", false)
        expansion.load(account: b)
        #expect(expansion.expanded.isEmpty, "another account starts collapsed")
        expansion.set("Projects", true)
        expansion.load(account: a)
        #expect(expansion.expanded == ["Projects/Launch"])
        expansion.load(account: b)
        #expect(expansion.expanded == ["Projects"])
    }

    @Test func onlyThreadPayloadsCountAsDroppedThreads() {
        #expect(ThreadDrag.threadIDs(in: [ThreadDrag.payload(for: "t1"), "hello", ThreadDrag.payload(for: "t2")]) == ["t1", "t2"])
        #expect(ThreadDrag.threadIDs(in: ["openagc-thread"]).isEmpty)
    }

    @Test func droppingThreadsOnALabelAppliesIt() async throws {
        let model = try await demo()
        let acme = try #require(model.mailboxes.labels.first { $0.name == "Customers/Acme" })
        let labelID = try #require(acme.labelId)
        let before = acme.totalCount
        let targets = model.threads.rows.prefix(3).filter { !$0.labelIds.contains(labelID) }.map(\.id)
        #expect(!targets.isEmpty)
        model.addLabel(labelID, toThreads: targets)
        try await waitUntil {
            (model.mailboxes.labels.first { $0.name == "Customers/Acme" }?.totalCount ?? 0) == before + UInt32(targets.count)
        }
    }
}
