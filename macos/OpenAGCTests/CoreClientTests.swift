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
