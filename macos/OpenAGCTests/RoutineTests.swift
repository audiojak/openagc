import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct RoutineTests {
    private func demo() async throws -> AppModel {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        await model.start(openDemo: true)
        return model
    }

    @Test func theSwiftMirrorRoundTripsTheCoresJSON() async throws {
        let model = try await demo()
        let info = try await model.core!.createRoutineFromTemplate(runner: "local")
        let def = try RoutineDefinition.decode(info.definitionJson)
        #expect(def.buckets.count == 6)
        #expect(def.buckets[0].labelName == "1-Daily")
        #expect(def.leaveAlone.humanThreads)
        #expect(def.identity.primaryEmail == "me@example.com")
        // Saving the Swift encoding back is accepted unchanged.
        let saved = try await model.core!.saveRoutine(json: try def.encoded())
        #expect(try RoutineDefinition.decode(saved.definitionJson) == def)
    }

    @Test func editingSavingAndPreviewThroughTheStore() async throws {
        let model = try await demo()
        let store = model.routines
        await store.create(runner: .local)
        #expect(store.draft != nil && !store.hasUnsavedChanges)
        store.draft?.name = "Sort my mail"
        store.draft?.buckets.removeLast()
        #expect(store.hasUnsavedChanges)
        await store.save()
        #expect(!store.hasUnsavedChanges)
        #expect(store.routines.first?.name == "Sort my mail")
        #expect(store.scopeCount != nil)

        await store.startPreview()
        #expect(store.preview == [], "the scripted agent answers in prose, so the preview is empty")

        await store.runNow(model: model)
        let deadline = ContinuousClock.now + .seconds(5)
        while store.runs.first?.status != "succeeded", ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(100))
            await store.reloadRuns()
        }
        #expect(store.runs.first?.status == "succeeded")
        #expect(RoutinesStore.activity(store.runs.first).hasPrefix("Ran "))
    }

    @Test func publishingWithoutAUsableCLIFallsBackToTheHandoff() async throws {
        let model = try await demo()
        let store = model.routines
        await store.create(runner: .claudeCloud)
        await store.publish()
        // Tests never reach a real claude: publishing fails over to the paste hand-off.
        #expect(store.error != nil)
        let handoff = try #require(store.handoff)
        #expect(handoff.url == "https://claude.ai/code/routines")
        #expect(handoff.prompt.contains("label_thread"))

        await store.create(runner: .chatGptCloud)
        await store.publish()
        #expect(store.handoff?.url == "https://chatgpt.com")
    }

    @Test func schedulePresetsRoundTrip() {
        #expect(SchedulePreset.parse("FREQ=HOURLY;BYMINUTE=44").0 == .hourly)
        #expect(SchedulePreset.parse("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=8;BYMINUTE=30").0 == .weekdays)
        let weekly = SchedulePreset.parse("FREQ=WEEKLY;BYDAY=FR;BYHOUR=16;BYMINUTE=5")
        #expect(weekly.0 == .weekly && weekly.weekday == "FR" && weekly.hour == 16 && weekly.minute == 5)
        #expect(SchedulePreset.parse("FREQ=MONTHLY;BYMONTHDAY=1").0 == .custom)
        #expect(SchedulePreset.rule(.daily, hour: 7, minute: 0, weekday: "MO") == "FREQ=DAILY;BYHOUR=7;BYMINUTE=0")
        #expect(PromptSafety.missing("Never trash. Never send. Never mark as spam.").isEmpty)
        #expect(PromptSafety.missing("label stuff").count == 3)
    }

    @Test func activityLines() {
        let run = RoutineRunInfo(runId: 1, inferred: false, sessionId: nil, startedAt: Int64(Date.now.timeIntervalSince1970 * 1000) - 7_200_000,
                                 endedAt: nil, status: "succeeded", countsJson: #"{"daily":2,"newsletters":82}"#, reportText: nil, threadCount: 84)
        #expect(RoutinesStore.activity(run).hasPrefix("Sorted 84 threads · "))
        #expect(RoutinesStore.activity(nil) == "Not run yet")
    }
}
