import Foundation
import Observation
import os

/// App-wide state: the core, the open account, and what is selected.
/// Routes core events to the stores that care (spec §14.2).
@MainActor
@Observable
final class AppModel {
    enum AccountState: Equatable {
        case starting
        case noAccount
        case open(accountID: String)
        case failed(String)
    }

    static let demoAccountID = "demo"
    static let demoThreadCount: UInt32 = 2_000

    private(set) var accountState: AccountState = .starting
    var selectedMailboxID: String? = "INBOX" {
        didSet { if selectedMailboxID != oldValue { mailboxChanged() } }
    }
    var selectedThreadID: String?

    let mailboxes: MailboxStore
    let threads: ThreadListStore
    let reader: ReaderStore
    let core: CoreClient?

    private let logger = Logger(subsystem: "ai.actual.openagc", category: "app")
    private var eventTask: Task<Void, Never>?

    init(core: CoreClient?) {
        self.core = core
        mailboxes = MailboxStore(core: core)
        threads = ThreadListStore(core: core)
        reader = ReaderStore(core: core)
    }

    /// Open the remembered account, or the demo when asked for on launch.
    func start(openDemo: Bool = UserDefaults.standard.bool(forKey: "OpenAGCDemo")) async {
        guard let core else {
            accountState = .failed("The core failed to start. See ~/Library/Logs/OpenAGC/core.log.")
            return
        }
        listenForEvents(from: core)
        if openDemo {
            await openDemoMailbox()
        } else if let id = UserDefaults.standard.string(forKey: "accountID") {
            await open(accountID: id)
        } else {
            accountState = .noAccount
        }
    }

    func openDemoMailbox() async {
        guard let core else { return }
        do {
            try await core.openAccount(Self.demoAccountID)
            let inbox = try await core.mailboxes().first { $0.kind == .inbox }
            if (inbox?.totalCount ?? 0) == 0 {
                try await core.seedDemoMailbox(threads: Self.demoThreadCount)
            }
            await open(accountID: Self.demoAccountID)
        } catch {
            accountState = .failed(error.message)
        }
    }

    private func open(accountID: String) async {
        guard let core else { return }
        do {
            try await core.openAccount(accountID)
            accountState = .open(accountID: accountID)
            await mailboxes.reload()
            await threads.show(mailboxID: selectedMailboxID ?? "INBOX")
        } catch {
            logger.error("opening account failed: \(error.message, privacy: .public)")
            accountState = .failed(error.message)
        }
    }

    private func mailboxChanged() {
        selectedThreadID = nil
        guard case .open = accountState, let id = selectedMailboxID else { return }
        Task { await threads.show(mailboxID: id) }
    }

    private func listenForEvents(from core: CoreClient) {
        eventTask = Task { [weak self] in
            for await event in core.events {
                guard let self else { return }
                await self.handle(event)
            }
        }
    }

    private func handle(_ event: CoreClientEvent) async {
        switch event {
        case let .threadsChanged(mailboxID, hint):
            await mailboxes.reload()
            if mailboxID == threads.mailboxID {
                await threads.apply(hint)
            }
        case let .error(error):
            logger.error("core error: \(error.message, privacy: .public)")
        case .syncStatus, .outboxStatus:
            break
        }
    }
}
