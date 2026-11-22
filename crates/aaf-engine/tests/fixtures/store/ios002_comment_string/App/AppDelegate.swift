import UIKit

// Settings used to live in UserDefaults; they moved to the keychain.
/* Old code:
   let uptime = ProcessInfo.processInfo.systemUptime
*/
final class AppDelegate: UIResponder, UIApplicationDelegate {
    let hint = "Do not use UserDefaults or mach_absolute_time here"
    let doc = """
    activeInputModes is documented elsewhere.
    """
    let myUserDefaultsWrapper = SettingsStore()
    let key = UserDefaultsKey.seen
    func creationDateKeyFor(_ id: Int) -> String { "k\(id)" }
}
