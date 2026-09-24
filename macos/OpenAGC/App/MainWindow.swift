import SwiftUI

/// The three-column main window: mailboxes, threads, message (spec §14.3).
struct MainWindow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow
    @State private var columnVisibility = NavigationSplitViewVisibility.all
    @FocusState private var searchFocused: Bool

    var body: some View {
        Group {
            switch model.accountState {
            case .noAccount, .signingIn:
                OnboardingView()
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            default:
                mailWindow
            }
        }
        .focusedSceneValue(\.isMailWindow, true)
        .task { if model.accountState == .starting { await model.start() } }
        .onAppear {
            model.openComposer = { openWindow(id: "compose", value: $0) }
            model.openRoutines = { openWindow(id: "routines") }
        }
    }

    private var mailWindow: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 180, ideal: 220)
        } content: {
            content
                .navigationSplitViewColumnWidth(min: 300, ideal: 380)
        } detail: {
            // The agent column sits beside the reader. (SwiftUI's
            // `.inspector` left its split item collapsed at zero width here.)
            HStack(spacing: 0) {
                detail
                    .frame(maxWidth: .infinity)
                if model.agent.isPresented {
                    Divider()
                    AgentInspector()
                        .frame(width: 340)
                        .transition(.move(edge: .trailing))
                }
            }
            .animation(.snappy(duration: 0.2), value: model.agent.isPresented)
        }
        .searchable(text: Bindable(model).searchText, placement: .toolbar, prompt: "Search mail")
        .searchFocused($searchFocused)
        .onChange(of: model.searchFocusRequests) { searchFocused = true }
    }

    @ViewBuilder private var content: some View {
        switch model.accountState {
        case .starting:
            ProgressView().controlSize(.small)
        case .noAccount, .signingIn:
            EmptyView()
        case let .failed(message):
            ContentUnavailableView("Something Went Wrong", systemImage: "exclamationmark.triangle", description: Text(message))
        case .open:
            VStack(spacing: 0) {
                if model.needsReauthentication {
                    ReauthenticationBanner()
                }
                if let error = model.threads.searchError {
                    Label(error, systemImage: "exclamationmark.magnifyingglass")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .padding(8)
                }
                if model.threads.rows.isEmpty {
                    if model.threads.searchQuery != nil {
                        ContentUnavailableView.search(text: model.searchText)
                    } else {
                        ContentUnavailableView("No Conversations", systemImage: "tray")
                    }
                } else {
                    ThreadListView()
                }
                AgentPromptBar()
            }
        }
    }

    @ViewBuilder private var detail: some View {
        if model.selectedThreadID != nil {
            ThreadReaderView()
        } else {
            ContentUnavailableView("No Message Selected", systemImage: "envelope.open")
        }
    }
}

/// Google rejected the stored credentials (revoked or expired).
private struct ReauthenticationBanner: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "person.crop.circle.badge.exclamationmark")
            Text("Gmail needs you to sign in again.").font(.callout)
            Spacer()
            Button("Sign In") { Task { await model.signIn(with: .effective()) } }
                .controlSize(.small)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.yellow.opacity(0.15))
    }
}
