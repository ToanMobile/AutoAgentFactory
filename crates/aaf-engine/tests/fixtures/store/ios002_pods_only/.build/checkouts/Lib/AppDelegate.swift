import UIKit

final class AppDelegate: UIResponder, UIApplicationDelegate {
    func start() {
        let seen = UserDefaults.standard.bool(forKey: "seen")
        print(seen)
    }
}
