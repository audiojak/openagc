import Foundation
import Testing
@testable import OpenAGC

struct GoogleClientConfigurationTests {
    private func isolated() -> (UserDefaults, KeychainSecretStore) {
        let suite = "ai.actual.openagc.tests.\(UUID().uuidString)"
        return (UserDefaults(suiteName: suite)!, KeychainSecretStore(service: suite))
    }

    @Test func theShippedClientIsTheProjectsDesktopClient() {
        let shipped = GoogleClientConfiguration.shipped()
        #expect(!shipped.isCustom)
        #expect(shipped.isUsable, "GoogleOAuth.plist carries the project's client (spec §7.3)")
        #expect(shipped.clientID.hasSuffix(".apps.googleusercontent.com"))
    }

    @Test func aCustomClientOverridesTheShippedOneAndCanBeCleared() throws {
        let (defaults, keychain) = isolated()
        #expect(GoogleClientConfiguration.custom(defaults: defaults, keychain: keychain) == nil)

        try GoogleClientConfiguration.saveCustom(clientID: "  id.apps.googleusercontent.com ", clientSecret: "sec",
                                                 defaults: defaults, keychain: keychain)
        let effective = GoogleClientConfiguration.effective(defaults: defaults, keychain: keychain)
        #expect(effective == GoogleClientConfiguration(clientID: "id.apps.googleusercontent.com", clientSecret: "sec", isCustom: true))
        #expect(effective.isUsable)
        #expect(try keychain.get(GoogleClientConfiguration.customSecretKey) == "sec", "the secret lives in the Keychain")
        #expect(defaults.string(forKey: GoogleClientConfiguration.customClientIDKey) == "id.apps.googleusercontent.com")

        try GoogleClientConfiguration.saveCustom(clientID: "", clientSecret: "", defaults: defaults, keychain: keychain)
        #expect(GoogleClientConfiguration.custom(defaults: defaults, keychain: keychain) == nil)
        #expect(try keychain.get(GoogleClientConfiguration.customSecretKey) == nil)
    }
}

@MainActor
struct SignInFlowTests {
    @Test func signInWithAnUnusableClientDoesNothing() async throws {
        let model = AppModel(core: try CoreClient(dataDirectory: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)))
        await model.start(openDemo: false)
        let state = model.accountState
        await model.signIn(with: GoogleClientConfiguration(clientID: "", clientSecret: nil, isCustom: false))
        #expect(model.accountState == state)
    }

    @Test func cancellingSignInReturnsToOnboarding() async throws {
        let model = AppModel(core: try CoreClient(dataDirectory: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)))
        await model.start(openDemo: false)
        model.cancelSignIn()
        #expect(model.accountState == .noAccount)
    }
}
