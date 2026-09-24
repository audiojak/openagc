import SwiftUI

/// The reader pane. Plain text for now; the sanitized-HTML WKWebView reader
/// replaces the body rendering (spec §14.4).
struct ThreadDetailView: View {
    @Environment(AppModel.self) private var model
    let threadID: String

    @State private var detail: ThreadDetail?
    @State private var bodies: [String: String] = [:]

    var body: some View {
        ScrollView {
            if let detail {
                VStack(alignment: .leading, spacing: 16) {
                    Text(detail.thread.subject.isEmpty ? "(no subject)" : detail.thread.subject)
                        .font(.title2.weight(.semibold))
                        .textSelection(.enabled)
                    ForEach(detail.messages, id: \.id) { message in
                        MessageCard(message: message, text: bodies[message.id])
                    }
                }
                .padding(20)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .task(id: threadID) { await load() }
    }

    private func load() async {
        guard let core = model.core else { return }
        guard let loaded = try? await core.thread(threadID) else { return }
        detail = loaded
        var texts: [String: String] = [:]
        for message in loaded.messages {
            if let body = try? await core.renderedBody(message.id) {
                texts[message.id] = body.text ?? ""
            }
        }
        bodies = texts
    }
}

private struct MessageCard: View {
    let message: MessageInfo
    let text: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(message.from.map { $0.name ?? $0.email } ?? "(unknown sender)")
                    .font(.headline)
                Spacer()
                Text(Date(timeIntervalSince1970: TimeInterval(message.date) / 1000), format: .dateTime.day().month().hour().minute())
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            if !message.to.isEmpty {
                Text("To: " + message.to.map { $0.name ?? $0.email }.joined(separator: ", "))
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            Text(text ?? message.snippet)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(14)
        .background(.background.secondary, in: .rect(cornerRadius: 10))
    }
}
