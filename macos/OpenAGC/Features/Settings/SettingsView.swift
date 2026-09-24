import SwiftUI

struct SettingsView: View {
    var body: some View {
        TabView {
            Tab("General", systemImage: "gearshape") {
                GeneralSettings()
            }
            Tab("Accounts", systemImage: "person.crop.circle") {
                Text("Accounts").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            Tab("Agents", systemImage: "sparkles") {
                Text("Agents").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(width: 560, height: 380)
    }
}

private struct GeneralSettings: View {
    @AppStorage(NewMailNotifier.notifyKey) private var notify = true
    @AppStorage(NewMailNotifier.badgeKey) private var badge = true
    @Environment(AppModel.self) private var model

    var body: some View {
        Form {
            Section("New Mail") {
                Toggle("Notify me about new mail in the Inbox", isOn: $notify)
                Toggle("Show unread count on the Dock icon", isOn: $badge)
                    .onChange(of: badge) { model.updateBadge() }
            }
        }
        .formStyle(.grouped)
    }
}
