import SwiftUI
import os

@main
struct OpenAGCApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model = AppModel(core: OpenAGCApp.makeCore())

    var body: some Scene {
        WindowGroup("OpenAGC", id: "main") {
            MainWindow()
                .environment(model)
                .onAppear { appDelegate.model = model }
        }
        .defaultSize(width: 1200, height: 760)
        .commands { MailCommands(model: model) }

        WindowGroup("New Message", id: "compose", for: ComposeRequest.self) { $request in
            if let request {
                ComposerView(request: request)
                    .environment(model)
            }
        }
        .defaultSize(width: 720, height: 560)
        .commandsRemoved()

        Settings {
            SettingsView()
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

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Message") { model.compose(.new(to: nil)) }
                .keyboardShortcut("n")
        }
        CommandMenu("Message") {
            Button("Reply") { model.reply(all: false) }
                .keyboardShortcut("r")
                .disabled(model.replyTargetMessageID == nil)
            Button("Reply All") { model.reply(all: true) }
                .keyboardShortcut("r", modifiers: [.command, .shift])
                .disabled(model.replyTargetMessageID == nil)
            Button("Forward") { model.forward() }
                .keyboardShortcut("f", modifiers: [.command, .shift])
                .disabled(model.replyTargetMessageID == nil)
        }
        TextFormattingCommands()
    }
}
