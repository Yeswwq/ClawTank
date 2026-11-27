// Viewer for the audit log: every connection attempt, newest first.

import AppKit
import SwiftUI

struct ConnectionLogView: View {
    let audit: AuditLog
    @State private var entries: [AuditEntry] = []

    var body: some View {
        VStack(alignment: .leading) {
            Table(entries) {
                TableColumn("Time") { Text($0.time.formatted(date: .abbreviated, time: .standard)) }
                    .width(min: 150, ideal: 170)
                TableColumn("Address", value: \.peer).width(min: 110, ideal: 130)
                TableColumn("User") { Text($0.user ?? "—") }.width(min: 70, ideal: 90)
                TableColumn("Event") { entry in
                    Label(entry.summary, systemImage: symbol(entry.outcome))
                        .foregroundStyle(color(entry.outcome))
                }
                .width(min: 110, ideal: 120)
                TableColumn("Detail") { entry in
                    let detail = localizedDetail(entry.detail)
                    Text(detail).lineLimit(1).help(detail)
                }
            }
            HStack {
                Text("^[\(entries.count) entry](inflect: true)").foregroundStyle(.secondary)
                Spacer()
                Button("Reveal in Finder") { NSWorkspace.shared.activateFileViewerSelecting([audit.url]) }
                Button("Refresh") { reload() }.keyboardShortcut("r")
            }
        }
        .padding()
        .frame(minWidth: 700, minHeight: 380)
        .onAppear(perform: reload)
    }

    private func reload() {
        entries = audit.entries()
    }

    private func localizedDetail(_ detail: String?) -> String {
        guard let detail else { return "" }
        switch detail {
        case "Wrong username or password": return String(localized: "Wrong username or password")
        case "Client closed the connection": return String(localized: "Client closed the connection")
        case "TLS handshake failed (client may have rejected the certificate)":
            return String(localized: "TLS handshake failed (client may have rejected the certificate)")
        case "lan-only": return String(localized: "Local network only")
        case "allowlist": return String(localized: "Not on the allowlist")
        case "locked-out": return String(localized: "Locked out")
        default: return detail
        }
    }

    private func symbol(_ outcome: AuditEntry.Outcome) -> String {
        switch outcome {
        case .connected: "checkmark.circle"
        case .disconnected: "xmark.circle"
        case .failedLogin: "exclamationmark.triangle"
        case .authenticationFailed: "key.slash"
        case .handshakeFailed: "network.badge.shield.half.filled"
        case .refused: "hand.raised"
        case .lockedOut: "lock"
        case .agentAction: "cursorarrow.rays"
        case .agentRefused: "hand.raised"
        }
    }

    private func color(_ outcome: AuditEntry.Outcome) -> Color {
        switch outcome {
        case .connected, .disconnected, .agentAction: .primary
        case .failedLogin, .authenticationFailed, .handshakeFailed, .refused, .agentRefused: .orange
        case .lockedOut: .red
        }
    }
}
