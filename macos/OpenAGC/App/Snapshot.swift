import AppKit
import os

/// Headless UI verification without Screen Recording permission: launched
/// with `-OpenAGCSnapshot /path.png`, the app renders its own main window
/// to a PNG and quits. Options (all user defaults, so also launch args):
///   -OpenAGCSnapshotDelay <seconds>     wait before capturing (default 3)
///   -OpenAGCSnapshotSelectFirst YES     select the first thread first
///   -OpenAGCSnapshotMailbox <id>        switch mailbox first
@MainActor
enum Snapshot {
    private static let logger = Logger(subsystem: "ai.actual.openagc", category: "snapshot")

    static func scheduleIfRequested(delegate: AppDelegate) {
        let defaults = UserDefaults.standard
        guard let path = defaults.string(forKey: "OpenAGCSnapshot") else { return }
        let delay = defaults.object(forKey: "OpenAGCSnapshotDelay") as? Double
            ?? Double(defaults.string(forKey: "OpenAGCSnapshotDelay") ?? "") ?? 3
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(delay / 2))
            if let mailbox = defaults.string(forKey: "OpenAGCSnapshotMailbox") {
                delegate.model?.selectedMailboxID = mailbox
                try? await Task.sleep(for: .milliseconds(500))
            }
            if defaults.bool(forKey: "OpenAGCSnapshotSelectFirst"), let first = delegate.model?.threads.rows.first {
                delegate.model?.selectedThreadID = first.id
            }
            try? await Task.sleep(for: .seconds(delay / 2))
            capture(to: URL(filePath: path))
            NSApp.terminate(nil)
        }
    }

    private static func capture(to url: URL) {
        guard let window = NSApp.windows.first(where: { $0.isVisible && !($0 is NSPanel) }),
              let view = window.contentView?.superview ?? window.contentView,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
        else {
            logger.error("no window to snapshot")
            return
        }
        view.cacheDisplay(in: view.bounds, to: rep)
        guard let png = rep.representation(using: .png, properties: [:]) else { return }
        do {
            try png.write(to: url)
            logger.info("snapshot written to \(url.path, privacy: .public)")
        } catch {
            logger.error("snapshot failed: \(error.localizedDescription, privacy: .public)")
        }
    }
}
