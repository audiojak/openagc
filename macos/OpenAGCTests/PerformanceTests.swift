import AppKit
import Foundation
import Testing
@testable import OpenAGC

/// §1.3 performance targets measured through the FFI against the 100k
/// fixture (`cargo xtask fixture`). Skipped when the fixture is absent.
@MainActor
struct PerformanceTests {
    nonisolated static let fixture = URL(filePath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        .appending(path: "build/fixtures/mail-100000.sqlite")

    nonisolated static var fixtureAvailable: Bool { FileManager.default.fileExists(atPath: fixture.path) }

    /// A core whose "perf" account is a copy of the fixture.
    private func fixtureModel() async throws -> AppModel {
        let data = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let accountDir = data.appending(path: "accounts/perf")
        try FileManager.default.createDirectory(at: accountDir, withIntermediateDirectories: true)
        try FileManager.default.copyItem(at: Self.fixture, to: accountDir.appending(path: "mail.sqlite"))
        let core = try CoreClient(dataDirectory: data)
        try await core.openAccount("perf")
        let model = AppModel(core: core)
        await model.threads.show(mailboxID: "INBOX")
        return model
    }

    private func p95(_ samples: [Duration]) -> Duration {
        let sorted = samples.sorted()
        return sorted[Int((Double(sorted.count - 1) * 0.95).rounded())]
    }

    private func measure(_ runs: Int = 40, _ body: (Int) async throws -> Void) async rethrows -> Duration {
        try await body(0) // warm-up
        var samples: [Duration] = []
        let clock = ContinuousClock()
        for i in 0..<runs {
            let start = clock.now
            try await body(i)
            samples.append(clock.now - start)
        }
        return p95(samples)
    }

    private func report(_ name: String, _ value: Duration, budget: Duration) {
        print(String(format: "perf: %-38@ p95 %7.2f ms (budget %.0f ms)", name as NSString,
                     value.inMilliseconds, budget.inMilliseconds))
        #expect(value <= budget, "\(name): p95 \(value) over \(budget)")
    }

    @Test(.enabled(if: PerformanceTests.fixtureAvailable))
    func inboxPageThroughTheFFI() async throws {
        let model = try await fixtureModel()
        let core = try #require(model.core)
        let value = try await measure { _ in _ = try await core.threads(in: "INBOX", limit: 150) }
        report("inbox page via FFI (150 rows)", value, budget: .milliseconds(20))
    }

    @Test(.enabled(if: PerformanceTests.fixtureAvailable))
    func selectingAThreadLoadsItForTheReader() async throws {
        let model = try await fixtureModel()
        let rows = model.threads.rows
        #expect(rows.count == 150)
        let value = try await measure { i in
            await model.reader.show(threadID: rows[(i * 3 + 1) % rows.count].id)
        }
        report("select thread → reader data ready", value, budget: .milliseconds(30))
    }

    @Test(.enabled(if: PerformanceTests.fixtureAvailable))
    func buildingTheReaderDocument() async throws {
        let model = try await fixtureModel()
        await model.reader.show(threadID: model.threads.rows[0].id)
        let messages = model.reader.documentMessages
        let value = await measure { _ in _ = EmailDocument.thread(messages, isDark: false) }
        report("build reader HTML", value, budget: .milliseconds(3))
    }

    @Test(.enabled(if: PerformanceTests.fixtureAvailable))
    func configuringThreadRows() async throws {
        let model = try await fixtureModel()
        let rows = model.threads.rows
        let view = ThreadRowView(frame: NSRect(x: 0, y: 0, width: 380, height: ThreadRowView.height))
        // A full screen of rows is ~12; 1,000 configurations is a fast fling.
        let value = await measure(10) { _ in
            for i in 0..<1_000 {
                view.configure(with: rows[i % rows.count])
                view.layoutSubtreeIfNeeded()
            }
        }
        report("configure + lay out 1,000 rows", value, budget: .milliseconds(120))
    }
}

private extension Duration {
    var inMilliseconds: Double {
        let (s, atto) = components
        return Double(s) * 1_000 + Double(atto) / 1e15
    }
}
