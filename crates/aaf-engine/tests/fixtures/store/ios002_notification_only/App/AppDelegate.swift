import UIKit

// Found on a real repo: observing the change notification is not an access.
final class AppDelegate: UIResponder, UIApplicationDelegate {
    var observer: NSObjectProtocol?

    func start() {
        observer = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification,
            object: nil,
            queue: .main
        ) { _ in }
    }
}
