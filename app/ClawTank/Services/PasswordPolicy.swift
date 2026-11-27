// Rules for user-selected RDP passwords. Require a short but mixed password while rejecting
// common, repetitive, or username-derived values before they reach the RDP listener.

import Foundation

enum PasswordPolicy {
    enum Problem: Hashable {
        case tooShort(minimum: Int)
        case missingUppercase
        case missingLowercase
        case missingSpecial
        case common
        case repetitive
        case containsUsername
    }

    static let minimumLength = 8

    private static let common: Set<String> = [
        "password1234", "123456789012", "qwertyuiopas", "passwordpassword", "letmeinletmein",
        "administrator", "iloveyou1234", "welcome12345", "000000000000", "111111111111"
    ]

    static func problems(_ password: String, username: String) -> [Problem] {
        var found: [Problem] = []
        if password.count < minimumLength { found.append(.tooShort(minimum: minimumLength)) }
        let scalars = password.unicodeScalars
        if !scalars.contains(where: { CharacterSet.uppercaseLetters.contains($0) }) {
            found.append(.missingUppercase)
        }
        if !scalars.contains(where: { CharacterSet.lowercaseLetters.contains($0) }) {
            found.append(.missingLowercase)
        }
        if !scalars.contains(where: {
            !CharacterSet.alphanumerics.contains($0) && !CharacterSet.whitespacesAndNewlines.contains($0)
        }) {
            found.append(.missingSpecial)
        }
        let lower = password.lowercased()
        if common.contains(lower) || lower.hasPrefix("password") || lower.hasPrefix("qwerty") { found.append(.common) }
        if Set(password).count <= 3 { found.append(.repetitive) }
        if !username.isEmpty, lower.contains(username.lowercased()) { found.append(.containsUsername) }
        return found
    }

    static func message(for problem: Problem) -> String {
        switch problem {
        case .tooShort(let minimum): String(localized: "Use at least \(minimum) characters.")
        case .missingUppercase: String(localized: "Include at least one uppercase letter.")
        case .missingLowercase: String(localized: "Include at least one lowercase letter.")
        case .missingSpecial: String(localized: "Include at least one special character.")
        case .common: String(localized: "That password is too common.")
        case .repetitive: String(localized: "Use more than a few different characters.")
        case .containsUsername: String(localized: "Don't include your username.")
        }
    }
}
