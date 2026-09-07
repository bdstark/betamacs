import Foundation

/// Line-oriented JSON client for betamacsd's unix socket. The daemon
/// verifies every envelope against its own pinned otactl root, so this
/// channel carries self-authenticating messages and needs no auth of its
/// own (betamacs/docs/managed-mode.md).
enum DaemonSocket {
  static let path = "/var/run/betamacsd.sock"

  struct DaemonError: Error, CustomStringConvertible {
    let description: String
  }

  static var available: Bool { FileManager.default.fileExists(atPath: path) }

  /// An app-install envelope carries the whole zip and the daemon unpacks,
  /// code-checks, and swaps before replying.
  static let installTimeout: TimeInterval = 120
  /// A status query is answered from memory. If it takes longer than this
  /// the daemon is wedged, and the caller must report that rather than
  /// wait — every caller of `status()` is on a path the menu depends on.
  static let statusTimeout: TimeInterval = 5

  /// The daemon's status reply, or nil when there is no daemon, it is
  /// unreachable, or it does not answer within `statusTimeout`. Blocking:
  /// never call on the main thread.
  static func status() -> [String: Any]? {
    guard available, let reply = try? roundTrip(["type": "status"], timeout: statusTimeout),
          reply["ok"] as? Bool == true else { return nil }
    return reply
  }

  /// Send one JSON object (newline-terminated), read one JSON reply.
  /// Blocking, with `timeout` on both the send and the receive; the
  /// default suits an install envelope, status callers pass their own.
  static func roundTrip(_ message: [String: Any], timeout seconds: TimeInterval = installTimeout) throws -> [String: Any] {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { throw DaemonError(description: "socket: \(errnoText)") }
    defer { close(fd) }
    var timeout = timeval(tv_sec: Int(seconds), tv_usec: Int32((seconds - seconds.rounded(.down)) * 1_000_000))
    _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
    _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let fits = path.withCString { src in
      withUnsafeMutableBytes(of: &addr.sun_path) { dst -> Bool in
        let len = strlen(src)
        guard len + 1 <= dst.count else { return false }
        memcpy(dst.baseAddress!, src, len + 1)
        return true
      }
    }
    guard fits else { throw DaemonError(description: "socket path too long") }
    let rc = withUnsafePointer(to: &addr) {
      $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
        connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
      }
    }
    guard rc == 0 else { throw DaemonError(description: "connect \(path): \(errnoText)") }

    var data = try JSONSerialization.data(withJSONObject: message)
    data.append(0x0a)
    try data.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
      var offset = 0
      while offset < buf.count {
        let n = write(fd, buf.baseAddress!.advanced(by: offset), buf.count - offset)
        guard n > 0 else { throw DaemonError(description: "write: \(errnoText)") }
        offset += n
      }
    }
    shutdown(fd, SHUT_WR)

    var reply = Data()
    var chunk = [UInt8](repeating: 0, count: 4096)
    while true {
      let n = read(fd, &chunk, chunk.count)
      guard n > 0 else { break }
      reply.append(contentsOf: chunk[0..<n])
      if chunk[0..<n].contains(0x0a) { break }
    }
    guard !reply.isEmpty,
          let object = try? JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
      throw DaemonError(description: "no reply from betamacsd")
    }
    return object
  }

  private static var errnoText: String { String(cString: strerror(errno)) }
}
