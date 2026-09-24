import AppKit
import SwiftUI

/// The thread list, backed by `NSTableView` (spec §13 rule 2): fixed row
/// height, view reuse, and incremental inserts when a page is appended.
/// SwiftUI `List` re-diffs and re-lays-out on every change and cannot hold
/// 120 fps over tens of thousands of rows.
struct ThreadListView: NSViewRepresentable {
    @Environment(AppModel.self) private var model

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let table = ThreadTableView()
        table.model = model
        table.headerView = nil
        table.style = .inset
        table.rowHeight = ThreadRowView.height
        table.usesAutomaticRowHeights = false
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.allowsMultipleSelection = true
        table.allowsEmptySelection = true
        table.backgroundColor = .clear
        let column = NSTableColumn(identifier: ThreadRowView.identifier)
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        table.setAccessibilityLabel("Threads")

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        context.coordinator.table = table
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let store = model.threads
        context.coordinator.update(rows: store.rows, generation: store.generation,
                                   selected: model.selectedThreadIDs.union(model.selectedThreadID.map { [$0] } ?? []))
    }

    @MainActor
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
        private let model: AppModel
        private var rows: [ThreadRow] = []
        private var generation = -1
        private var applyingSelection = false
        weak var table: NSTableView?

        init(model: AppModel) {
            self.model = model
        }

        func update(rows newRows: [ThreadRow], generation newGeneration: Int, selected: Set<String>) {
            guard let table else { return }
            if newGeneration != generation {
                generation = newGeneration
                rows = newRows
                table.reloadData()
            } else if newRows.count > rows.count, newRows.starts(with: rows, by: { $0.id == $1.id }) {
                let added = IndexSet(integersIn: rows.count..<newRows.count)
                rows = newRows
                table.insertRows(at: added, withAnimation: [])
            } else if newRows != rows {
                rows = newRows
                table.reloadData()
            }
            select(selected)
        }

        /// Make the table's selection match the model's, by thread id.
        private func select(_ ids: Set<String>) {
            guard let table else { return }
            let target = IndexSet(rows.indices.filter { ids.contains(rows[$0].id) })
            guard target != table.selectedRowIndexes else { return }
            applyingSelection = true
            table.selectRowIndexes(target, byExtendingSelection: false)
            if let first = target.first, target.count == 1 { table.scrollRowToVisible(first) }
            applyingSelection = false
        }

        func numberOfRows(in tableView: NSTableView) -> Int {
            rows.count
        }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            let view = tableView.makeView(withIdentifier: ThreadRowView.identifier, owner: nil) as? ThreadRowView
                ?? ThreadRowView()
            view.configure(with: rows[row])
            model.threads.rowWillAppear(at: row)
            return view
        }

        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !applyingSelection, let table else { return }
            let selected = table.selectedRowIndexes.filter { $0 < rows.count }
            // One thread shows in the reader; multi-select is for bulk actions.
            model.selectedThreadIDs = selected.count > 1 ? Set(selected.map { rows[$0].id }) : []
            model.selectedThreadID = selected.count == 1 ? rows[selected.first!].id : nil
        }

        /// Swipe left to archive, right to toggle read (spec §14.3).
        func tableView(_ tableView: NSTableView, rowActionsForRow row: Int,
                       edge: NSTableView.RowActionEdge) -> [NSTableViewRowAction] {
            guard row < rows.count else { return [] }
            let id = rows[row].id
            let model = self.model
            switch edge {
            case .trailing:
                let archive = NSTableViewRowAction(style: .destructive, title: "Archive") { _, _ in
                    model.selectedThreadIDs = []
                    model.selectedThreadID = id
                    model.archiveSelection()
                }
                archive.backgroundColor = .systemPurple
                return [archive]
            case .leading:
                let unread = rows[row].unreadCount > 0
                let toggle = NSTableViewRowAction(style: .regular, title: unread ? "Read" : "Unread") { _, _ in
                    model.selectedThreadIDs = []
                    model.selectedThreadID = id
                    model.toggleReadSelection()
                    tableView.rowActionsVisible = false
                }
                toggle.backgroundColor = .systemBlue
                return [toggle]
            @unknown default:
                return []
            }
        }
    }
}

/// The thread table, with Mail-style single-key shortcuts and a context
/// menu (spec §14.3). Keys act on the selection.
final class ThreadTableView: NSTableView {
    weak var model: AppModel?

    /// Gmail-style j/k: move the selection like the arrow keys.
    private func moveSelection(by delta: Int) {
        guard numberOfRows > 0 else { return }
        let current = selectedRow < 0 ? (delta > 0 ? -1 : numberOfRows) : selectedRow
        let next = min(max(current + delta, 0), numberOfRows - 1)
        selectRowIndexes(IndexSet(integer: next), byExtendingSelection: false)
        scrollRowToVisible(next)
    }

    override func keyDown(with event: NSEvent) {
        guard let model, event.modifierFlags.intersection([.command, .control, .option]).isEmpty else {
            super.keyDown(with: event)
            return
        }
        switch event.charactersIgnoringModifiers {
        case "e": model.archiveSelection()
        case "u": model.toggleReadSelection()
        case "s": model.toggleStarSelection()
        case "l": showLabelMenu()
        case "#": model.trashSelection()
        case "r": model.reply(all: false)
        case "a": model.reply(all: true)
        case "f": model.forward()
        case "c": model.compose(.new(to: nil))
        case "j": moveSelection(by: 1)
        case "k": moveSelection(by: -1)
        case "/": model.focusSearch()
        default:
            if event.keyCode == 51 || event.keyCode == 117 { // delete, forward delete
                model.trashSelection()
            } else {
                super.keyDown(with: event)
            }
        }
    }

    /// Right-click acts on the clicked row, or the whole selection if the
    /// clicked row is part of it.
    override func menu(for event: NSEvent) -> NSMenu? {
        let row = self.row(at: convert(event.locationInWindow, from: nil))
        guard row >= 0, let model else { return nil }
        if !selectedRowIndexes.contains(row) {
            selectRowIndexes([row], byExtendingSelection: false)
        }
        let menu = NSMenu()
        menu.addItem(ActionItem("Archive", key: "e") { model.archiveSelection() })
        menu.addItem(ActionItem("Move to Inbox") { model.moveSelectionToInbox() })
        menu.addItem(.separator())
        menu.addItem(ActionItem("Mark as Read / Unread", key: "u") { model.toggleReadSelection() })
        menu.addItem(ActionItem("Star / Unstar", key: "s") { model.toggleStarSelection() })
        let labels = NSMenuItem(title: "Label", action: nil, keyEquivalent: "")
        labels.submenu = labelMenu()
        menu.addItem(labels)
        menu.addItem(.separator())
        menu.addItem(ActionItem("Move to Trash", key: "\u{8}") { model.trashSelection() })
        return menu
    }

    private func labelMenu() -> NSMenu {
        let menu = NSMenu()
        guard let model else { return menu }
        let targets = Set(model.actionTargets)
        let rows = model.threads.rows.filter { targets.contains($0.id) }
        for label in model.mailboxes.labels {
            guard let id = label.labelId else { continue }
            let applied = !rows.isEmpty && rows.allSatisfy { $0.labelIds.contains(id) }
            let item = ActionItem(label.name) { model.setLabel(id, applied: !applied) }
            item.state = applied ? .on : .off
            menu.addItem(item)
        }
        if menu.items.isEmpty {
            let empty = NSMenuItem(title: "No Labels", action: nil, keyEquivalent: "")
            empty.isEnabled = false
            menu.addItem(empty)
        }
        return menu
    }

    private func showLabelMenu() {
        let row = selectedRow >= 0 ? selectedRow : 0
        let rect = rect(ofRow: row)
        labelMenu().popUp(positioning: nil, at: NSPoint(x: rect.minX + 40, y: rect.maxY), in: self)
    }
}

/// A menu item that runs a closure.
final class ActionItem: NSMenuItem {
    private let handler: () -> Void

    init(_ title: String, key: String = "", handler: @escaping () -> Void) {
        self.handler = handler
        super.init(title: title, action: #selector(run), keyEquivalent: "")
        target = self
        if !key.isEmpty { keyEquivalent = key; keyEquivalentModifierMask = [] }
    }

    @available(*, unavailable)
    required init(coder: NSCoder) { fatalError("not used") }

    @objc private func run() { handler() }
}
