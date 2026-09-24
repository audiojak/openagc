import Foundation
import Testing
@testable import OpenAGC

@MainActor
struct UpdaterTests {
    @Test func developmentBuildsDoNotUpdateThemselves() {
        let updater = Updater()
        #expect(!updater.isConfigured, "no SUPublicEDKey in this build")
        #expect(!updater.canCheckForUpdates)
        updater.checkForUpdates() // a no-op, not a crash
    }

    @Test func infoPlistCarriesTheFeedAndVersions() {
        let info = Bundle.main.infoDictionary ?? [:]
        #expect(info["SUFeedURL"] as? String == "https://raw.githubusercontent.com/audiojak/openagc/main/appcast.xml")
        #expect(info["CFBundleShortVersionString"] as? String == "0.1.0")
        #expect(info["CFBundleVersion"] as? String == "1")
    }

    @Test func betasAreAChannelUsersOptInto() {
        #expect(Updater.channels(betas: false).isEmpty)
        #expect(Updater.channels(betas: true) == ["beta"])
    }
}
