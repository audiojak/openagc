import Foundation
import Observation
import Sparkle

/// Sparkle 2 auto-updates (spec §16). Off unless the build carries an
/// EdDSA public key, so development builds never check. Betas are a
/// Sparkle channel the user opts into in Settings.
@MainActor
@Observable
final class Updater: NSObject {
    nonisolated static let betaKey = "receiveBetaUpdates"

    /// Whether this build can update itself at all.
    let isConfigured: Bool
    private(set) var canCheckForUpdates = false

    @ObservationIgnored private var controller: SPUStandardUpdaterController?
    @ObservationIgnored private var observation: NSKeyValueObservation?

    init(bundle: Bundle = .main) {
        let key = bundle.object(forInfoDictionaryKey: "SUPublicEDKey") as? String ?? ""
        let feed = bundle.object(forInfoDictionaryKey: "SUFeedURL") as? String ?? ""
        isConfigured = !key.isEmpty && !feed.isEmpty
        super.init()
        guard isConfigured else { return }
        let controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: self,
                                                      userDriverDelegate: nil)
        self.controller = controller
        observation = controller.updater.observe(\.canCheckForUpdates, options: [.initial, .new]) { [weak self] updater, _ in
            let can = updater.canCheckForUpdates
            Task { @MainActor in self?.canCheckForUpdates = can }
        }
    }

    func checkForUpdates() {
        controller?.checkForUpdates(nil)
    }

    var automaticallyChecks: Bool {
        get { controller?.updater.automaticallyChecksForUpdates ?? false }
        set { controller?.updater.automaticallyChecksForUpdates = newValue }
    }
}

extension Updater: SPUUpdaterDelegate {
    /// Release builds follow the default channel; opting in adds "beta".
    nonisolated func allowedChannels(for updater: SPUUpdater) -> Set<String> {
        Self.channels(betas: UserDefaults.standard.bool(forKey: Self.betaKey))
    }

    nonisolated static func channels(betas: Bool) -> Set<String> {
        betas ? ["beta"] : []
    }
}
