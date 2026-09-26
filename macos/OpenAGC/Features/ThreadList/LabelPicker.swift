import AppKit
import SwiftUI

/// What the label picker (`l`) lists for a filter: the label tree flattened
/// with depth, each row's state for the targeted threads, and a "create"
/// row when the filter names a label that does not exist (spec §14.3).
struct LabelPickerModel: Equatable {
    struct Row: Equatable, Identifiable {
        let node: LabelNode
        /// On for all targets, mixed for some, off for none.
        let state: NSControl.StateValue
        var id: String { node.id }
        var depth: Int { node.depth }
    }

    let rows: [Row]
    /// The path to offer creating, if the filter is a new label.
    let createPath: String?

    init(labels: [MailboxInfo], targets: [ThreadRow], filter: String) {
        let query = filter.trimmingCharacters(in: .whitespaces)
        let tree = LabelTree.build(labels)
        let visible = query.isEmpty ? tree : tree.compactMap { $0.filtered(query) }
        rows = visible.flatMap(\.flattened).map { node in
            guard let id = node.mailbox?.labelId, !targets.isEmpty else { return Row(node: node, state: .off) }
            let count = targets.filter { $0.labelIds.contains(id) }.count
            return Row(node: node, state: count == targets.count ? .on : count == 0 ? .off : .mixed)
        }
        let cleaned = query.split(separator: "/").map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }.joined(separator: "/")
        let exists = labels.contains { $0.name.caseInsensitiveCompare(cleaned) == .orderedSame }
        createPath = cleaned.isEmpty || exists ? nil : cleaned
    }

    enum ReturnAction: Equatable {
        case toggle(Row)
        case create(String)
        case nothing
    }

    /// Return toggles the first matching label; with no match it creates
    /// the typed path.
    var returnAction: ReturnAction {
        if let row = rows.first(where: { !$0.node.isGroup }) { return .toggle(row) }
        if let createPath { return .create(createPath) }
        return .nothing
    }
}

/// The `l` popover: a filter field over the label tree. Clicking a label
/// toggles it on the targeted threads; Return applies the first match or
/// creates the typed path.
struct LabelPickerView: View {
    @Environment(AppModel.self) private var model
    @State private var filter = ""
    @State private var error: String?
    @FocusState private var focused: Bool
    let close: () -> Void

    private var picker: LabelPickerModel {
        let targets = Set(model.actionTargets)
        return LabelPickerModel(labels: model.mailboxes.labels,
                                targets: model.threads.rows.filter { targets.contains($0.id) }, filter: filter)
    }

    var body: some View {
        let picker = picker
        VStack(alignment: .leading, spacing: 6) {
            TextField("Filter or new label (nest with /)", text: $filter)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit { submit(picker) }
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(picker.rows) { row in
                        rowView(row)
                    }
                    if let path = picker.createPath {
                        Button {
                            create(path)
                        } label: {
                            Label("Create “\(path)”", systemImage: "plus")
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .padding(.vertical, 3)
                    }
                    if picker.rows.isEmpty && picker.createPath == nil {
                        Text("No labels").foregroundStyle(.secondary).padding(.vertical, 3)
                    }
                }
            }
            .frame(maxHeight: 320)
            if let error {
                Text(error).font(.caption).foregroundStyle(.red)
            }
        }
        .padding(10)
        .frame(width: 280)
        .onAppear { focused = true }
    }

    @ViewBuilder private func rowView(_ row: LabelPickerModel.Row) -> some View {
        let content = HStack(spacing: 6) {
            Image(systemName: row.state == .on ? "checkmark.square.fill" : row.state == .mixed ? "minus.square" : "square")
                .foregroundStyle(row.node.isGroup ? .clear : .secondary)
            Text(row.node.name)
                .foregroundStyle(row.node.isGroup ? .secondary : .primary)
            Spacer(minLength: 0)
        }
        .padding(.leading, CGFloat(row.depth) * 14)
        .padding(.vertical, 3)
        .contentShape(Rectangle())
        if let id = row.node.mailbox?.labelId {
            Button { toggle(id, row.state) } label: { content }
                .buttonStyle(.plain)
                .accessibilityLabel("\(row.node.path), \(row.state == .on ? "applied" : "not applied")")
        } else {
            content.accessibilityLabel("\(row.node.path), group")
        }
    }

    private func toggle(_ labelID: String, _ state: NSControl.StateValue) {
        model.setLabel(labelID, applied: state != .on)
        close()
    }

    private func submit(_ picker: LabelPickerModel) {
        switch picker.returnAction {
        case .toggle(let row): if let id = row.node.mailbox?.labelId { toggle(id, row.state) }
        case .create(let path): create(path)
        case .nothing: break
        }
    }

    private func create(_ path: String) {
        Task {
            if let message = await model.createLabel(path: path) {
                error = message
            } else {
                close()
            }
        }
    }
}

/// Hosts the picker in a transient popover anchored at a table row.
@MainActor
enum LabelPickerPopover {
    static func show(relativeTo rect: NSRect, of view: NSView, model: AppModel) {
        let popover = NSPopover()
        popover.behavior = .transient
        let host = NSHostingController(rootView: LabelPickerView { popover.performClose(nil) }.environment(model))
        popover.contentViewController = host
        popover.show(relativeTo: rect, of: view, preferredEdge: .maxY)
    }
}
