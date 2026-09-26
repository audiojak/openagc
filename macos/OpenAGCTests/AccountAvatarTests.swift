import Foundation
import Testing
@testable import OpenAGC

struct AccountAvatarTests {
    @Test func initialsComeFromTheNameOrTheAddress() {
        #expect(AccountAvatar.initials(name: "Ada Lovelace", email: "ada@example.com") == "AL")
        #expect(AccountAvatar.initials(name: "Ada Augusta King Lovelace", email: "x@example.com") == "AL")
        #expect(AccountAvatar.initials(name: "ada", email: "x@example.com") == "A")
        #expect(AccountAvatar.initials(name: nil, email: "john@actual.ai") == "J")
        #expect(AccountAvatar.initials(name: "  ", email: "zed@example.com") == "Z")
        #expect(AccountAvatar.initials(name: "123 !!", email: "q@example.com") == "Q", "no letters: use the address")
        #expect(AccountAvatar.initials(name: nil, email: "") == "?")
    }

    @Test func eachAddressKeepsItsColour() {
        let a = AccountAvatar.paletteIndex(for: "work@example.com")
        #expect(a == AccountAvatar.paletteIndex(for: "WORK@example.com"), "case-insensitive")
        let spread = Set(["a@x.com", "b@x.com", "c@x.com", "d@x.com", "e@x.com", "f@x.com"].map(AccountAvatar.paletteIndex(for:)))
        #expect(spread.count > 1, "different addresses usually differ")
    }
}
