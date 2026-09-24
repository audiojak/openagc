import SwiftUI
import os

@main
struct OpenAGCApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model = AppModel(core: OpenAGCApp.makeCore())
    @State private var updater = Updater()

    var body: some Scene {
        WindowGroup("OpenAGC", id: "main") {
            MainWindow()
                .environment(model)
                .onAppear { appDelegate.model = model }
        }
        .defaultSize(width: 1200, height: 760)
        .commands {
            MailCommands(model: model)
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…") { updater.checkForUpdates() }
                    .disabled(!updater.canCheckForUpdates)
            }
        }

        WindowGroup("New Message", id: "compose", for: ComposeRequest.self) { $request in
            if let request {
                ComposerView(request: request)
                    .environment(model)
            }
        }
        .defaultSize(width: 720, height: 560)
        .commandsRemoved()

        Window("Keyboard Shortcuts", id: "shortcuts") {
            KeyboardShortcutsView()
        }
        .windowResizability(.contentSize)

        Window("Routines", id: "routines") {
            RoutinesWindow()
                .environment(model)
        }
        .defaultSize(width: 980, height: 720)

        Settings {
            SettingsView()
                .environment(model)
                .environment(updater)
        }
    }

    private static func makeCore() -> CoreClient? {
        do {
            return try CoreClient(dataDirectory: CoreClient.defaultDataDirectory(),
                                  logDirectory: CoreClient.defaultLogDirectory())
        } catch {
            Logger(subsystem: "ai.actual.openagc", category: "app")
                .fault("core failed to start: \(String(describing: error), privacy: .public)")
            return nil
        }
    }
}

/// Menu commands for mail (spec §14.6). Reply and Forward act on the
/// thread being read.
struct MailCommands: Commands {
    let model: AppModel
    /// Set only while the main window is key, so a shortcut typed in a
    /// composer (⌘⌫ deletes to line start there) never acts on the
    /// selection behind it.
    @FocusedValue(\.isMailWindow) private var isMailWindow
    @Environment(\.openWindow) private var openWindow

    private var mailKey: Bool { isMailWindow == true && model.isMailOpen }
    private var noTargets: Bool { !mailKey || model.actionTargets.isEmpty }
    private var noReplyTarget: Bool { !mailKey || model.replyTargetMessageID == nil }

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Message") { model.compose(.new(to: nil)) }
                .keyboardShortcut("n")
        }
        CommandGroup(after: .textEditing) {
            Button("Search Mail") { model.focusSearch() }
                .keyboardShortcut("f")
                .disabled(!mailKey)
        }
        CommandGroup(replacing: .help) {
            Button("Keyboard Shortcuts") { openWindow(id: "shortcuts") }
                .keyboardShortcut("/", modifiers: [.command, .shift])
            Link("OpenAGC on GitHub", destination: URL(string: "https://github.com/audiojak/openagc")!)
        }
        CommandGroup(after: .windowList) {
            Button("Routines") { openWindow(id: "routines") }
                .keyboardShortcut("r", modifiers: [.command, .option])
        }
        CommandGroup(before: .sidebar) {
            ForEach(Array(Self.mailboxShortcuts.enumerated()), id: \.offset) { index, item in
                Button(item.title) { model.selectedMailboxID = item.id }
                    .keyboardShortcut(KeyEquivalent(Character(String(index + 1))))
                    .disabled(!mailKey)
            }
            Divider()
            Button("Check for New Mail") { model.core?.syncNow() }
                .keyboardShortcut("n", modifiers: [.command, .shift])
                .disabled(!mailKey)
            Divider()
        }
        CommandMenu("Message") {
            Button("Reply") { model.reply(all: false) }
                .keyboardShortcut("r")
                .disabled(noReplyTarget)
            Button("Reply All") { model.reply(all: true) }
                .keyboardShortcut("r", modifiers: [.command, .shift])
                .disabled(noReplyTarget)
            Button("Forward") { model.forward() }
                .keyboardShortcut("f", modifiers: [.command, .shift])
                .disabled(noReplyTarget)
            Divider()
            Button("Archive") { model.archiveSelection() }
                .keyboardShortcut("a", modifiers: [.command, .control])
                .disabled(noTargets)
            Button("Move to Inbox") { model.moveSelectionToInbox() }
                .keyboardShortcut("i", modifiers: [.command, .control])
                .disabled(noTargets || model.selectedMailboxID == "INBOX")
            Button("Move to Trash") { model.trashSelection() }
                .keyboardShortcut(.delete)
                .disabled(noTargets)
            Divider()
            Button("Mark as Read or Unread") { model.toggleReadSelection() }
                .keyboardShortcut("u", modifiers: [.command, .shift])
                .disabled(noTargets)
            Button("Star or Unstar") { model.toggleStarSelection() }
                .keyboardShortcut("l", modifiers: [.command, .shift])
                .disabled(noTargets)
            Divider()
            Button("Ask \(model.agent.providerName)…") { model.focusAgentPrompt() }
                .keyboardShortcut("k")
                .disabled(!mailKey)
            Button(model.agent.isPresented ? "Hide Agent" : "Show Agent") { model.agent.isPresented.toggle() }
                .keyboardShortcut("i", modifiers: [.command, .option])
                .disabled(!mailKey)
            Divider()
            Button("Load Remote Images") { model.reader.loadRemoteImagesForThread() }
                .keyboardShortcut("i", modifiers: [.command, .shift])
                .disabled(!mailKey || !model.reader.hasRemoteImages || model.reader.allowsRemoteImages)
        }
        TextFormattingCommands()
    }

    /// `⌘1`… in the View menu, in sidebar order.
    static let mailboxShortcuts: [(title: String, id: String)] = [
        ("Inbox", "INBOX"), ("Starred", "STARRED"), ("Sent", "SENT"),
        ("Drafts", "DRAFT"), ("Archive", "@archive"), ("Trash", "TRASH"),
    ]
}

extension FocusedValues {
    /// True in the main mail window's scene.
    @Entry var isMailWindow: Bool?
}
