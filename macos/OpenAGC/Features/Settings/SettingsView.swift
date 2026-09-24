import SwiftUI

struct SettingsView: View {
    var body: some View {
        TabView {
            Tab("General", systemImage: "gearshape") {
                GeneralSettings()
            }
            Tab("Accounts", systemImage: "person.crop.circle") {
                AccountSettings()
            }
            Tab("Agents", systemImage: "sparkles") {
                AgentSettings()
            }
            Tab("Permissions", systemImage: "hand.raised") {
                AgentPermissionsSettings()
            }
            Tab("Routines", systemImage: "clock.arrow.2.circlepath") {
                RoutineSettings()
            }
            Tab("Privacy", systemImage: "lock.shield") {
                PrivacySettings()
            }
        }
        .frame(width: 640, height: 520)
    }
}

private struct GeneralSettings: View {
    @AppStorage(NewMailNotifier.notifyKey) private var notify = true
    @AppStorage(NewMailNotifier.badgeKey) private var badge = true
    @AppStorage(Updater.betaKey) private var betas = false
    @Environment(AppModel.self) private var model
    @Environment(Updater.self) private var updater

    var body: some View {
        Form {
            Section("New Mail") {
                Toggle("Notify me about new mail in the Inbox", isOn: $notify)
                Toggle("Show unread count on the Dock icon", isOn: $badge)
                    .onChange(of: badge) { model.updateBadge() }
            }
            Section("Updates") {
                if updater.isConfigured {
                    Toggle("Check for updates automatically", isOn: Binding(
                        get: { updater.automaticallyChecks }, set: { updater.automaticallyChecks = $0 }))
                    Toggle("Include beta versions", isOn: $betas)
                    Button("Check Now") { updater.checkForUpdates() }
                        .disabled(!updater.canCheckForUpdates)
                } else {
                    Text("This build does not update itself. Official releases do.")
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
    }
}
