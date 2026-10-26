import UserNotifications

/// Opens comux's end-to-end encrypted pushes (decisions #177): the relay and Apple deliver a
/// placeholder alert plus `sealed`/`kid`; this swaps in the real title and body. A push that
/// doesn't open shows locally defined text, never whatever arrived in the clear.
final class NotificationService: UNNotificationServiceExtension {
    private var content: UNMutableNotificationContent?
    private var deliver: ((UNNotificationContent) -> Void)?

    override func didReceive(_ request: UNNotificationRequest,
                             withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void) {
        guard let content = request.content.mutableCopy() as? UNMutableNotificationContent else {
            contentHandler(request.content)
            return
        }
        self.content = content
        deliver = contentHandler
        // Only what the envelope says is shown: nothing that came in the clear (a subtitle too).
        content.subtitle = ""
        if let opened = PushEnvelope.open(content.userInfo) {
            content.title = opened.title
            content.body = opened.body
        } else {
            Self.fallback(content)
        }
        contentHandler(content)
        deliver = nil
    }

    /// Out of time: show the generic text rather than anything unverified.
    override func serviceExtensionTimeWillExpire() {
        if let deliver, let content {
            Self.fallback(content)
            deliver(content)
        }
        deliver = nil
    }

    static func fallback(_ content: UNMutableNotificationContent) {
        content.subtitle = ""
        content.title = "copad"
        content.body = "An agent needs you. Open copad to see it."
    }
}
