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
        let table = NSTableView()
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
        context.coordinator.update(rows: store.rows, generation: store.generation, selectedID: model.selectedThreadID)
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

        func update(rows newRows: [ThreadRow], generation newGeneration: Int, selectedID: String?) {
            guard let table else { return }
            if newGeneration != generation {
                generation = newGeneration
                rows = newRows
                table.reloadData()
                if newRows.isEmpty == false, table.selectedRow < 0, selectedID == nil {
                    table.scrollRowToVisible(0)
                }
            } else if newRows.count > rows.count, newRows.starts(with: rows, by: { $0.id == $1.id }) {
                let added = IndexSet(integersIn: rows.count..<newRows.count)
                rows = newRows
                table.insertRows(at: added, withAnimation: [])
            } else if newRows != rows {
                rows = newRows
                table.reloadData()
            }
            select(id: selectedID)
        }

        private func select(id: String?) {
            guard let table else { return }
            let target = id.flatMap { id in rows.firstIndex { $0.id == id } }
            let current = table.selectedRowIndexes
            if let target {
                guard !current.contains(target) else { return }
                applyingSelection = true
                table.selectRowIndexes([target], byExtendingSelection: false)
                table.scrollRowToVisible(target)
                applyingSelection = false
            } else if id == nil, !current.isEmpty {
                applyingSelection = true
                table.deselectAll(nil)
                applyingSelection = false
            }
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
            let selected = table.selectedRowIndexes
            // One thread shows in the reader; multi-select is for bulk actions.
            model.selectedThreadID = selected.count == 1 ? rows[selected.first!].id : nil
        }
    }
}
