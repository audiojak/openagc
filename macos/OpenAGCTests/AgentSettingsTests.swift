import Foundation
import Testing
@testable import OpenAGC

struct AgentSettingsTests {
    private func info(_ id: String, _ status: AgentStatusInfo) -> AgentProviderInfo {
        AgentProviderInfo(id: id, name: id == "codex" ? "Codex" : "Claude", status: status)
    }

    @Test func statusesReadClearly() {
        #expect(AgentStatusText(info("claude-code", .ready(version: "2.1.34"))).isReady)
        #expect(AgentStatusText(info("claude-code", .ready(version: "2.1.34"))).detail == "Ready — version 2.1.34")
        #expect(AgentStatusText(info("codex", .notInstalled)).detail.contains("brew install codex"))
        #expect(AgentStatusText(info("claude-code", .notInstalled)).detail.contains("claude-code"))
        #expect(AgentStatusText(info("codex", .notAuthenticated(version: "0.150.0"))).detail.contains("codex login"))
        let old = AgentStatusText(info("claude-code", .updateRequired(version: "1.0.0", minimum: "2.1.0")))
        #expect(!old.isReady && old.detail.contains("2.1.0"))
        #expect(AgentStatusText(info("codex", .error(message: "boom"))).detail.hasSuffix("boom"))
    }
}

@MainActor
struct AgentPermissionTests {
    @Test func thePolicyFollowsTheUsersChoices() async throws {
        let dir = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = AppModel(core: try CoreClient(dataDirectory: dir))
        let saved = UserDefaults.standard.stringArray(forKey: AppModel.agentApprovalKey)
        defer { UserDefaults.standard.set(saved, forKey: AppModel.agentApprovalKey) }
        UserDefaults.standard.set(["mail_archive"], forKey: AppModel.agentApprovalKey)
        model.applyAgentPolicy()
        #expect(model.core?.agentPolicy == ["mail_archive"])
        #expect(model.core?.configurableAgentTools.contains("mail_send") == false, "sends always ask")
        UserDefaults.standard.set([String](), forKey: AppModel.agentApprovalKey)
        model.applyAgentPolicy()
        #expect(model.core?.agentPolicy == [])
    }

    @Test func activityExportsAsJSONLinesOldestFirst() throws {
        let actions = [
            AgentActionInfo(actionId: 2, sessionId: "s", tool: "mail_send", argumentsJson: #"{"draft_id":3}"#,
                            risk: "external", state: "rejected", resultSummary: nil, createdAt: 20, resolvedAt: 25),
            AgentActionInfo(actionId: 1, sessionId: "s", tool: "mail_search", argumentsJson: #"{"query":"x"}"#,
                            risk: "read_only", state: "done", resultSummary: "t1, t2", createdAt: 10, resolvedAt: 11),
        ]
        let lines = AgentActivityView.jsonLines(actions).split(separator: "\n")
        #expect(lines.count == 2)
        let first = try #require(try JSONSerialization.jsonObject(with: Data(lines[0].utf8)) as? [String: Any])
        #expect(first["action_id"] as? Int == 1)
        #expect((first["arguments"] as? [String: Any])?["query"] as? String == "x")
        #expect(first["result"] as? String == "t1, t2")
        #expect(AgentActivityView.stateText("denied") == "Blocked")
    }
}
