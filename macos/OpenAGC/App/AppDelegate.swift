import AppKit

/// Owns app-lifecycle concerns SwiftUI does not cover: the dock, Sparkle,
/// URL handling and the core's lifetime.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {}

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }
}
