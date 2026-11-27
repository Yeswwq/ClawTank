// Resolves ClawTank's selected bundle localization and opens System Settings.
// macOS owns the per-app language choice; the app does not rewrite AppleLanguages itself.

import AppKit
import Foundation

enum AppLanguage {
    static var preferredIdentifier: String {
        Bundle.main.preferredLocalizations.first ?? Locale.current.identifier
    }

    static var displayName: String {
        displayName(for: preferredIdentifier)
    }

    static func displayName(for identifier: String) -> String {
        let language = Locale(identifier: identifier).language
        switch language.languageCode?.identifier {
        case "zh":
            if language.script?.identifier == "Hant" {
                return String(localized: "System language")
            }
            return String(localized: "Simplified Chinese")
        case "en":
            return String(localized: "English")
        default:
            return String(localized: "System language")
        }
    }

    @MainActor
    static func openSystemSettings() {
        if let settingsURL = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.apple.systempreferences") {
            NSWorkspace.shared.openApplication(at: settingsURL, configuration: NSWorkspace.OpenConfiguration())
        }
    }
}
