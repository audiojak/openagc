import SwiftUI
import UniformTypeIdentifiers

/// A composer window's content (spec §14.5): header fields, the rich-text
/// body, the quoted original, attachments, and Send.
struct ComposerView: View {
    let request: ComposeRequest
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var store: ComposerStore?
    @State private var showsQuote = false
    @State private var importing = false

    var body: some View {
        Group {
            if let store {
                switch store.phase {
                case .loading:
                    ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
                case let .failed(message):
                    ContentUnavailableView("Can't Open Message", systemImage: "exclamationmark.triangle",
                                           description: Text(message))
                default:
                    editor(store)
                }
            } else {
                Color.clear
            }
        }
        .frame(minWidth: 520, minHeight: 380)
        .navigationTitle(store?.windowTitle ?? "New Message")
        .task {
            let store = ComposerStore(core: model.core)
            self.store = store
            await store.load(request)
        }
        .onChange(of: store?.phase) { _, phase in
            if phase == .sent { dismiss() }
        }
        .onDisappear {
            guard let store, store.phase == .editing else { return }
            Task { await store.save() }
        }
    }

    private func editor(_ store: ComposerStore) -> some View {
        @Bindable var store = store
        return VStack(spacing: 0) {
            header(store)
            if let error = store.saveError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 6)
                    .background(.orange.opacity(0.1))
            }
            RichTextEditor(text: $store.body, focusOnAppear: !store.to.isEmpty)
                .frame(maxHeight: .infinity)
            if !store.quotedHTML.isEmpty {
                quote(store)
            }
            if !store.attachments.isEmpty {
                attachmentStrip(store)
            }
        }
        .disabled(store.phase == .sending)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button("Attach", systemImage: "paperclip") { importing = true }
                    .keyboardShortcut("a", modifiers: [.command, .shift])
                    .help("Attach Files")
                Button("Discard", systemImage: "trash") { Task { await store.discard() } }
                    .help("Delete Draft")
                Button("Send", systemImage: "paperplane.fill") { Task { await store.send() } }
                    .keyboardShortcut("d", modifiers: [.command, .shift])
                    .disabled(!store.canSend)
                    .help("Send (⇧⌘D)")
            }
        }
        .fileImporter(isPresented: $importing, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            guard case let .success(urls) = result else { return }
            let scoped = urls.map { ($0, $0.startAccessingSecurityScopedResource()) }
            store.attach(urls)
            for (url, started) in scoped where started { url.stopAccessingSecurityScopedResource() }
        }
        .dropDestination(for: URL.self) { urls, _ in
            let files = urls.filter(\.isFileURL)
            store.attach(files)
            return !files.isEmpty
        }
    }

    private func header(_ store: ComposerStore) -> some View {
        @Bindable var store = store
        let suggest: (String) -> [AddressInfo] = { [core = model.core] text in core?.suggestContactsNow(text) ?? [] }
        return VStack(spacing: 0) {
            row("To:") {
                RecipientField(addresses: $store.to, suggest: suggest, accessibilityLabel: "To")
                if !store.showsCcBcc {
                    Button("Cc/Bcc") { store.showsCcBcc = true }
                        .buttonStyle(.link)
                        .font(.callout)
                }
            }
            if store.showsCcBcc {
                row("Cc:") { RecipientField(addresses: $store.cc, suggest: suggest, accessibilityLabel: "Cc") }
                row("Bcc:") { RecipientField(addresses: $store.bcc, suggest: suggest, accessibilityLabel: "Bcc") }
            }
            row("Subject:") {
                TextField("", text: $store.subject)
                    .textFieldStyle(.plain)
                    .accessibilityLabel("Subject")
            }
            row("From:") {
                Text(store.from).foregroundStyle(.secondary)
                Spacer()
            }
        }
    }

    private func row(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(label)
                    .foregroundStyle(.secondary)
                    .frame(width: 64, alignment: .trailing)
                content()
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            Divider()
        }
    }

    private func quote(_ store: ComposerStore) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            Divider()
            Button {
                showsQuote.toggle()
            } label: {
                Label(showsQuote ? "Hide Quoted Text" : "Show Quoted Text",
                      systemImage: showsQuote ? "chevron.down" : "ellipsis")
                    .font(.callout)
            }
            .buttonStyle(.borderless)
            .padding(.horizontal, 16)
            .padding(.vertical, 6)
            if showsQuote {
                MessageWebView(html: Self.quoteDocument(store.quotedHTML), allowRemoteImages: false)
                    .frame(height: 220)
            }
        }
    }

    private func attachmentStrip(_ store: ComposerStore) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(store.attachments, id: \.path) { attachment in
                    HStack(spacing: 6) {
                        Image(systemName: "doc")
                        Text(attachment.filename).lineLimit(1)
                        Text(ByteCountFormatter.string(fromByteCount: Int64(attachment.size), countStyle: .file))
                            .foregroundStyle(.secondary)
                        Button("Remove", systemImage: "xmark.circle.fill") { store.removeAttachment(attachment) }
                            .labelStyle(.iconOnly)
                            .buttonStyle(.borderless)
                    }
                    .font(.callout)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(.quaternary, in: .capsule)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .overlay(alignment: .top) { Divider() }
    }

    /// The quote was sanitized by the core; show it under the reader's CSP.
    static func quoteDocument(_ html: String) -> String {
        """
        <!doctype html><html><head><meta charset="utf-8">
        <meta http-equiv="Content-Security-Policy" content="\(EmailDocument.contentSecurityPolicy)">
        <meta name="color-scheme" content="light dark">
        <style>body{font:13px -apple-system;margin:8px 16px;color:#666}
        blockquote{margin:0 0 0 4px;padding-left:10px;border-left:2px solid #ccc}</style>
        </head><body>\(html)</body></html>
        """
    }
}
