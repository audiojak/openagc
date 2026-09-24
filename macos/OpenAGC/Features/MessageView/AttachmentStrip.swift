import AppKit
import QuickLook
import SwiftUI
import UniformTypeIdentifiers

/// The thread's attachments under the reader header (spec §14.3). Nothing
/// downloads until an attachment is used: click to preview with Quick Look,
/// double-click to open, drag to copy it out, or right-click for more.
struct AttachmentStrip: View {
    let attachments: [AttachmentInfo]
    @Environment(AppModel.self) private var model
    @State private var previewURL: URL?
    @State private var loading: Set<String> = []
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(attachments, id: \.id) { attachment in
                        chip(attachment)
                    }
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 2)
            }
            if let error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .padding(.horizontal, 20)
            }
        }
        .padding(.bottom, 6)
        .quickLookPreview($previewURL)
    }

    private func chip(_ attachment: AttachmentInfo) -> some View {
        HStack(spacing: 6) {
            if loading.contains(attachment.id) {
                ProgressView().controlSize(.mini)
            } else {
                Image(nsImage: Self.icon(for: attachment))
                    .resizable()
                    .frame(width: 16, height: 16)
            }
            Text(attachment.filename)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: 220, alignment: .leading)
            Text(ByteCountFormatter.string(fromByteCount: Int64(attachment.size), countStyle: .file))
                .foregroundStyle(.secondary)
        }
        .font(.callout)
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        .background(.quaternary.opacity(0.6), in: .rect(cornerRadius: 6))
        .contentShape(.rect)
        .onTapGesture(count: 2) { perform(attachment) { NSWorkspace.shared.open($0) } }
        .onTapGesture { perform(attachment) { previewURL = $0 } }
        .onDrag { dragProvider(attachment) }
        .contextMenu {
            Button("Quick Look") { perform(attachment) { previewURL = $0 } }
            Button("Open") { perform(attachment) { NSWorkspace.shared.open($0) } }
            Button("Save As…") { perform(attachment) { save($0) } }
            Button("Show in Finder") { perform(attachment) { NSWorkspace.shared.activateFileViewerSelecting([$0]) } }
        }
        .help("\(attachment.filename) — click to preview, double-click to open")
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction(named: "Open") { perform(attachment) { NSWorkspace.shared.open($0) } }
    }

    /// Fetch (or find cached) and hand the file URL to `action`.
    private func perform(_ attachment: AttachmentInfo, _ action: @escaping (URL) -> Void) {
        guard let core = model.core, !loading.contains(attachment.id) else { return }
        error = nil
        loading.insert(attachment.id)
        Task {
            defer { loading.remove(attachment.id) }
            do {
                action(try await core.attachmentFile(attachment.id).url)
            } catch {
                self.error = "Couldn’t get “\(attachment.filename)”: \((error as? CoreClientError)?.message ?? error.localizedDescription)"
            }
        }
    }

    private func save(_ url: URL) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = url.lastPathComponent
        panel.canCreateDirectories = true
        guard panel.runModal() == .OK, let destination = panel.url else { return }
        do {
            if FileManager.default.fileExists(atPath: destination.path) {
                try FileManager.default.removeItem(at: destination)
            }
            try FileManager.default.copyItem(at: url, to: destination)
            CoreClient.quarantine(destination)
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// A file promise: the download starts only when the drop lands.
    private func dragProvider(_ attachment: AttachmentInfo) -> NSItemProvider {
        let provider = NSItemProvider()
        provider.suggestedName = attachment.filename
        let type = UTType(mimeType: attachment.mimeType) ?? UTType(filenameExtension:
            (attachment.filename as NSString).pathExtension) ?? .data
        let core = model.core
        let id = attachment.id
        provider.registerFileRepresentation(forTypeIdentifier: type.identifier, fileOptions: [], visibility: .all) { done in
            Task {
                do {
                    guard let core else { throw CoreClientError(kind: .internalError, message: "no core") }
                    done(try await core.attachmentFile(id).url, false, nil)
                } catch {
                    done(nil, false, error)
                }
            }
            return nil
        }
        return provider
    }

    private static func icon(for attachment: AttachmentInfo) -> NSImage {
        let type = UTType(mimeType: attachment.mimeType)
            ?? UTType(filenameExtension: (attachment.filename as NSString).pathExtension) ?? .data
        return NSWorkspace.shared.icon(for: type)
    }
}
