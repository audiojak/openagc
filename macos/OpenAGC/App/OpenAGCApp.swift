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
