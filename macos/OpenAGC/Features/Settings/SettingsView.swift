import SwiftUI

struct SettingsView: View {
    var body: some View {
        TabView {
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
