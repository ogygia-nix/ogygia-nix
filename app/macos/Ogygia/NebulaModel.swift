import AppKit
import Foundation
import Observation
import ServiceManagement

/// The menu bar's view of ogygia-macos-nebulad: whether macOS lets it run,
/// and what it last reported.
@MainActor
@Observable
final class NebulaModel {
    /// The launch daemon bundled at Contents/Library/LaunchDaemons.
    @ObservationIgnored private let service = SMAppService.daemon(
        plistName: "dev.ogygia.macos.nebulad.plist")

    private(set) var registration: SMAppService.Status = .notRegistered
    private(set) var registrationError: String?
    private(set) var status: DaemonProtocol.Status?
    private(set) var unreachable: String?

    init() {
        register()
        Task { await poll() }
    }

    /// The first line of the menu.
    var headline: String {
        switch registration {
        case .requiresApproval:
            return "Allow the network daemon in System Settings"
        case .notFound:
            return "This build has no network daemon"
        case .notRegistered:
            return registrationError.map { "Network daemon not installed: \($0)" }
                ?? "Network daemon not installed"
        case .enabled:
            break
        @unknown default:
            break
        }
        guard let status else {
            return unreachable == nil ? "Starting the network daemon…" : "Network daemon not responding"
        }
        guard let device = status.device, status.repositoryURL != nil else {
            return "Not configured"
        }
        switch status.nebula {
        case .running:
            return "Connected as \(device)"
        case .exited:
            return "Nebula stopped"
        case .stopped:
            return status.updating ? "Fetching configuration…" : "Not connected"
        }
    }

    /// Further lines for the menu: the address, and anything that went wrong.
    var details: [String] {
        guard registration == .enabled else { return [] }
        guard let status else { return unreachable.map { [$0] } ?? [] }
        var lines: [String] = []
        // The daemon restarts itself within a minute of an update replacing
        // it; until then it is still the old build.
        let build = Bundle.main.infoDictionary?["CFBundleVersion"] as? String
        if let running = status.build, running != build {
            lines.append("The network daemon is from build \(running) and will restart shortly")
        }
        if case .running = status.nebula, let address = status.address {
            lines.append(address)
        }
        if case .exited(let reason) = status.nebula {
            lines.append(reason)
        }
        if let error = status.updateError {
            let running = if case .running = status.nebula { true } else { false }
            lines.append(running ? "Update failed, still on the previous config: \(error)" : error)
        }
        if let expires = status.certExpires {
            lines.append("Certificate expires \(expires.formatted(date: .abbreviated, time: .omitted))")
        }
        return lines
    }

    var needsApproval: Bool { registration == .requiresApproval }

    func openLoginItems() {
        SMAppService.openSystemSettingsLoginItems()
    }

    func copyPublicKey() {
        guard let key = status?.publicKey else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(key, forType: .string)
    }

    func refresh() {
        Task { await send(.refresh) }
    }

    /// Store new settings through the daemon, which alone may write them.
    /// Returns the daemon's refusal, if it refused.
    func save(repositoryURL: String, branch: String, device: String) async -> String? {
        do {
            status = try await DaemonClient.send(
                .setSettings(repositoryURL: repositoryURL, branch: branch, device: device))
            unreachable = nil
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    /// Follow the fleet's Nebula config or manage it locally, through the
    /// daemon. Returns the daemon's refusal, if it refused.
    func saveConfig(mode: DaemonProtocol.ConfigMode, local: DaemonProtocol.LocalConfig) async -> String? {
        do {
            status = try await DaemonClient.send(.setConfig(mode: mode, local: local))
            unreachable = nil
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    /// Ask launchd to run the daemon, which macOS holds until the user
    /// allows it under Login Items. Registering again on every launch is
    /// harmless and repairs a registration the user removed.
    private func register() {
        registration = service.status
        if registration != .enabled {
            do {
                try service.register()
                registrationError = nil
            } catch {
                registrationError = error.localizedDescription
            }
        }
        registration = service.status
    }

    private func poll() async {
        while true {
            registration = service.status
            if registration == .enabled {
                await send(.status)
            }
            try? await Task.sleep(for: .seconds(3))
        }
    }

    private func send(_ request: DaemonProtocol.Request) async {
        do {
            status = try await DaemonClient.send(request)
            unreachable = nil
        } catch {
            status = nil
            unreachable = error.localizedDescription
        }
    }
}
