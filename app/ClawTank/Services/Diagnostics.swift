// "Copy diagnostics": a .zip a user can attach to a bug report. It contains recent ClawTank
// log entries, settings, the connection log, versions and the display list. It never includes
// the RDP password or the TLS private key (they are never read here).

import AppKit
import ApplicationServices
import Foundation
import OSLog

enum Diagnostics {
    /// Writes the bundle's files into `folder` (exposed for tests).
    static func writeContents(to folder: URL, settings: [String: String], audit: AuditLog,
                              coreVersion: String, since: Date = Date().addingTimeInterval(-3 * 3600)) throws {
        let files = FileManager.default
        try files.createDirectory(at: folder, withIntermediateDirectories: true)

        let process = ProcessInfo.processInfo
        var info = [
            "ClawTank \(Bundle.main.shortVersion) (core \(coreVersion))",
            "macOS \(process.operatingSystemVersionString)",
            "Hardware \(hardwareModel()) · \(process.processorCount) cores · "
                + "\(process.physicalMemory / 1_073_741_824) GB",
            "Generated \(ISO8601DateFormatter().string(from: Date()))",
            "",
            "Application:"
        ] + applicationIdentity()
        info += [
            "",
            "Displays:"
        ]
        for id in CGVirtualDisplayProvider.onlineDisplays() {
            let bounds = CGDisplayBounds(id)
            let virtual = CGDisplayVendorNumber(id) == CGVirtualDisplayHandle.vendorID ? " (ClawTank virtual)" : ""
            info.append("  \(id): \(Int(bounds.width))x\(Int(bounds.height)) pt\(virtual)")
        }
        info += ["", "Settings:"] + settings.keys.sorted().map { "  \($0) = \(settings[$0] ?? "")" }
        try info.joined(separator: "\n").write(to: folder.appendingPathComponent("info.txt"), atomically: true,
                                               encoding: .utf8)

        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.prettyPrinted]
        try encoder.encode(audit.entries(limit: 1000)).write(to: folder.appendingPathComponent("connections.json"))

        try recentLogs(since: since).write(to: folder.appendingPathComponent("log.txt"), atomically: true,
                                           encoding: .utf8)
    }

    /// Zips the bundle to `destination` with ditto.
    static func export(to destination: URL, settings: [String: String], audit: AuditLog,
                       coreVersion: String) throws {
        let folder = FileManager.default.temporaryDirectory
            .appendingPathComponent("ClawTank-diagnostics-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: folder) }
        try writeContents(to: folder, settings: settings, audit: audit, coreVersion: coreVersion)
        try? FileManager.default.removeItem(at: destination)
        let ditto = Process()
        ditto.executableURL = URL(fileURLWithPath: "/usr/bin/ditto")
        ditto.arguments = ["-c", "-k", "--keepParent", folder.path, destination.path]
        try ditto.run()
        ditto.waitUntilExit()
        if ditto.terminationStatus != 0 {
            throw CocoaError(.fileWriteUnknown)
        }
    }

    static func recentLogs(since: Date) -> String {
        guard let store = try? OSLogStore(scope: .currentProcessIdentifier),
              let entries = try? store.getEntries(at: store.position(date: since)) else {
            return "(log store unavailable)"
        }
        let formatter = ISO8601DateFormatter()
        return entries.compactMap { $0 as? OSLogEntryLog }
            .filter { $0.subsystem == "dev.clawtank.ClawTank" }
            .map { "\(formatter.string(from: $0.date)) [\($0.category)] \($0.composedMessage)" }
            .joined(separator: "\n")
    }

    private static func hardwareModel() -> String {
        var size = 0
        sysctlbyname("hw.model", nil, &size, nil, 0)
        var model = [CChar](repeating: 0, count: max(size, 1))
        sysctlbyname("hw.model", &model, &size, nil, 0)
        let bytes = model.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }
        return String(bytes: bytes, encoding: .utf8) ?? "unknown"
    }

    /// Records the exact app identity used for the permission checks.
    ///
    /// macOS TCC associates Screen Recording and Accessibility grants with the
    /// running code identity. A user can therefore have enabled an older copy
    /// of ClawTank while the current process is a different ad-hoc build. These
    /// fields make that distinction visible in an exported diagnostics bundle.
    private static func applicationIdentity() -> [String] {
        let bundle = Bundle.main
        var lines = [
            "  path = \(bundle.bundleURL.path)",
            "  bundleID = \(bundle.bundleIdentifier ?? "(unknown)")",
            "  executable = \(bundle.executableURL?.path ?? "(unknown)")",
            "  hostArchitecture = \(hostArchitecture())",
            "  screenRecording = \(CGPreflightScreenCaptureAccess() ? "granted" : "denied")",
            "  accessibility = \(AXIsProcessTrusted() ? "granted" : "denied")"
        ]
        lines.append(contentsOf: codeSignatureSummary().map { "  \($0)" })
        return lines
    }

    private static func hostArchitecture() -> String {
        var value = utsname()
        uname(&value)
        return withUnsafeBytes(of: &value.machine) { bytes in
            String(bytes: bytes.prefix(while: { $0 != 0 }), encoding: .utf8) ?? "unknown"
        }
    }

    /// Returns stable, non-sensitive fields from `codesign -dv`.
    ///
    /// The command writes its inspection output to stderr. Only identity
    /// metadata is retained; entitlements and requirements are intentionally
    /// omitted from the bundle.
    private static func codeSignatureSummary() -> [String] {
        guard let executable = Bundle.main.executableURL?.path else { return [] }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
        process.arguments = ["-dv", "--verbose=4", executable]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
            process.waitUntilExit()
        } catch {
            return ["codeSignature = unavailable"]
        }
        let output = String(bytes: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        let keys = ["Identifier=", "CDHash=", "TeamIdentifier=", "Signature=", "Runtime Version="]
        let fields = output.split(whereSeparator: \.isNewline).compactMap { line -> String? in
            let text = String(line)
            guard let key = keys.first(where: { text.hasPrefix($0) }) else { return nil }
            return "codeSignature.\(key.dropLast()) = \(text.dropFirst(key.count))"
        }
        return fields.isEmpty ? ["codeSignature = unavailable"] : fields
    }
}
