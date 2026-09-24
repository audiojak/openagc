import AppKit
import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct MenuTests {
    private func demo() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    @Test func mailboxShortcutsPointAtRealMailboxes() async throws {
        let model = try await demo()
        let ids = Set(model.mailboxes.mailboxes.map(\.id))
        for item in MailCommands.mailboxShortcuts {
            #expect(ids.contains(item.id), "\(item.title) → \(item.id)")
        }
        #expect(model.isMailOpen)
    }

    @Test func searchShortcutRequestsFocus() async throws {
        let model = try await demo()
        let before = model.searchFocusRequests
        model.focusSearch()
        #expect(model.searchFocusRequests == before + 1)
    }

    private final class Rows: NSObject, NSTableViewDataSource {
        func numberOfRows(in tableView: NSTableView) -> Int { 5 }
    }

    private func key(_ characters: String) -> NSEvent {
        NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: 0,
                         context: nil, characters: characters, charactersIgnoringModifiers: characters,
                         isARepeat: false, keyCode: 0)!
    }

    @Test func jAndKMoveTheSelection() async throws {
        let model = try await demo()
        let table = ThreadTableView()
        table.addTableColumn(NSTableColumn(identifier: .init("c")))
        let rows = Rows()
        table.dataSource = rows
        table.model = model
        table.reloadData()
        table.keyDown(with: key("j"))
        #expect(table.selectedRow == 0)
        table.keyDown(with: key("j"))
        table.keyDown(with: key("j"))
        #expect(table.selectedRow == 2)
        table.keyDown(with: key("k"))
        #expect(table.selectedRow == 1)
        for _ in 0..<10 { table.keyDown(with: key("j")) }
        #expect(table.selectedRow == 4, "stops at the last row")
    }
}

struct ShortcutGuideTests {
    @Test func noTwoShortcutsShareKeys() {
        let all = KeyboardShortcutGuide.groups.flatMap(\.shortcuts)
        let menu = all.filter { !$0.inThreadList }.map(\.keys)
        #expect(Set(menu).count == menu.count, "menu shortcuts clash: \(menu)")
        let list = all.filter(\.inThreadList).map(\.keys)
        #expect(Set(list).count == list.count)
        #expect(all.count >= 25)
    }
}

@MainActor
struct AccessibilityTests {
    @Test func threadRowsOfferVoiceOverActionsThatWork() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        let coordinator = ThreadListView.Coordinator(model: model)
        let row = try #require(model.threads.rows.first)
        let actions = coordinator.accessibilityActions(for: row)
        #expect(actions.map(\.name).prefix(2) == ["Archive", "Move to Trash"])
        #expect(actions.contains { $0.name == (row.isStarred ? "Unstar" : "Star") })
        let archive = try #require(actions.first)
        #expect(archive.handler?() == true)
        #expect(!model.threads.rows.contains { $0.id == row.id }, "archived through VoiceOver")
    }
}
