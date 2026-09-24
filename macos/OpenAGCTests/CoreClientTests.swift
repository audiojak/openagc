import Foundation
import Testing
@testable import OpenAGC

struct CoreClientTests {
    private func tempDir() -> URL {
        FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
    }

    @Test func pingRoundTripsThroughRust() throws {
        let client = try CoreClient(dataDirectory: tempDir())
        #expect(client.ping("hello") == "pong: hello")
    }

    @Test func asyncCallsRunOnTheCoreRuntime() async throws {
        let client = try CoreClient(dataDirectory: tempDir())
        #expect(try await client.pingAsync("hi") == "pong: hi (on openagc-core)")
    }

    @MainActor
    @Test func asyncCallsFromTheMainActorDoNotBlockIt() async throws {
        let client = try CoreClient(dataDirectory: tempDir())
        async let reply = client.pingAsync("main")
        // The main actor stays free to run other work while Rust sleeps.
        await Task.yield()
        #expect(try await reply == "pong: main (on openagc-core)")
    }

    @Test func versionComesFromTheCrate() throws {
        let client = try CoreClient(dataDirectory: tempDir())
        #expect(client.version == "0.1.0")
    }

    @Test func rustErrorsSurfaceAsTypedErrors() {
        #expect(throws: CoreClientError(kind: .invalidInput, message: "data_dir must not be empty")) {
            try CoreClient(dataDirectoryPath: "")
        }
    }
}
