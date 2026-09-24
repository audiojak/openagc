import AppKit
import os

/// Headless UI verification without Screen Recording permission: launched
/// with `-OpenAGCSnapshot /path.png`, the app renders its own main window
/// to a PNG and quits. Options (all user defaults, so also launch args):
///   -OpenAGCSnapshotDelay <seconds>     wait before capturing (default 3)
///   -OpenAGCSnapshotSelectFirst YES     select the first thread first
///   -OpenAGCSnapshotMailbox <id>        switch mailbox first
///   -OpenAGCSnapshotSelectIndex <n>     select row n instead
///   -OpenAGCSnapshotAppearance dark|light
///   -OpenAGCSnapshotSearch <query>      type a search first
///   -OpenAGCSnapshotMode layer          render the CALayer tree instead
///                                       (catches layer-only SwiftUI content)
@MainActor
enum Snapshot {
    private static let logger = Logger(subsystem: "ai.actual.openagc", category: "snapshot")

    static func scheduleIfRequested(delegate: AppDelegate) {
        let defaults = UserDefaults.standard
        guard let path = defaults.string(forKey: "OpenAGCSnapshot") else { return }
        let delay = defaults.object(forKey: "OpenAGCSnapshotDelay") as? Double
            ?? Double(defaults.string(forKey: "OpenAGCSnapshotDelay") ?? "") ?? 3
        switch defaults.string(forKey: "OpenAGCSnapshotAppearance") {
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        default: break
        }
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(delay / 2))
            if let mailbox = defaults.string(forKey: "OpenAGCSnapshotMailbox") {
                delegate.model?.selectedMailboxID = mailbox
                try? await Task.sleep(for: .milliseconds(500))
            }
            if let query = defaults.string(forKey: "OpenAGCSnapshotSearch") {
                delegate.model?.searchText = query
                try? await Task.sleep(for: .milliseconds(500))
            }
            if let rows = delegate.model?.threads.rows, !rows.isEmpty {
                if let index = Int(defaults.string(forKey: "OpenAGCSnapshotSelectIndex") ?? ""), rows.indices.contains(index) {
                    delegate.model?.selectedThreadID = rows[index].id
                } else if defaults.bool(forKey: "OpenAGCSnapshotSelectFirst") {
                    delegate.model?.selectedThreadID = rows[0].id
                }
            }
            try? await Task.sleep(for: .seconds(delay / 2))
            if let model = delegate.model {
                FileHandle.standardError.write(Data("snapshot state: \(model.accountState) rows=\(model.threads.rows.count)\n".utf8))
            } else {
                FileHandle.standardError.write(Data("snapshot state: no model\n".utf8))
            }
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
        if UserDefaults.standard.string(forKey: "OpenAGCSnapshotMode") == "layer",
           let layer = view.layer, let context = NSGraphicsContext(bitmapImageRep: rep) {
            context.cgContext.scaleBy(x: CGFloat(rep.pixelsWide) / view.bounds.width,
                                      y: CGFloat(rep.pixelsHigh) / view.bounds.height)
            layer.render(in: context.cgContext)
        } else {
            view.cacheDisplay(in: view.bounds, to: rep)
        }
        guard let png = rep.representation(using: .png, properties: [:]) else { return }
        do {
            try png.write(to: url)
            logger.info("snapshot written to \(url.path, privacy: .public)")
        } catch {
            logger.error("snapshot failed: \(error.localizedDescription, privacy: .public)")
        }
    }
}
