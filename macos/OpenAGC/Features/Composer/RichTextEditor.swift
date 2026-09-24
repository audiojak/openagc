import AppKit
import SwiftUI

/// The composer body: an `NSTextView` with rich text, lists and links, and
/// the standard Format menu (bold/italic/underline via ⌘B/⌘I/⌘U). The view
/// owns the text while editing; changes flow out through `text`.
struct RichTextEditor: NSViewRepresentable {
    @Binding var text: NSAttributedString
    var focusOnAppear = false

    func makeCoordinator() -> Coordinator { Coordinator(text: $text) }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSTextView.scrollableTextView()
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        guard let textView = scroll.documentView as? NSTextView else { return scroll }
        textView.delegate = context.coordinator
        textView.isRichText = true
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.usesFindBar = true
        textView.usesFontPanel = false
        textView.isAutomaticLinkDetectionEnabled = true
        textView.isAutomaticQuoteSubstitutionEnabled = true
        textView.isAutomaticDashSubstitutionEnabled = true
        textView.isContinuousSpellCheckingEnabled = true
        textView.isGrammarCheckingEnabled = false
        textView.drawsBackground = false
        textView.textContainerInset = NSSize(width: 16, height: 12)
        textView.font = ComposerHTML.bodyFont
        textView.typingAttributes = [.font: ComposerHTML.bodyFont, .foregroundColor: NSColor.textColor]
        textView.setAccessibilityLabel("Message body")
        textView.textStorage?.setAttributedString(Self.display(text))
        context.coordinator.lastText = text
        if focusOnAppear {
            DispatchQueue.main.async { textView.window?.makeFirstResponder(textView) }
        }
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let textView = scroll.documentView as? NSTextView else { return }
        // Only replace the text when it changed from outside (a draft load).
        if text !== context.coordinator.lastText, !text.isEqual(to: textView.attributedString()) {
            textView.textStorage?.setAttributedString(Self.display(text))
            context.coordinator.lastText = text
        }
    }

    /// Loaded HTML has no color; show it in the current text color.
    private static func display(_ text: NSAttributedString) -> NSAttributedString {
        let copy = NSMutableAttributedString(attributedString: text)
        copy.addAttribute(.foregroundColor, value: NSColor.textColor, range: NSRange(location: 0, length: copy.length))
        return copy
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        var text: Binding<NSAttributedString>
        var lastText: NSAttributedString?

        init(text: Binding<NSAttributedString>) {
            self.text = text
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else { return }
            let snapshot = NSAttributedString(attributedString: textView.attributedString())
            lastText = snapshot
            text.wrappedValue = snapshot
        }
    }
}
