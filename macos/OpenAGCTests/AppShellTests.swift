import Testing
@testable import OpenAGC

@MainActor
struct AppShellTests {
    @Test func appDelegateKeepsRunningWhenLastWindowCloses() {
        let delegate = AppDelegate()
        #expect(delegate.applicationShouldTerminateAfterLastWindowClosed(.shared) == false)
    }
}
