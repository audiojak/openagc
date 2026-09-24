import Foundation

/// A Swift mirror of the core's routine JSON (`agent_api::routines::Routine`,
/// spec §11.2), for the editor. Field names follow the JSON through the
/// snake_case coding strategies.
struct RoutineDefinition: Codable, Equatable {
    struct TemplateRef: Codable, Equatable { var id: String; var version: Int }
    struct Schedule: Codable, Equatable { var rrule: String }
    struct LeaveAlone: Codable, Equatable {
        var humanThreads: Bool
        var repliedByMe: Bool
        var starred: Bool
        var spamTrash: Bool
        var custom: [String]
    }
    struct Bucket: Codable, Equatable, Identifiable {
        var id: String
        var labelName: String
        var color: String
        var cadence: String
        var title: String
        var description: String
        var positiveExamples: [String]
        var negativeExamples: [String]
        var priorityWhenAmbiguous: String?
        var listIndividuallyInReport: Bool
    }
    struct Unmatched: Codable, Equatable { var kind: String; var label: String? }
    struct Report: Codable, Equatable { var counts: Bool; var maxLines: Int }
    struct Identity: Codable, Equatable {
        var primaryEmail: String
        var aliases: [String]
        var frequentlyCc: [String]
    }
    struct Limits: Codable, Equatable { var maxThreadsPerRun: Int; var getThreadOnlyWhenNeeded: Bool }
    struct Cloud: Codable, Equatable {
        var triggerId: String?
        var routineUrl: String?
        var environmentId: String?
        var publishedFingerprint: String?
        var publishedAt: Int64?
    }

    var id: String
    var name: String
    var enabled: Bool
    var template: TemplateRef?
    var runner: String
    var schedule: Schedule
    var agent: String?
    var scope: String
    var parentLabel: String
    var leaveAlone: LeaveAlone
    var buckets: [Bucket]
    var unmatched: Unmatched
    var report: Report
    var identity: Identity
    var limits: Limits
    var advancedPrompt: String?
    var cloud: Cloud

    static func decode(_ json: String) throws -> RoutineDefinition {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(RoutineDefinition.self, from: Data(json.utf8))
    }

    func encoded() throws -> String {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        encoder.outputFormatting = [.sortedKeys]
        return String(decoding: try encoder.encode(self), as: UTF8.self)
    }
}

/// Where a routine runs, with the honest explanation the picker shows
/// (spec §11.5).
enum RoutineRunner: String, CaseIterable, Identifiable {
    case claudeCloud = "claude_cloud"
    case local
    case chatGptCloud = "chat_gpt_cloud"
    case claudeDesktop = "claude_desktop"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .claudeCloud: "Claude cloud"
        case .local: "This Mac (OpenAGC)"
        case .chatGptCloud: "ChatGPT"
        case .claudeDesktop: "Claude Desktop"
        }
    }

    var badge: String {
        switch self {
        case .claudeCloud: "☁︎ Claude"
        case .local: "⌘ Local"
        case .chatGptCloud: "☁︎ ChatGPT"
        case .claudeDesktop: "Desktop"
        }
    }

    var explanation: String {
        switch self {
        case .claudeCloud:
            "Runs on Anthropic's cloud on your Claude plan, even when this Mac is off. Needs Claude Code signed in with your claude.ai account and Gmail connected at claude.ai. OpenAGC sets it up through your claude command; the routine then works under Claude's Gmail permissions, not OpenAGC's approval rules."
        case .local:
            "Runs here with your installed Claude Code or Codex, through OpenAGC's tools and approval rules. Needs this Mac awake and OpenAGC open at the scheduled time."
        case .chatGptCloud:
            "Runs on OpenAI's cloud on your ChatGPT plan. Needs the Gmail app connected in ChatGPT; unattended label changes may need approval there. OpenAGC can't create it for you: it copies the prompt and opens ChatGPT."
        case .claudeDesktop:
            "Runs in the Claude Desktop app's scheduled tasks on this Mac. OpenAGC can't create it for you: it copies the prompt for you to paste."
        }
    }

    /// Claude cloud runs at most hourly, in UTC.
    var scheduleNote: String? {
        switch self {
        case .claudeCloud: "Claude cloud runs at most hourly and uses UTC; it may start a few minutes late."
        case .local: "Runs only while OpenAGC is open. A run missed while it was closed is noted, not made up."
        default: nil
        }
    }
}

/// Schedule presets for the editor, as RRULEs (local time).
enum SchedulePreset: String, CaseIterable, Identifiable {
    case hourly, daily, weekdays, weekly, custom
    var id: String { rawValue }

    var title: String {
        switch self {
        case .hourly: "Every hour"
        case .daily: "Every day"
        case .weekdays: "Weekdays"
        case .weekly: "Once a week"
        case .custom: "Custom"
        }
    }

    static func rule(_ preset: SchedulePreset, hour: Int, minute: Int, weekday: String) -> String? {
        switch preset {
        case .hourly: "FREQ=HOURLY;BYMINUTE=\(minute)"
        case .daily: "FREQ=DAILY;BYHOUR=\(hour);BYMINUTE=\(minute)"
        case .weekdays: "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=\(hour);BYMINUTE=\(minute)"
        case .weekly: "FREQ=WEEKLY;BYDAY=\(weekday);BYHOUR=\(hour);BYMINUTE=\(minute)"
        case .custom: nil
        }
    }

    /// Read a rule back into the editor's controls.
    static func parse(_ rule: String) -> (SchedulePreset, hour: Int, minute: Int, weekday: String) {
        var parts: [String: String] = [:]
        for kv in rule.split(separator: ";") {
            let pair = kv.split(separator: "=", maxSplits: 1)
            if pair.count == 2 { parts[String(pair[0])] = String(pair[1]) }
        }
        let minute = Int(parts["BYMINUTE"] ?? "") ?? 0
        let hour = Int(parts["BYHOUR"] ?? "") ?? 8
        let days = parts["BYDAY"] ?? "MO"
        let known = Set(["FREQ", "BYMINUTE", "BYHOUR", "BYDAY"])
        guard parts.keys.allSatisfy(known.contains) else { return (.custom, hour, minute, "MO") }
        switch (parts["FREQ"], days) {
        case ("HOURLY", _) where parts["BYHOUR"] == nil && parts["BYDAY"] == nil: return (.hourly, hour, minute, "MO")
        case ("DAILY", _) where parts["BYDAY"] == nil: return (.daily, hour, minute, "MO")
        case ("WEEKLY", "MO,TU,WE,TH,FR"): return (.weekdays, hour, minute, "MO")
        case ("WEEKLY", _) where !days.contains(","): return (.weekly, hour, minute, days)
        default: return (.custom, hour, minute, "MO")
        }
    }
}

extension RoutineDefinition.Bucket {
    static let colors = ["red", "orange", "yellow", "green", "teal", "blue", "purple", "pink", "brown", "gray"]
    static let cadences = ["daily", "weekly", "monthly"]

    static func new(order: Int) -> Self {
        .init(id: "bucket-\(UUID().uuidString.prefix(8).lowercased())", labelName: "\(order)-New", color: "gray",
              cadence: "weekly", title: "New bucket", description: "", positiveExamples: [], negativeExamples: [],
              priorityWhenAmbiguous: nil, listIndividuallyInReport: false)
    }
}
