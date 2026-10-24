import SwiftUI

/// The mesh around this Mac: how many tunnels it has and how each is
/// reached, drawn with this Mac in the middle, the lighthouses and relays
/// around it and every other host outside them.
struct NebulaMeshView: View {
    let nebula: NebulaModel

    @State private var feed = MeshModel()
    @State private var selection: String?

    var body: some View {
        Group {
            if let mesh = feed.mesh {
                VStack(alignment: .leading, spacing: 16) {
                    summary(mesh)
                    MeshGraph(mesh: mesh, me: nebula.status?.device ?? "This Mac", selection: $selection)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                    footer(mesh)
                }
                .padding(20)
            } else if let error = feed.error {
                ContentUnavailableView(
                    "Can't show the mesh", systemImage: "network.slash", description: Text(error))
            } else {
                ProgressView()
            }
        }
        .navigationTitle("Mesh")
        .task { await feed.watch() }
    }

    private func summary(_ mesh: DaemonProtocol.Mesh) -> some View {
        let connected = mesh.hosts.filter { $0.state == .connected }
        let relayed = connected.filter { !$0.via.isEmpty }.count
        return HStack(spacing: 12) {
            StatTile(value: connected.count, label: "Tunnels")
            StatTile(value: connected.count - relayed, label: "Direct")
            StatTile(value: relayed, label: "Through relays")
            StatTile(value: mesh.hosts.filter { $0.state == .handshaking }.count, label: "Handshaking")
            StatTile(value: mesh.hosts.filter { $0.state == .noTunnel }.count, label: "No tunnel")
            if !mesh.forwarding.isEmpty {
                StatTile(value: mesh.forwarding.count, label: "Pairs relayed")
            }
        }
    }

    /// The selected host, or why the mesh may be out of date.
    @ViewBuilder
    private func footer(_ mesh: DaemonProtocol.Mesh) -> some View {
        if let error = feed.error {
            Text("Not updating: \(error)")
                .foregroundStyle(.red)
        } else if let peer = mesh.hosts.first(where: { $0.address == selection }) {
            HStack(spacing: 16) {
                Text(peer.displayName).fontWeight(.semibold)
                Text(peer.address).textSelection(.enabled)
                Text([peer.stateText, peer.path(in: mesh)].filter { !$0.isEmpty }.joined(separator: ", "))
                if let expires = peer.certExpires {
                    Text("Certificate expires \(expires.formatted(date: .abbreviated, time: .omitted))")
                }
            }
            .foregroundStyle(.secondary)
        } else {
            Text("Click a host for its details.")
                .foregroundStyle(.tertiary)
        }
    }
}

private struct StatTile: View {
    let value: Int
    let label: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(value, format: .number)
                .font(.title2.monospacedDigit())
                .fontWeight(.semibold)
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
    }
}

private struct MeshGraph: View {
    let mesh: DaemonProtocol.Mesh
    let me: String
    @Binding var selection: String?

    var body: some View {
        GeometryReader { geometry in
            let positions = layout(in: geometry.size)
            let centre = CGPoint(x: geometry.size.width / 2, y: geometry.size.height / 2)
            ZStack {
                Canvas { context, _ in
                    drawEdges(in: &context, positions: positions, centre: centre)
                }
                Node(name: me, symbol: "laptopcomputer", state: .connected, selected: false)
                    .position(centre)
                ForEach(mesh.hosts) { peer in
                    Node(
                        name: peer.displayName, symbol: symbol(peer), state: peer.state,
                        selected: selection == peer.address
                    )
                    .position(positions[peer.address] ?? centre)
                    .onTapGesture {
                        selection = selection == peer.address ? nil : peer.address
                    }
                }
            }
        }
    }

    private func symbol(_ peer: DaemonProtocol.Peer) -> String {
        if peer.lighthouse { return "lightbulb.max" }
        if peer.relay { return "arrow.triangle.branch" }
        return "desktopcomputer"
    }

    /// Lighthouses and relays on an inner ring, everyone else on an outer
    /// one, each host reached through a relay placed by its relay.
    private func layout(in size: CGSize) -> [String: CGPoint] {
        let centre = CGPoint(x: size.width / 2, y: size.height / 2)
        let outerRadius = max(min(size.width, size.height) / 2 - 36, 60)
        let innerHosts = mesh.hosts.filter { $0.lighthouse || $0.relay }
        var outerHosts = mesh.hosts.filter { !($0.lighthouse || $0.relay) }

        func point(_ angle: Double, _ radius: Double) -> CGPoint {
            CGPoint(x: centre.x + radius * cos(angle), y: centre.y + radius * sin(angle))
        }
        func angle(_ index: Int, of count: Int) -> Double {
            -Double.pi / 2 + 2 * Double.pi * Double(index) / Double(max(count, 1))
        }

        var positions: [String: CGPoint] = [:]
        var innerAngles: [String: Double] = [:]
        let innerRadius = outerHosts.isEmpty ? outerRadius : outerRadius * 0.5
        for (index, host) in innerHosts.enumerated() {
            let a = angle(index, of: innerHosts.count)
            innerAngles[host.address] = a
            positions[host.address] = point(a, innerRadius)
        }
        // Sort the outer ring so a relayed host sits beside its relay.
        let evenAngles = Dictionary(
            uniqueKeysWithValues: outerHosts.enumerated().map { ($1.address, angle($0, of: outerHosts.count)) })
        func target(_ host: DaemonProtocol.Peer) -> Double {
            let a = host.via.first.flatMap { innerAngles[$0] } ?? evenAngles[host.address] ?? 0
            return a < -Double.pi / 2 ? a + 2 * Double.pi : a
        }
        outerHosts.sort { target($0) < target($1) }
        for (index, host) in outerHosts.enumerated() {
            positions[host.address] = point(angle(index, of: outerHosts.count), outerRadius)
        }
        return positions
    }

    private func drawEdges(in context: inout GraphicsContext, positions: [String: CGPoint], centre: CGPoint) {
        func line(_ from: CGPoint, _ to: CGPoint, _ colour: Color, _ style: StrokeStyle) {
            var path = Path()
            path.move(to: from)
            path.addLine(to: to)
            context.stroke(path, with: .color(colour), style: style)
        }
        let solid = StrokeStyle(lineWidth: 2)
        let dashed = StrokeStyle(lineWidth: 2, dash: [6, 4])
        let dotted = StrokeStyle(lineWidth: 1.5, lineCap: .round, dash: [1, 5])

        for peer in mesh.hosts {
            guard let at = positions[peer.address] else { continue }
            switch peer.state {
            case .connected where peer.via.isEmpty:
                line(centre, at, .accentColor, solid)
            case .connected:
                for relay in peer.via {
                    line(positions[relay] ?? centre, at, .accentColor, dashed)
                }
            case .handshaking:
                line(centre, at, .orange, dotted)
            case .noTunnel:
                break
            }
        }
        // Traffic this Mac relays runs through it between the two hosts.
        for pair in mesh.forwarding {
            for end in [pair.from, pair.to] {
                if let at = positions[end] {
                    line(centre, at, .purple, dashed)
                }
            }
        }
    }
}

private struct Node: View {
    let name: String
    let symbol: String
    let state: DaemonProtocol.PeerState
    let selected: Bool

    var body: some View {
        VStack(spacing: 4) {
            Image(systemName: symbol)
                .font(.system(size: 15))
                .foregroundStyle(state == .noTunnel ? Color.secondary : Color.white)
                .frame(width: 34, height: 34)
                .background(fill, in: Circle())
                .overlay(Circle().strokeBorder(Color.primary, lineWidth: selected ? 2 : 0))
            Text(name)
                .font(.caption)
                .lineLimit(1)
                .foregroundStyle(state == .noTunnel ? .secondary : .primary)
                // Edges can run under a name, so it sits on the window's colour.
                .padding(.horizontal, 4)
                .background(.background, in: Capsule())
                .frame(maxWidth: 110)
        }
        .contentShape(Rectangle())
        .help(name)
    }

    private var fill: Color {
        switch state {
        case .connected: .accentColor
        case .handshaking: .orange
        case .noTunnel: Color.secondary.opacity(0.2)
        }
    }
}
