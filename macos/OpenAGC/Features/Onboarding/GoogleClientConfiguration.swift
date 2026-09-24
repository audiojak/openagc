import Foundation

/// Which Google OAuth client to sign in with (spec §7.3): the user's own
/// ("bring your own client") if configured, otherwise the one shipped in
/// GoogleOAuth.plist.
struct GoogleClientConfiguration: Equatable {
    static let customClientIDKey = "customGoogleClientID"
    static let customSecretKey = "oauth.client_secret.custom"

    var clientID: String
    var clientSecret: String?
    var isCustom: Bool

    var isUsable: Bool { !clientID.trimmingCharacters(in: .whitespaces).isEmpty }

    static func shipped(bundle: Bundle = .main) -> GoogleClientConfiguration {
        guard let url = bundle.url(forResource: "GoogleOAuth", withExtension: "plist"),
              let dict = NSDictionary(contentsOf: url) as? [String: String]
        else { return GoogleClientConfiguration(clientID: "", clientSecret: nil, isCustom: false) }
        let secret = dict["ClientSecret"].flatMap { $0.isEmpty ? nil : $0 }
        return GoogleClientConfiguration(clientID: dict["ClientID"] ?? "", clientSecret: secret, isCustom: false)
    }

    static func custom(defaults: UserDefaults = .standard, keychain: KeychainSecretStore = KeychainSecretStore()) -> GoogleClientConfiguration? {
        guard let id = defaults.string(forKey: customClientIDKey), !id.isEmpty else { return nil }
        let secret = (try? keychain.get(customSecretKey)) ?? nil
        return GoogleClientConfiguration(clientID: id, clientSecret: secret, isCustom: true)
    }

    /// The client to use now.
    static func effective(defaults: UserDefaults = .standard, keychain: KeychainSecretStore = KeychainSecretStore(),
                          bundle: Bundle = .main) -> GoogleClientConfiguration {
        custom(defaults: defaults, keychain: keychain) ?? shipped(bundle: bundle)
    }

    static func saveCustom(clientID: String, clientSecret: String, defaults: UserDefaults = .standard,
                           keychain: KeychainSecretStore = KeychainSecretStore()) throws {
        let id = clientID.trimmingCharacters(in: .whitespacesAndNewlines)
        let secret = clientSecret.trimmingCharacters(in: .whitespacesAndNewlines)
        if id.isEmpty {
            defaults.removeObject(forKey: customClientIDKey)
            try keychain.delete(customSecretKey)
            return
        }
        defaults.set(id, forKey: customClientIDKey)
        if secret.isEmpty { try keychain.delete(customSecretKey) } else { try keychain.set(customSecretKey, secret) }
    }
}
