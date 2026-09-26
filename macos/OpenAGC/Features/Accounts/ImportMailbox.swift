import SwiftUI

/// An import being set up: what was picked and what the user will call it.
struct ImportDraft: Identifiable, Equatable {
    let path: String
    let scan: MailboxScan?
    var name: String
    /// The user's addresses, comma-separated as typed.
    var addresses: String
    var error: String?

    var id: String { path }

    var addressList: [String] {
        addresses.split(whereSeparator: { $0 == "," || $0 == " " || $0 == ";" })
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { $0.contains("@") }
    }

    /// Rough duration: parsing and indexing run at about 40 MB a minute.
    static func estimate(bytes: UInt64) -> String {
        let minutes = Double(bytes) / 40_000_000
        if minutes < 1 { return "under a minute" }
        if minutes < 90 { return "about \(Int(minutes.rounded())) minutes" }
        return "about \(Int((minutes / 60).rounded())) hours"
    }
}

/// File › Import Mailbox…: name the archive and say which addresses are
/// yours (spec §7.8).
struct ImportMailboxSheet: View {
    @Environment(AppModel.self) private var model
    @State var draft: ImportDraft

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Import Mailbox").font(.title2.weight(.semibold))
            Text("The mail becomes its own account: searchable and ready for the agent, but it cannot send, and nothing is uploaded.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let scan = draft.scan {
                LabeledContent("From") {
                    Text(scan.files.count == 1 ? URL(filePath: scan.files[0]).lastPathComponent : "\(scan.files.count) mailbox files")
                }
                LabeledContent("Size") {
                    Text("\(ByteCountFormatter.string(fromByteCount: Int64(scan.totalBytes), countStyle: .file)) · \(ImportDraft.estimate(bytes: scan.totalBytes))")
                }
                TextField("Account name", text: $draft.name)
                TextField("Your addresses (mail from them counts as sent)", text: $draft.addresses)
            }
            if let error = draft.error {
                Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { model.importDraft = nil }
                    .keyboardShortcut(.cancelAction)
                Button("Import") {
                    model.importDraft = draft
                    Task { await model.confirmImport() }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(draft.scan == nil || draft.name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 460)
    }
}

/// Progress for the running import, with Cancel; a summary when done.
struct ImportProgressSheet: View {
    @Environment(AppModel.self) private var model
    let accountID: String

    var body: some View {
        let status = model.imports[accountID]
        VStack(alignment: .leading, spacing: 14) {
            Text(status?.done == true ? "Import finished" : "Importing mail…").font(.title3.weight(.semibold))
            if let status {
                if status.done {
                    Text(summary(status)).fixedSize(horizontal: false, vertical: true)
                } else {
                    ProgressView(value: status.totalBytes == 0 ? 0 : Double(status.bytes) / Double(status.totalBytes))
                    Text("\(status.imported.formatted()) messages so far").foregroundStyle(.secondary)
                }
            } else {
                ProgressView()
            }
            HStack {
                Spacer()
                if status?.done == true {
                    Button("Close") { Task { await model.finishImport(show: false) } }
                    Button("Show Mail") { Task { await model.finishImport(show: true) } }
                        .keyboardShortcut(.defaultAction)
                        .disabled(status?.imported == 0)
                } else {
                    Button("Stop Import") { model.cancelRunningImport() }
                        .help("What has been imported so far is kept; Re-import in Settings continues.")
                }
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func summary(_ status: ImportStatus) -> String {
        if let error = status.error { return "The import stopped: \(error)" }
        var parts = ["\(status.imported.formatted()) messages imported"]
        if status.duplicates > 0 { parts.append("\(status.duplicates.formatted()) duplicates skipped") }
        if status.unreadable > 0 { parts.append("\(status.unreadable.formatted()) unreadable entries skipped") }
        let text = parts.joined(separator: ", ") + "."
        return status.cancelled ? text + " Stopped early; Re-import in Settings continues where it left off." : text
    }
}
