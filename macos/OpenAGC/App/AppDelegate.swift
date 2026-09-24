import AppKit
import os

/// Owns app-lifecycle concerns SwiftUI does not cover: the dock, Sparkle,
/// URL handling, and the self-snapshot used for headless UI checks.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    weak var model: AppModel?

    func applicationDidFinishLaunching(_ notification: Notification) {
        Snapshot.scheduleIfRequested(delegate: self)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }
}
