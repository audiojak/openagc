import AppKit
import os

/// Owns app-lifecycle concerns SwiftUI does not cover: the dock, Sparkle,
/// URL handling and the core's lifetime.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let logger = Logger(subsystem: "ai.actual.openagc", category: "app")
    private(set) var core: CoreClient?

    func applicationDidFinishLaunching(_ notification: Notification) {
        do {
            let client = try CoreClient(dataDirectory: CoreClient.defaultDataDirectory())
            logger.info("core \(client.version, privacy: .public) ready")
            core = client
        } catch {
            logger.fault("core failed to start: \(String(describing: error), privacy: .public)")
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }
}
