import SwiftUI

/// The reader pane: subject, a remote-images banner when needed, and the
/// thread rendered in one locked-down web view.
struct ThreadReaderView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        let reader = model.reader
        VStack(spacing: 0) {
            if let detail = reader.detail {
                header(detail)
                if reader.hasRemoteImages && !reader.allowsRemoteImages {
                    remoteImagesBanner
                }
                MessageWebView(
                    html: EmailDocument.thread(reader.documentMessages, isDark: colorScheme == .dark),
                    allowRemoteImages: reader.allowsRemoteImages)
            } else {
                Color.clear
            }
        }
        .task(id: model.selectedThreadID) { await reader.show(threadID: model.selectedThreadID) }
    }

    private func header(_ detail: ThreadDetail) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Text(detail.thread.subject.isEmpty ? "(no subject)" : detail.thread.subject)
                .font(.title3.weight(.semibold))
                .textSelection(.enabled)
                .lineLimit(2)
            Spacer()
            if detail.messages.count > 1 {
                Text("\(detail.messages.count) messages")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 20)
        .padding(.top, 14)
        .padding(.bottom, 6)
    }

    private var remoteImagesBanner: some View {
        HStack(spacing: 10) {
            Image(systemName: "photo.badge.exclamationmark")
                .foregroundStyle(.secondary)
            Text("Remote images are hidden to protect your privacy.")
                .font(.callout)
            Spacer()
            Button("Load Images") { model.reader.loadRemoteImagesForThread() }
            Button("Always from Sender") { model.reader.alwaysLoadRemoteImagesFromSenders() }
        }
        .controlSize(.small)
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
        .background(.quaternary.opacity(0.5))
    }
}
