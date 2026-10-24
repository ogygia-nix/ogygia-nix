import Darwin
import Foundation

/// Sends one request to ogygia-macos-nebulad and reads its reply, the way the
/// Tailscale GUI talks to tailscaled over its local socket.
enum DaemonClient {
    enum Failure: LocalizedError {
        case unreachable(String)
        case daemon(String)

        var errorDescription: String? {
            switch self {
            case .unreachable(let why): "Can't reach the network daemon: \(why)"
            case .daemon(let message): message
            }
        }
    }

    static func send(_ request: DaemonProtocol.Request) async throws -> DaemonProtocol.Status {
        guard let status = try await respond(request).status else {
            throw Failure.daemon("The network daemon sent an empty reply.")
        }
        return status
    }

    static func mesh() async throws -> DaemonProtocol.Mesh {
        guard let mesh = try await respond(.mesh).mesh else {
            throw Failure.daemon("The network daemon sent an empty reply.")
        }
        return mesh
    }

    private static func respond(_ request: DaemonProtocol.Request) async throws -> DaemonProtocol.Response {
        let line = try DaemonProtocol.encode(request)
        // Blocking socket calls stay off the main actor.
        let reply = try await Task.detached { try exchange(line) }.value
        let response = try DaemonProtocol.decode(reply)
        if let error = response.error {
            throw Failure.daemon(error)
        }
        return response
    }

    private static func exchange(_ line: Data) throws -> Data {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw Failure.unreachable(errnoMessage()) }
        defer { close(fd) }

        // A daemon busy evaluating still answers at once, so a slow reply
        // means something is wrong. Asking Nebula for the mesh may take the
        // daemon up to its own 10 second limit.
        var timeout = timeval(tv_sec: 15, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let path = Array(DaemonProtocol.socketPath.utf8CString)
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            buffer.copyBytes(from: path.map { UInt8(bitPattern: $0) }.prefix(buffer.count - 1))
        }
        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else { throw Failure.unreachable(errnoMessage()) }

        try line.withUnsafeBytes { bytes in
            var sent = 0
            while sent < bytes.count {
                let n = write(fd, bytes.baseAddress! + sent, bytes.count - sent)
                guard n > 0 else { throw Failure.unreachable(errnoMessage()) }
                sent += n
            }
        }

        var reply = Data()
        var buffer = [UInt8](repeating: 0, count: 16384)
        while !reply.contains(UInt8(ascii: "\n")) {
            let n = read(fd, &buffer, buffer.count)
            guard n > 0 else {
                throw Failure.unreachable(n == 0 ? "the connection closed" : errnoMessage())
            }
            reply.append(contentsOf: buffer[..<n])
        }
        return reply.prefix(while: { $0 != UInt8(ascii: "\n") })
    }

    private static func errnoMessage() -> String {
        String(cString: strerror(errno))
    }
}
