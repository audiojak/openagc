import SwiftUI

/// The conversation being answered, above a reply (spec §14.5): the
/// earlier messages as rows that open when clicked, as in the reader, and
/// the latest one open. Drafts (this reply among them) are left out.
struct ComposerThreadPane: View {
    @Environment(AppModel.self) private var model
    @Environment(\.colorScheme) private var colorScheme
    let threadID: String
    @State private var reader: ReaderStore?

    var body: some View {
        Group {
            if let reader, reader.detail != nil {
                MessageWebView(
                    html: EmailDocument.thread(Self.shown(reader.documentMessages), isDark: colorScheme == .dark,
                                               onlyLatestOpen: true),
                    allowRemoteImages: reader.allowsRemoteImages,
                    inlineImages: reader.inlineImages,
                    scrollToLatest: true)
            } else {
                Color.clear
            }
        }
        .task(id: threadID) {
            let store = reader ?? ReaderStore(core: model.core)
            store.ownAddresses = model.reader.ownAddresses
            reader = store
            await store.show(threadID: threadID)
        }
        .accessibilityLabel("The conversation you are answering")
    }

    /// The messages shown: everything sent, not drafts.
    nonisolated static func shown(_ messages: [EmailDocument.Message]) -> [EmailDocument.Message] {
        messages.filter { !$0.isDraft }
    }
}
