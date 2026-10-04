import SwiftUI
import UniformTypeIdentifiers

/// A composer window's content (spec §14.5): header fields, the rich-text
/// body, the message being answered (shown by default, in a pane that can
/// be resized or hidden), writing help from the agent, attachments, and
/// Send.
struct ComposerView: View {
    let request: ComposeRequest
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var store: ComposerStore?
    @State private var showsQuote = true
    @State private var importing = false
    @State private var assistant = ComposerAssistant()
    @FocusState private var assistantFocused: Bool
    @State private var factAnswers: [String: String] = [:]
    @State private var saveFacts = true

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
            let store = ComposerStore(core: model.core, account: model.openAccountID)
            self.store = store
            await store.load(request)
            if model.agent.providers.isEmpty { await model.agent.loadProviders() }
            await assistant.loadAudiences(model.core)
        }
        .onChange(of: store?.phase) { _, phase in
            guard phase == .sent else { return }
            if let store, store.didSend, let account = store.accountID ?? model.openAccountID {
                Task { await model.messageSent(heldDraftID: store.heldSend, taskID: store.taskID, accountID: account) }
            }
            dismiss()
        }
        .onDisappear {
            guard let store else { return }
            Task { await store.close() }
        }
    }

    private func editor(_ store: ComposerStore) -> some View {
        @Bindable var store = store
        return VStack(spacing: 0) {
            if let agent = request.agentName {
                Banner("Created by \(agent). Edit it if you like, then approve sending in the agent panel.",
                       systemImage: "sparkles", intent: .info)
            }
            header(store)
                .background {
                    // ⌘Return sends too, as in Gmail (not while reviewing an
                    // agent's draft: that send is the approval card's).
                    if request.agentName == nil {
                        Button("Send") { Task { await store.send() } } // no-help: hidden
                            .keyboardShortcut(.return, modifiers: .command)
                            .disabled(!store.canSend)
                            .hidden()
                    }
                }
            if let error = store.saveError {
                Banner(error, systemImage: "exclamationmark.triangle.fill", intent: .caution)
            }
            if let threadID = store.threadID, store.inReplyTo != nil {
                // A reply: the conversation above it, the latest message
                // open and earlier ones as rows; the divider draggable.
                if showsQuote {
                    VSplitView {
                        VStack(spacing: 0) {
                            ComposerThreadPane(threadID: threadID)
                            conversationToggle
                        }
                        .frame(minHeight: 160, idealHeight: 360, maxHeight: .infinity)
                        .layoutPriority(1)
                        bodyEditor(store)
                            .frame(minHeight: 120, idealHeight: 180, maxHeight: .infinity)
                    }
                } else {
                    conversationToggle
                    bodyEditor(store)
                        .frame(maxHeight: .infinity)
                }
            } else if !store.quotedHTML.isEmpty, showsQuote {
                // The editor and the original, the divider between them
                // draggable.
                VSplitView {
                    bodyEditor(store)
                        .frame(minHeight: 120, maxHeight: .infinity)
                    quote(store)
                        .frame(minHeight: 90, idealHeight: 260, maxHeight: .infinity)
                }
            } else {
                bodyEditor(store)
                    .frame(maxHeight: .infinity)
                if !store.quotedHTML.isEmpty {
                    quoteToggle
                }
            }
            assistantBar(store)
            if !store.attachments.isEmpty {
                attachmentStrip(store)
            }
        }
        .disabled(store.phase == .sending)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button("Attach", systemImage: "paperclip") { importing = true }
                    .keyboardShortcut("a", modifiers: [.command, .shift])
                    .help(ToolbarHelp.composer("Attach")) // toolbar
                // Reviewing an agent's draft: the decision is the approval
                // card's, so sending here would go around it.
                if request.agentName == nil {
                    Button("Discard", systemImage: "trash") { Task { await store.discard() } }
                        .help(ToolbarHelp.composer("Discard")) // toolbar
                    Button("Send", systemImage: "paperplane.fill") { Task { await store.send() } }
                        .keyboardShortcut("d", modifiers: [.command, .shift])
                        .disabled(!store.canSend)
                        .help(ToolbarHelp.composer("Send")) // toolbar
                }
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
        let suggest: (String) -> [AddressInfo] = { [drafts = store.drafts] text in drafts?.suggestContactsNow(text) ?? [] }
        return VStack(spacing: 0) {
            row("To:") {
                // With no one to write to yet (a new message, a forward), the
                // cursor starts here; a reply starts in the body.
                RecipientField(addresses: $store.to, suggest: suggest, accessibilityLabel: "To",
                               focusOnAppear: store.to.isEmpty)
                if !store.showsCcBcc {
                    Button("Cc/Bcc") { store.showsCcBcc = true }
                        .hoverHelp("Add Cc and Bcc fields")
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

    /// The message body; Tab goes to writing help, as Return goes to a new
    /// line (spec §14.5).
    private func bodyEditor(_ store: ComposerStore) -> some View {
        @Bindable var store = store
        return RichTextEditor(text: $store.body, focusOnAppear: !store.to.isEmpty, onTab: { assistantFocused = true })
    }

    /// Shows or hides the conversation above a reply.
    private var conversationToggle: some View {
        HStack {
            Button {
                showsQuote.toggle()
            } label: {
                Label(showsQuote ? "Hide Conversation" : "Show Conversation",
                      systemImage: showsQuote ? "chevron.up" : "chevron.down")
                    .font(.callout)
            }
            .hoverHelp(showsQuote ? "Hide the conversation you are answering" : "Show the conversation you are answering")
            .buttonStyle(.borderless)
            Spacer()
        }
        .padding(.horizontal, Space.xl)
        .padding(.vertical, Space.s)
        .overlay(alignment: .bottom) { InsetRule() }
    }

    private func row(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(spacing: 0) {
            // Top, not first-baseline: asking the recipient token field for
            // its baseline while the user typed made it re-tokenize and ask
            // for layout again, forever (the app hung and ran out of memory
            // forwarding a message, 2026-09-28).
            HStack(alignment: .top, spacing: Space.m) {
                Text(label)
                    .foregroundStyle(.secondary)
                    .frame(width: 64, alignment: .trailing)
                    .padding(.top, Space.hair)
                content()
            }
            .padding(.horizontal, Space.l)
            .padding(.vertical, Space.s)
            InsetRule()
        }
    }

    /// The original, under the editor.
    private func quote(_ store: ComposerStore) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            quoteToggle
            MessageWebView(html: Self.quoteDocument(store.quotedHTML), allowRemoteImages: false)
        }
    }

    private var quoteToggle: some View {
        HStack {
            Button {
                showsQuote.toggle()
            } label: {
                Label(showsQuote ? "Hide Original" : "Show Original",
                      systemImage: showsQuote ? "chevron.down" : "chevron.up")
                    .font(.callout)
            }
            .hoverHelp(showsQuote ? "Hide the message you are answering" : "Show the message you are answering")
            .buttonStyle(.borderless)
            Spacer()
        }
        .padding(.horizontal, Space.xl)
        .padding(.vertical, Space.s)
        .overlay(alignment: .top) { InsetRule() }
    }

    /// Writing help: ask the agent to write or change the message.
    private func assistantBar(_ store: ComposerStore) -> some View {
        @Bindable var assistant = assistant
        let ready = model.agent.isProviderReady
        let name = model.agent.providerName
        let run = { Task { await assistant.run(store: store, model: model, original: ComposerAssistant.plainText(fromHTML: store.quotedHTML)) } }
        return VStack(alignment: .leading, spacing: Space.xs) {
            HStack(spacing: Space.m) {
                Image(systemName: "sparkles").foregroundStyle(.tint)
                // As large as the main window's prompt, and it grows with
                // what is typed (⌥Return for a new line).
                TextField(ready ? "Ask \(name) to write or change this message…" : "\(name) is not set up (Settings › Agents)",
                          text: $assistant.instruction, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(1...6)
                    .focused($assistantFocused)
                    .onSubmit { run() }
                    .disabled(!ready || assistant.state == .working)
                    .accessibilityLabel("Writing help")
                Menu {
                    ForEach(ComposerAssistant.suggestions(replying: !store.quotedHTML.isEmpty), id: \.self) { suggestion in
                        Button(suggestion) {
                            assistant.instruction = suggestion
                            assistantFocused = true
                        }
                    }
                } label: {
                    Image(systemName: "text.bubble")
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                .disabled(!ready)
                .hoverHelp("Ideas to ask for; choosing one puts it in the box")
                if assistant.state == .working {
                    ProgressView().controlSize(.small)
                    Button("Stop", systemImage: "stop.circle.fill") { assistant.cancel() }
                        .labelStyle(.iconOnly)
                        .buttonStyle(.borderless)
                        .hoverHelp("Stop the agent writing")
                } else {
                    Button("Write", systemImage: "arrow.up.circle.fill") { run() }
                        .labelStyle(.iconOnly)
                        .buttonStyle(.borderless)
                        .disabled(!ready || assistant.instruction.trimmingCharacters(in: .whitespaces).isEmpty)
                        .hoverHelp("Have \(name) write this into the message (Return)")
                }
            }
            .font(.body)
            .controlSize(.regular)
            .glassCapsule()
            switch assistant.state {
            case .done:
                HStack(spacing: Space.m) {
                    Text(assistant.followsGuide
                         ? "Written by \(name), following your writing guide. Read it before sending."
                         : "Written by \(name). Read it before sending.").foregroundStyle(.secondary)
                    audienceMenu(store)
                    Button("Undo") { assistant.undo() }
                        .buttonStyle(.link)
                        .hoverHelp("Put back the message as it was before")
                }
                .font(TypeRole.caption)
                if !assistant.checkFailures.isEmpty {
                    Label(assistant.checkFailures.joined(separator: "; "), systemImage: "exclamationmark.triangle")
                        .font(TypeRole.caption)
                        .foregroundStyle(Tone.caution)
                        .fixedSize(horizontal: false, vertical: true)
                }
            case let .asking(questions):
                factQuestions(questions)
            case let .failed(message):
                Text(message).font(TypeRole.caption).foregroundStyle(Tone.failure)
            default:
                // An agent's draft under review can be rewritten for an audience.
                if request.agentName != nil {
                    audienceMenu(store).font(TypeRole.caption)
                }
            }
        }
        .controlSize(.small)
        .padding(.horizontal, Space.l)
        .padding(.vertical, Space.s)
        .overlay(alignment: .top) { InsetRule() }
    }

    /// The facts the agent needs before it drafts: a field for each, and
    /// whether to keep the answers in the writing guide.
    private func factQuestions(_ questions: [ComposerAssistant.FactQuestion]) -> some View {
        let name = model.agent.providerName
        return VStack(alignment: .leading, spacing: Space.s) {
            Text("\(name) needs a few facts before it writes this. Answer what you can.")
                .font(TypeRole.caption)
                .foregroundStyle(.secondary)
            ForEach(questions) { q in
                VStack(alignment: .leading, spacing: Space.hair) {
                    Text(q.question).font(TypeRole.caption.weight(.medium))
                    TextField(q.fact, text: Binding(get: { factAnswers[q.id] ?? "" }, set: { factAnswers[q.id] = $0 }),
                              axis: .vertical)
                        .textFieldStyle(.roundedBorder)
                        .lineLimit(1...4)
                        .accessibilityLabel(q.question)
                }
            }
            HStack(spacing: Space.m) {
                Toggle("Keep these facts in my writing guide", isOn: $saveFacts)
                    .toggleStyle(.checkbox)
                    .hoverHelp("Later drafts use them without asking; each can be changed or removed in the Writing Guide")
                Spacer(minLength: Space.m)
                CancelButton(help: "Stop; the message stays as it is (Esc)") {
                    assistant.cancel()
                    factAnswers = [:]
                }
                Button("Skip") { answerFacts([:]) }
                    .hoverHelp("Write it now, leaving [brackets] for what is not known")
                Button("Write") { answerFacts(factAnswers) }
                    .keyboardShortcut(.defaultAction)
                    .hoverHelp("Write the message with these answers")
            }
        }
    }

    private func answerFacts(_ answers: [String: String]) {
        let save = saveFacts
        factAnswers = [:]
        Task { await assistant.answer(answers, save: save) }
    }

    /// "Written for Customers": another audience writes a new draft under
    /// its guidelines (spec §14.9).
    @ViewBuilder private func audienceMenu(_ store: ComposerStore) -> some View {
        if !assistant.audienceChoices.isEmpty {
            let current = assistant.writtenFor
            Menu(current.isEmpty ? "Written for everyone" : "Written for \(current.joined(separator: ", "))") {
                Button("The Recipients' Own") { switchAudience(nil, store) } // no-help: menu
                Divider() // menu
                ForEach(assistant.audienceChoices, id: \.self) { name in
                    Button(name) { switchAudience([name], store) } // no-help: menu
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .disabled(assistant.state == .working)
            .hoverHelp("Write it for another audience, following that audience's guidelines")
        }
    }

    private func switchAudience(_ audiences: [String]?, _ store: ComposerStore) {
        Task { await assistant.switchAudience(to: audiences, store: store, model: model) }
    }

    private func attachmentStrip(_ store: ComposerStore) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: Space.m) {
                ForEach(store.attachments, id: \.path) { attachment in
                    HStack(spacing: Space.s) {
                        Image(systemName: "doc")
                        Text(attachment.filename).lineLimit(1)
                        Text(ByteCountFormatter.string(fromByteCount: Int64(attachment.size), countStyle: .file))
                            .foregroundStyle(.secondary)
                        Button("Remove", systemImage: "xmark.circle.fill") { store.removeAttachment(attachment) }
                            .hoverHelp("Remove \(attachment.filename) from the message")
                            .labelStyle(.iconOnly)
                            .buttonStyle(.borderless)
                    }
                    .font(.callout)
                    .padding(.horizontal, Space.m)
                    .padding(.vertical, Space.xs)
                    .background(.quaternary, in: .capsule)
                }
            }
            .padding(.horizontal, Space.l)
            .padding(.vertical, Space.m)
        }
        .overlay(alignment: .top) { InsetRule() }
    }

    /// The quote was sanitized by the core; show it under the reader's CSP.
    static func quoteDocument(_ html: String) -> String {
        """
        <!doctype html><html><head><meta charset="utf-8">
        <meta http-equiv="Content-Security-Policy" content="\(EmailDocument.contentSecurityPolicy)">
        <meta name="color-scheme" content="light dark">
        <style>:root{color-scheme:light dark}
        body{font:13px -apple-system;margin:8px 16px;background:Canvas;color:GrayText}
        blockquote{margin:0 0 0 4px;padding-left:10px;border-left:2px solid color-mix(in srgb, CanvasText 25%, transparent)}
        a{color:LinkText}</style>
        </head><body>\(html)</body></html>
        """
    }
}
