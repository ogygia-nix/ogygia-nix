import SwiftUI

/// Every host in the mesh and how this Mac reaches it.
struct NebulaPeersView: View {
    @State private var feed = MeshModel()
    @State private var sortOrder = [KeyPathComparator(\DaemonProtocol.Peer.displayName)]

    var body: some View {
        Group {
            if let mesh = feed.mesh {
                Table(mesh.hosts.sorted(using: sortOrder), sortOrder: $sortOrder) {
                    TableColumn("Name", value: \.displayName)
                    TableColumn("Address", value: \.address)
                    TableColumn("State", value: \.stateText)
                    TableColumn("Path") { peer in
                        Text(peer.path(in: mesh))
                    }
                    TableColumn("Role") { peer in
                        Text(role(peer))
                    }
                    TableColumn("Certificate expires") { peer in
                        Text(peer.certExpires?.formatted(date: .abbreviated, time: .omitted) ?? "")
                    }
                }
            } else if let error = feed.error {
                ContentUnavailableView(
                    "Can't show the peers", systemImage: "network.slash", description: Text(error))
            } else {
                ProgressView()
            }
        }
        .navigationTitle("Peers")
        .task { await feed.watch() }
    }

    private func role(_ peer: DaemonProtocol.Peer) -> String {
        switch (peer.lighthouse, peer.relay) {
        case (true, true): "Lighthouse, relay"
        case (true, false): "Lighthouse"
        case (false, true): "Relay"
        case (false, false): ""
        }
    }
}
