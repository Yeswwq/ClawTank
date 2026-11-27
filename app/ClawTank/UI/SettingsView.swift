// Settings: General, Display & Quality, Security, Remote Access, Agents. Network changes are applied with an explicit
// button (they restart the server and disconnect any client).

import SwiftUI

struct SettingsView: View {
    @ObservedObject var server: ServerController
    @ObservedObject var settings: AppSettings
    let agents: AgentService

    var body: some View {
        TabView {
            GeneralSettings(server: server, settings: settings)
                .tabItem { Label("General", systemImage: "gearshape") }
            DisplaySettings(settings: settings)
                .tabItem { Label("Display & Quality", systemImage: "display") }
            SecuritySettings(server: server, settings: settings)
                .tabItem { Label("Security", systemImage: "lock.shield") }
            RemoteAccessSettings(server: server)
                .tabItem { Label("Remote Access", systemImage: "globe") }
            AgentSettings(agents: agents, settings: settings)
                .tabItem { Label("Agents", systemImage: "cursorarrow.rays") }
        }
        .padding(20)
        .frame(minWidth: 520, minHeight: 420)
    }
}

private struct GeneralSettings: View {
    @ObservedObject var server: ServerController
    @ObservedObject var settings: AppSettings
    @State private var startAtLogin = LoginItem.isEnabled
    @State private var loginError: String?

    var body: some View {
        Form {
            Toggle("Start ClawTank at login", isOn: $startAtLogin)
                .onChange(of: startAtLogin) { _, enabled in
                    loginError = LoginItem.setEnabled(enabled)
                    startAtLogin = LoginItem.isEnabled
                }
            if LoginItem.needsApproval {
                Text("Approve ClawTank in System Settings → General → Login Items.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            if let loginError { Text(loginError).foregroundStyle(.red).font(.callout) }
            Section("Application Language") {
                LabeledContent("Current language", value: AppLanguage.displayName)
                Button("Open System Settings…") {
                    AppLanguage.openSystemSettings()
                }
                Text(String(localized: """
                    Choose ClawTank under System Settings → General → Language & Region → Applications. \
                    Quit and reopen ClawTank after changing its language.
                    """))
                    .font(.callout).foregroundStyle(.secondary)
            }
            Section("Diagnostics") {
                Picker("Rust log level", selection: $settings.rustLogLevel) {
                    ForEach(RustLogLevel.allCases) { Text($0.title).tag($0) }
                }
                Text(String(localized: """
                    The selected level controls Rust log detail. First-frame bitmap metadata also \
                    requires the controlled diagnostic switch. Screen pixels and credentials are \
                    never logged.
                    """))
                    .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Toggle("Keep this Mac awake while someone is connected", isOn: $settings.keepAwake)
            Toggle("Swap Ctrl and ⌘ (Ctrl+C copies)", isOn: $settings.swapControlCommand)
            Toggle("Show connection diagnostics overlay on the video", isOn: $settings.showsStatsOverlay)
            Section("Sound and clipboard") {
                Toggle("Play this Mac's sound on the client", isOn: $settings.audioEnabled)
                Toggle("Mute this Mac's speakers while connected", isOn: $settings.muteLocalAudio)
                    .disabled(!settings.audioEnabled)
                Toggle("Share the clipboard (text, rich text, HTML, images)", isOn: $settings.clipboardEnabled)
                Text("Applies from the next connection.").font(.callout).foregroundStyle(.secondary)
            }
            Section("Connect") {
                ConnectionDetails(server: server)
            }
        }
        .formStyle(.grouped)
    }
}

private struct DisplaySettings: View {
    @ObservedObject var settings: AppSettings

    var body: some View {
        Form {
            Picker("Display", selection: $settings.displayMode) {
                ForEach(DisplayMode.allCases) { Text($0.title).tag($0) }
            }
            Picker("Virtual display density", selection: $settings.virtualDisplayDensity) {
                ForEach(VirtualDisplayDensity.allCases) { Text($0.title).tag($0) }
            }
            Text("Client DPI is a hint. RDP desktop pixel dimensions are not multiplied.")
                .font(.callout).foregroundStyle(.secondary)
            Picker("Maximum frame rate", selection: $settings.maxFrameRate) {
                Text("60 fps").tag(60)
                Text("30 fps").tag(30)
            }
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text("Image quality")
                    Spacer()
                    Text("\(settings.qualityPreference)%")
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                }
                Slider(value: Binding(
                    get: { Double(settings.qualityPreference) },
                    set: { settings.qualityPreference = Int($0.rounded()) }), in: 0...100, step: 1) {
                    Text("Image quality")
                } minimumValueLabel: {
                    Image(systemName: "arrow.down.circle")
                        .help("Prioritize lower bandwidth")
                } maximumValueLabel: {
                    Image(systemName: "sparkles")
                        .help("Prioritize image quality")
                }
                HStack {
                    Text("Low bandwidth")
                    Spacer()
                    Text("High quality")
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Text("The preference adjusts the AVC420 bitrate budget. " +
                 "Codec selection still follows the client's negotiated capabilities.")
                .font(.callout).foregroundStyle(.secondary)
            Text("Changes apply from the next connection or window resize.")
                .font(.callout).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
    }
}

private struct SecuritySettings: View {
    @ObservedObject var server: ServerController
    @ObservedObject var settings: AppSettings
    @State private var port = ""
    @State private var lanOnly = true
    @State private var allowlist = ""
    @State private var confirmingOpen = false
    @State private var error: String?

    var body: some View {
        Form {
            Section("Password") {
                PasswordChanger(server: server, finishTitle: String(localized: "Reset Password")) {}
            }
            Section("Network") {
                Toggle("Only allow connections from my local network", isOn: Binding(
                    get: { lanOnly },
                    set: { newValue in
                        if newValue { lanOnly = true } else { confirmingOpen = true }
                    }))
                TextField("Allowlist (optional): IPs or CIDRs, comma-separated", text: $allowlist)
                TextField("Port", text: $port)
                if let error { Text(error).foregroundStyle(.red).font(.callout) }
                Button("Apply Network Changes") { apply() }
                Text("Applying restarts the server and disconnects anyone connected.")
                    .font(.callout).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear {
            port = String(settings.port)
            lanOnly = settings.lanOnly
            allowlist = settings.allowlist
        }
        .alert("Allow connections from outside your network?", isPresented: $confirmingOpen) {
            Button("Allow", role: .destructive) { lanOnly = false }
            Button("Keep LAN-only", role: .cancel) {}
        } message: {
            Text(String(localized: """
                Anyone who can reach this Mac's port could try to log in. Only do this behind a VPN, \
                with a strong password, and ideally with an allowlist. Failed logins are rate-limited.
                """))
        }
    }

    private func apply() {
        guard let number = Int(port), (1...65535).contains(number) else {
            error = String(localized: "Port must be a number from 1 to 65535.")
            return
        }
        error = nil
        settings.port = number
        settings.lanOnly = lanOnly
        settings.allowlist = allowlist.trimmingCharacters(in: .whitespaces)
        server.restart()
    }
}
