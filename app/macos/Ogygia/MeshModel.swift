import Foundation
import Observation

/// The mesh as the daemon last reported it, refreshed while a pane showing
/// it is on screen. Nebula logs every query, so nothing asks otherwise.
@MainActor
@Observable
final class MeshModel {
    private(set) var mesh: DaemonProtocol.Mesh?
    private(set) var error: String?

    /// Poll until the calling task is cancelled, as `.task` does when its
    /// view goes away.
    func watch() async {
        while !Task.isCancelled {
            do {
                mesh = try await DaemonClient.mesh()
                error = nil
            } catch {
                self.error = error.localizedDescription
            }
            try? await Task.sleep(for: .seconds(5))
        }
    }
}

extension DaemonProtocol.Mesh {
    /// A host's name, or its address when the mesh does not know one.
    func name(of address: String) -> String {
        hosts.first { $0.address == address }?.displayName ?? address
    }
}

extension DaemonProtocol.Peer {
    var displayName: String { name ?? address }

    var stateText: String {
        switch state {
        case .connected: "Connected"
        case .handshaking: "Handshaking"
        case .noTunnel: "No tunnel"
        }
    }

    /// How the tunnel reaches the host.
    func path(in mesh: DaemonProtocol.Mesh) -> String {
        guard state == .connected else { return "" }
        if !via.isEmpty {
            return "Through " + via.map { mesh.name(of: $0) }.formatted(.list(type: .and))
        }
        return remote.map { "Direct to \($0)" } ?? "Direct"
    }
}
