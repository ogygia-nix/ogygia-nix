import Foundation

/// The line protocol spoken by ogygia-macos-nebulad, mirroring
/// src/ogygia-macos-nebulad/src/protocol.rs: one JSON object per line each
/// way over the daemon's Unix socket.
enum DaemonProtocol {
    static let socketPath = "/var/run/ogygia/macos-nebulad.sock"

    enum Request: Encodable, Sendable {
        case status
        case refresh
        case setSettings(repositoryURL: String, branch: String, device: String)
        case setConfig(mode: ConfigMode, local: LocalConfig)
        case mesh

        private enum CodingKeys: String, CodingKey {
            case command, repositoryURL = "repository_url", branch, device, mode, local
        }

        func encode(to encoder: any Encoder) throws {
            var container = encoder.container(keyedBy: CodingKeys.self)
            switch self {
            case .status:
                try container.encode("status", forKey: .command)
            case .refresh:
                try container.encode("refresh", forKey: .command)
            case .setSettings(let repositoryURL, let branch, let device):
                try container.encode("set_settings", forKey: .command)
                try container.encode(repositoryURL, forKey: .repositoryURL)
                try container.encode(branch, forKey: .branch)
                try container.encode(device, forKey: .device)
            case .setConfig(let mode, let local):
                try container.encode("set_config", forKey: .command)
                try container.encode(mode, forKey: .mode)
                try container.encode(local, forKey: .local)
            case .mesh:
                try container.encode("mesh", forKey: .command)
            }
        }
    }

    /// `{"status": {...}}`, `{"mesh": {...}}` or `{"error": "..."}`.
    struct Response: Decodable, Sendable {
        var status: Status?
        var mesh: Mesh?
        var error: String?
    }

    struct Status: Decodable, Sendable, Equatable {
        var build: String?
        var repositoryURL: String?
        var branch: String?
        var device: String?
        var settingsManaged: Bool
        var publicKey: String?
        var address: String?
        var certExpires: Date?
        var updating: Bool
        var lastUpdate: Date?
        var updateError: String?
        var nebula: Nebula
        // Optional, as a daemon an update has not restarted yet sends none.
        var configMode: ConfigMode?
        var configModeManaged: Bool?
        var fleetConfig: LocalConfig?
        var localConfig: LocalConfig?

        private enum CodingKeys: String, CodingKey {
            case build
            case repositoryURL = "repository_url"
            case branch
            case device
            case settingsManaged = "settings_managed"
            case publicKey = "public_key"
            case address
            case certExpires = "cert_expires"
            case updating
            case lastUpdate = "last_update"
            case updateError = "update_error"
            case nebula
            case configMode = "config_mode"
            case configModeManaged = "config_mode_managed"
            case fleetConfig = "fleet_config"
            case localConfig = "local_config"
        }
    }

    /// Mirrors src/ogygia-macos-nebulad/src/config.rs.
    enum ConfigMode: String, Codable, Sendable {
        case fleet
        case local
    }

    struct LocalConfig: Codable, Sendable, Equatable {
        var lighthouses: [Lighthouse] = []
        var useRelays = false
        var relays: [String] = []
        var listenPort: UInt16 = 0
        var inbound: [FirewallRule] = []
        var outbound: [FirewallRule] = []

        private enum CodingKeys: String, CodingKey {
            case lighthouses
            case useRelays = "use_relays"
            case relays
            case listenPort = "listen_port"
            case inbound, outbound
        }
    }

    /// Rows the settings window edits carry an `id` of their own, which is
    /// neither sent nor compared.
    struct Lighthouse: Codable, Sendable, Equatable, Identifiable {
        var id = UUID()
        var address: String
        var endpoints: [String]

        private enum CodingKeys: String, CodingKey {
            case address, endpoints
        }

        static func == (a: Self, b: Self) -> Bool {
            a.address == b.address && a.endpoints == b.endpoints
        }
    }

    struct FirewallRule: Codable, Sendable, Equatable, Identifiable {
        var id = UUID()
        var port: String
        var proto: String
        var host: String?
        var group: String?
        var groups: [String]?
        var cidr: String?
        var localCIDR: String?
        var caName: String?
        var caSHA: String?

        private enum CodingKeys: String, CodingKey {
            case port, proto, host, group, groups, cidr
            case localCIDR = "local_cidr"
            case caName = "ca_name"
            case caSHA = "ca_sha"
        }

        static func == (a: Self, b: Self) -> Bool {
            (a.port, a.proto, a.host, a.group, a.groups) == (b.port, b.proto, b.host, b.group, b.groups)
                && (a.cidr, a.localCIDR, a.caName, a.caSHA) == (b.cidr, b.localCIDR, b.caName, b.caSHA)
        }
    }

    /// Mirrors src/ogygia-macos-nebulad/src/mesh.rs.
    struct Mesh: Decodable, Sendable, Equatable {
        var hosts: [Peer]
        var forwarding: [Forward]
    }

    struct Peer: Decodable, Sendable, Equatable, Identifiable {
        var name: String?
        var address: String
        var state: PeerState
        var lighthouse: Bool
        var relay: Bool
        var remote: String?
        var via: [String]
        var certExpires: Date?

        var id: String { address }

        private enum CodingKeys: String, CodingKey {
            case name, address, state, lighthouse, relay, remote, via
            case certExpires = "cert_expires"
        }
    }

    enum PeerState: String, Decodable, Sendable {
        case connected
        case handshaking
        case noTunnel = "no_tunnel"
    }

    struct Forward: Decodable, Sendable, Equatable, Hashable {
        var from: String
        var to: String
    }

    enum Nebula: Decodable, Sendable, Equatable {
        case stopped
        case running(since: Date)
        case exited(reason: String)

        private enum CodingKeys: String, CodingKey {
            case state, since, reason
        }

        init(from decoder: any Decoder) throws {
            let container = try decoder.container(keyedBy: CodingKeys.self)
            switch try container.decode(String.self, forKey: .state) {
            case "running":
                self = .running(since: try container.decode(Date.self, forKey: .since))
            case "exited":
                self = .exited(reason: try container.decode(String.self, forKey: .reason))
            default:
                self = .stopped
            }
        }
    }

    static func encode(_ request: Request) throws -> Data {
        var line = try JSONEncoder().encode(request)
        line.append(UInt8(ascii: "\n"))
        return line
    }

    static func decode(_ line: Data) throws -> Response {
        let decoder = JSONDecoder()
        // chrono writes RFC 3339 with fractional seconds when it has them.
        decoder.dateDecodingStrategy = .custom { decoder in
            let text = try decoder.singleValueContainer().decode(String.self)
            let formatter = ISO8601DateFormatter()
            formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            if let date = formatter.date(from: text) {
                return date
            }
            formatter.formatOptions = [.withInternetDateTime]
            if let date = formatter.date(from: text) {
                return date
            }
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "not a date: \(text)"))
        }
        return try decoder.decode(Response.self, from: line)
    }
}
