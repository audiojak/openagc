import AppKit
import os

/// Headless UI verification without Screen Recording permission: launched
/// with `-OpenAGCSnapshot /path.png`, the app renders its own main window
/// to a PNG and quits. Always pair it with `-OpenAGCDataDirectory <tmp>`
/// and `-OpenAGCDemo YES` so no real account is opened (see
/// scripts/snapshot.sh). Options (all user defaults, so also launch args):
///   -OpenAGCSnapshotDelay <seconds>     wait before capturing (default 3)
///   -OpenAGCSnapshotSelectFirst YES     select the first thread first
///   -OpenAGCSnapshotMailbox <id>        switch mailbox first
///   -OpenAGCSnapshotSelectIndex <n>     select row n instead
///   -OpenAGCSnapshotAppearance dark|light
///   -OpenAGCSnapshotSearch <query>      type a search first
///   -OpenAGCSnapshotCompose new|reply|forward   open a composer and
///                                       capture it instead
///   -OpenAGCSnapshotAgentPrompt <text>  ask the agent first (use with
///                                       -OpenAGCFakeAgents YES)
///   -OpenAGCSnapshotProposal <summary>  show a sample approval card
///   -OpenAGCSnapshotWidth <points>      resize the main window first
///   -OpenAGCSnapshotRoutine <runner>    open the Routines window (creating
///                                       a routine if there is none) and
///                                       capture it
///   -OpenAGCSnapshotMode pdf            draw through AppKit's PDF (print) path
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
            // Some AppKit animations (split view items) only run in the
            // active app.
            NSApp.activate()
            if let width = Double(defaults.string(forKey: "OpenAGCSnapshotWidth") ?? ""),
               let main = NSApp.windows.first(where: { $0.isVisible && !($0 is NSPanel) }) {
                var frame = main.frame
                frame.size.width = width
                main.setFrame(frame, display: true)
            }
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
            if let prompt = defaults.string(forKey: "OpenAGCSnapshotAgentPrompt"), let model = delegate.model {
                await model.agent.loadProviders()
                await model.askAgent(prompt)
                try? await Task.sleep(for: .milliseconds(800))
                // A sample approval card: the scripted agent cannot call tools.
                if let summary = defaults.string(forKey: "OpenAGCSnapshotProposal"), let session = model.agent.sessionID {
                    await model.agent.apply(sessionID: session, events: [
                        .actionProposed(actionId: 1, tool: "mail_send", summary: summary, draftId: 1),
                    ])
                }
            }
            var window: NSWindow?
            if let runner = defaults.string(forKey: "OpenAGCSnapshotRoutine"), let model = delegate.model {
                if model.routines.routines.isEmpty {
                    await model.routines.create(runner: RoutineRunner(rawValue: runner) ?? .claudeCloud)
                }
                model.openRoutines?()
                try? await Task.sleep(for: .milliseconds(800))
                window = NSApp.windows.last { $0.isVisible && ($0.identifier?.rawValue.hasPrefix("routines") ?? false) }
            }
            if let compose = defaults.string(forKey: "OpenAGCSnapshotCompose"), let model = delegate.model {
                try? await Task.sleep(for: .milliseconds(500))
                switch compose {
                case "reply": model.reply(all: true)
                case "forward": model.forward()
                default: model.compose(.new(to: nil))
                }
                try? await Task.sleep(for: .milliseconds(500))
                // The app may not be active when launched from a script, so
                // there is no key window; find the composer by its scene id.
                window = NSApp.windows.last { $0.isVisible && ($0.identifier?.rawValue.hasPrefix("compose") ?? false) }
                FileHandle.standardError.write(Data("snapshot composer window: \(window != nil)\n".utf8))
            }
            try? await Task.sleep(for: .seconds(delay / 2))
            if let model = delegate.model {
                FileHandle.standardError.write(Data("snapshot state: \(model.accountState) rows=\(model.threads.rows.count) agent=\(model.agent.isPresented)/\(model.agent.entries.count)/\(model.agent.providers.count)\n".utf8))
            } else {
                FileHandle.standardError.write(Data("snapshot state: no model\n".utf8))
            }
            if defaults.bool(forKey: "OpenAGCSnapshotDumpViews"), let root = (window ?? NSApp.windows.first)?.contentView?.superview {
                dump(root, depth: 0)
            }
            capture(window, to: URL(filePath: path))
            NSApp.terminate(nil)
        }
    }

    /// Debugging layouts: the view tree with frames, to stderr.
    private static func dump(_ view: NSView, depth: Int) {
        guard depth < 14 else { return }
        var detail = ""
        if let table = view as? NSTableView { detail = " rows=\(table.numberOfRows)" }
        if let outline = view as? NSOutlineView {
            let expanded = (0..<outline.numberOfRows).filter { outline.isItemExpanded(outline.item(atRow: $0)) }
            detail += " expandedRows=\(expanded)"
        }
        if let text = view as? NSTextField, !text.stringValue.isEmpty { detail = " \"\(text.stringValue)\"" }
        let line = String(repeating: "  ", count: depth) + "\(type(of: view)) \(view.frame.integral) hidden=\(view.isHidden)\(detail)\n"
        FileHandle.standardError.write(Data(line.utf8))
        for sub in view.subviews { dump(sub, depth: depth + 1) }
    }

    private static func capture(_ preferred: NSWindow?, to url: URL) {
        guard let window = preferred ?? NSApp.windows.first(where: { $0.isVisible && !($0 is NSPanel) }),
              let view = window.contentView?.superview ?? window.contentView,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
        else {
            logger.error("no window to snapshot")
            return
        }
        if UserDefaults.standard.string(forKey: "OpenAGCSnapshotMode") == "pdf" {
            // AppKit's print path draws some SwiftUI content that bitmap
            // caching misses on macOS 26.
            let pdf = view.dataWithPDF(inside: view.bounds)
            guard let image = NSImage(data: pdf),
                  let tiff = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff),
                  let png = bitmap.representation(using: .png, properties: [:]) else { return }
            try? png.write(to: url)
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
