import AppKit
import Foundation
import Observation
import os

/// The routines window's state (spec §11.5).
@MainActor
@Observable
final class RoutinesStore {
    private(set) var routines: [RoutineInfo] = []
    /// The latest runs per routine, for the list's activity line.
    private(set) var latestRuns: [String: RoutineRunInfo] = [:]
    /// The list's selection. Setting it loads the routine in the background;
    /// code that needs the draft at once calls `select(_:)`.
    var selectedID: String? {
        didSet { if selectedID != oldValue && !selecting { Task { await select() } } }
    }
    @ObservationIgnored private var selecting = false
    /// The routine being edited; saved explicitly.
    var draft: RoutineDefinition?
    private(set) var savedDraft: RoutineDefinition?
    private(set) var runs: [RoutineRunInfo] = []
    private(set) var busy: String?
    private(set) var message: String?
    private(set) var error: String?
    /// Set when publishing needs the paste hand-off.
    var handoff: RoutineHandoff?
    private(set) var preview: [RoutinePreviewRow]?
    private(set) var previewSession: String?
    /// Threads matching the scope right now.
    private(set) var scopeCount: Int?

    @ObservationIgnored private let core: CoreClient?
    @ObservationIgnored private let logger = Logger(subsystem: "ai.actual.openagc", category: "routines")

    init(core: CoreClient?) {
        self.core = core
    }

    var hasUnsavedChanges: Bool { draft != nil && draft != savedDraft }
    var selected: RoutineInfo? { routines.first { $0.id == selectedID } }
    var runner: RoutineRunner { RoutineRunner(rawValue: draft?.runner ?? "local") ?? .local }

    // MARK: Loading

    func load() async {
        guard let core else { return }
        routines = (try? await core.routines()) ?? []
        for r in routines {
            latestRuns[r.id] = try? await core.routineRuns(r.id, limit: 1).first
        }
        if selectedID == nil || !routines.contains(where: { $0.id == selectedID }) {
            await select(routines.first?.id)
        } else {
            await reloadRuns()
        }
    }

    /// Select a routine and load it before returning.
    func select(_ id: String?) async {
        selecting = true
        selectedID = id
        selecting = false
        await select()
    }

    private func select() async {
        preview = nil
        previewSession = nil
        error = nil
        message = nil
        guard let info = selected else {
            draft = nil
            savedDraft = nil
            runs = []
            return
        }
        do {
            let def = try RoutineDefinition.decode(info.definitionJson)
            draft = def
            savedDraft = def
        } catch {
            self.error = "This routine could not be read: \(error.localizedDescription)"
        }
        await reloadRuns()
        await countScope()
    }

    func reloadRuns() async {
        guard let core, let id = selectedID else { return }
        runs = (try? await core.routineRuns(id)) ?? []
        latestRuns[id] = runs.first
    }

    func countScope() async {
        guard let core, let scope = draft?.scope, !scope.isEmpty else { scopeCount = nil; return }
        scopeCount = (try? await core.search(scope, limit: 500))?.rows.count
    }

    // MARK: Editing

    func create(runner: RoutineRunner) async {
        await perform("Creating…") { core in
            let info = try await core.createRoutineFromTemplate(runner: runner.rawValue)
            await self.load()
            await self.select(info.id)
        }
    }

    func save() async {
        guard let draft else { return }
        await perform("Saving…") { core in
            let info = try await core.saveRoutine(json: try draft.encoded())
            self.savedDraft = try RoutineDefinition.decode(info.definitionJson)
            self.draft = self.savedDraft
            await self.load()
            self.message = info.changedSincePublish ? "Saved. Publish to update the cloud copy." : "Saved."
        }
    }

    func revert() {
        draft = savedDraft
    }

    func delete() async {
        guard let id = selectedID else { return }
        await perform("Deleting…") { core in
            try await core.deleteRoutine(id)
            await self.select(nil)
            await self.load()
        }
    }

    func setEnabled(_ id: String, _ enabled: Bool) async {
        await perform(nil) { core in
            _ = try await core.setRoutineEnabled(id, enabled)
            await self.load()
            if self.selectedID == id { self.draft?.enabled = enabled; self.savedDraft?.enabled = enabled }
        }
    }

    // MARK: Running and publishing

    /// Local: start a run (it shows in the agent panel). Cloud: fire it.
    func runNow(model: AppModel) async {
        guard let id = selectedID else { return }
        await perform("Starting…") { core in
            switch self.runner {
            case .local:
                _ = try await core.runRoutineNow(id)
                self.message = "Running. Follow it in the agent panel."
            case .claudeCloud:
                _ = try await core.runCloudRoutineNow(id)
                self.message = "Started on Claude. Results appear here and in your inbox in a few minutes."
                model.core?.syncNow()
            default:
                self.message = "Run it from \(self.runner.title)."
            }
            await self.reloadRuns()
        }
    }

    /// Claude cloud through the CLI; on failure, the paste hand-off.
    func publish() async {
        guard let id = selectedID else { return }
        if hasUnsavedChanges { await save() }
        switch runner {
        case .claudeCloud:
            busy = "Publishing to Claude…"
            defer { busy = nil }
            do {
                _ = try await core?.publishRoutineToCloud(id)
                message = "Published to Claude."
                await load()
            } catch {
                logger.warning("publish failed: \(error.message, privacy: .private)")
                self.error = "Couldn't publish through Claude Code: \(error.message). You can set it up by hand instead."
                handoff = try? await core?.routineHandoff(id)
            }
        default:
            handoff = try? await core?.routineHandoff(id)
        }
    }

    /// Put the hand-off prompt on the clipboard and open the page.
    func copyAndOpen(_ handoff: RoutineHandoff) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(handoff.prompt, forType: .string)
        if let url = URL(string: handoff.url) { NSWorkspace.shared.open(url) }
    }

    func attach(url: String) async {
        guard let id = selectedID else { return }
        await perform("Linking…") { core in
            _ = try await core.attachCloudRoutine(id, urlOrID: url)
            self.handoff = nil
            await self.load()
            self.message = "Linked to the routine at claude.ai."
        }
    }

    func refreshCloudRuns() async {
        guard let id = selectedID else { return }
        await perform("Checking Claude…") { core in
            try await core.refreshCloudRuns(id)
            await self.reloadRuns()
        }
    }

    func undo(_ run: RoutineRunInfo) async {
        await perform("Undoing…") { core in
            let n = try await core.undoRoutineRun(run.runId)
            self.message = n == 0 ? "Nothing to undo: those threads were already changed." : "Moved \(n) thread\(n == 1 ? "" : "s") back to the inbox."
            await self.reloadRuns()
        }
    }

    // MARK: Preview

    func startPreview() async {
        guard let id = selectedID else { return }
        if hasUnsavedChanges { await save() }
        preview = nil
        await perform(nil) { core in
            self.previewSession = try await core.previewRoutine(id)
            self.busy = "Classifying…"
        }
        // Poll for the finished preview (at most two minutes).
        let deadline = ContinuousClock.now + .seconds(120)
        while let session = previewSession, ContinuousClock.now < deadline {
            if let rows = core?.routinePreview(session) {
                preview = rows
                previewSession = nil
                busy = nil
                if rows.isEmpty { message = "The preview found nothing to sort, or the agent did not answer in the expected form." }
                return
            }
            try? await Task.sleep(for: .milliseconds(500))
        }
        busy = nil
    }

    // MARK: Helpers

    private func perform(_ label: String?, _ body: (CoreClient) async throws -> Void) async {
        guard let core else { return }
        error = nil
        message = nil
        busy = label
        defer { if label != nil { busy = nil } }
        do {
            try await body(core)
        } catch let e as CoreClientError {
            error = e.message
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// "Sorted 84 threads · 2 h ago", for the list.
    static func activity(_ run: RoutineRunInfo?, now: Date = .now) -> String {
        guard let run else { return "Not run yet" }
        let when = RelativeDateTimeFormatter().localizedString(
            for: Date(timeIntervalSince1970: TimeInterval(run.startedAt) / 1000), relativeTo: now)
        switch run.status {
        case "running": return "Running now"
        case "missed": return "Missed a run \(when)"
        case "failed": return "Failed \(when)"
        default:
            let counts = (try? JSONSerialization.jsonObject(with: Data(run.countsJson.utf8)) as? [String: Int]) ?? [:]
            let sorted = max(counts.values.reduce(0, +), Int(run.threadCount))
            return sorted > 0 ? "Sorted \(sorted) thread\(sorted == 1 ? "" : "s") · \(when)" : "Ran \(when)"
        }
    }
}
