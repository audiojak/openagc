import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct UpdaterTests {
    @Test func debugBuildsDoNotUpdateThemselves() {
        #expect(Updater.isDebugBuild, "tests run against a Debug build")
        let updater = Updater()
        #expect(!updater.isConfigured, "Debug builds never start Sparkle, even with a key")
        #expect(!updater.canCheckForUpdates)
        updater.checkForUpdates() // a no-op, not a crash
    }

    @Test func infoPlistCarriesTheFeedKeyAndVersions() {
        let info = Bundle.main.infoDictionary ?? [:]
        #expect(info["SUFeedURL"] as? String == "https://raw.githubusercontent.com/audiojak/openagc/main/appcast.xml")
        #expect((info["SUPublicEDKey"] as? String)?.isEmpty == false, "release builds need the EdDSA public key")
        #expect(info["CFBundleShortVersionString"] as? String == "0.1.0")
        #expect(info["CFBundleVersion"] as? String == "1")
    }

    @Test func betasAreAChannelUsersOptInto() {
        #expect(Updater.channels(betas: false).isEmpty)
        #expect(Updater.channels(betas: true) == ["beta"])
    }
}
