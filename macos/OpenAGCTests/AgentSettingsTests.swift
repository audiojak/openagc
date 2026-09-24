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
