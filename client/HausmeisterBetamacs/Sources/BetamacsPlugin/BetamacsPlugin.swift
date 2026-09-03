import AppKit
import CryptoKit
import HausmeisterKit

/// Manages betamacs — the screen censor — on this Mac through otactl's
/// firmware pipeline: signed settings envelopes (app "betamacs-config")
/// and app updates (app "betamacs", macos-app-zip) come over the device
/// mTLS channel and are handed to betamacsd, the root watchdog daemon
/// that owns the managed install, over its unix socket. The daemon
/// re-verifies every envelope against its pinned otactl root, so this
/// plugin is a courier, not an authority (betamacs/docs/managed-mode.md).
///
/// The plugin also surfaces betamacs health (the daemon's heartbeat view
/// of the censor) in the menu, which is the tamper-evidence path: a
/// silent or capture-blind censor shows up here, and later in otactl.
public final class BetamacsPlugin: HausmeisterPlugin {
  public static let id = "betamacs"
  public let title = "Betamacs"

  static let appName = "betamacs"
  static let configApp = "betamacs-config"
  static let arch = "arm64"
  static let appFormat = "macos-app-zip"

  private let host: HostServices
  private var checking = false
  /// Last daemon status, refreshed each tick for the menu.
  private var statusLine = "status unknown"

  public required init(host: HostServices) {
    self.host = host
  }

  // MARK: menu

  public func menuItems() -> [NSMenuItem] {
    let status = NSMenuItem(title: "betamacs: \(statusLine)", action: nil, keyEquivalent: "")
    status.isEnabled = false
    return [status]
  }

  // MARK: lifecycle

  public func activate() { tick() }

  /// Revocation stops updates and drops the menu section, but never
  /// uninstalls: betamacs is a safety control, and removing it is a
  /// deliberate operator action on the Mac, not a side effect.
  public func deactivate() {}

  public func tick() {
    refreshStatus()
    guard entitled, DaemonSocket.available, !checking else { return }
    checking = true
    Task { [weak self] in
      guard let self else { return }
      do { _ = try await self.checkAndPushConfig() } catch {
        self.host.log.error("betamacs: config: \(error)")
      }
      do { _ = try await self.checkAndInstallApp() } catch {
        self.host.log.error("betamacs: app: \(error)")
      }
      await MainActor.run {
        self.checking = false
        self.refreshStatus()
      }
    }
  }

  public func checkForUpdates() async -> String? {
    guard entitled else {
      return host.entitlements == nil ? nil : "Betamacs is not authorized for this Mac."
    }
    guard DaemonSocket.available else {
      return "Betamacs: betamacsd is not installed — run the managed installer once."
    }
    guard !checking else { return nil }
    checking = true
    defer { checking = false }
    var lines: [String] = []
    do { lines.append("Betamacs settings: \(try await checkAndPushConfig()).") }
    catch { lines.append("Betamacs settings: check failed — \(error).") }
    do { lines.append("Betamacs app: \(try await checkAndInstallApp()).") }
    catch { lines.append("Betamacs app: check failed — \(error).") }
    return lines.joined(separator: "\n")
  }

  // MARK: entitlement + status

  /// The cross-app firmware fetches need ext: grants; the server enforces
  /// the same with 403 NOT_ENTITLED, so this is about not asking.
  private var entitled: Bool {
    host.entitlements?.extensions.contains { $0.app == BetamacsPlugin.configApp } ?? false
  }

  private func refreshStatus() {
    guard DaemonSocket.available else {
      statusLine = "daemon not installed"
      return
    }
    guard let reply = try? DaemonSocket.roundTrip(["type": "status"]),
          reply["ok"] as? Bool == true else {
      statusLine = "daemon unreachable"
      return
    }
    let age = reply["heartbeatAgeSecs"] as? Int ?? -1
    let captureOk = reply["captureOk"] as? Bool ?? false
    let epoch = reply["configEpoch"] as? Int ?? 0
    if age < 0 {
      statusLine = "agent has not reported yet"
    } else if age > 60 {
      statusLine = "agent silent for \(age)s"
    } else if !captureOk {
      statusLine = "capture unhealthy (Screen Recording?)"
    } else {
      statusLine = "healthy · config epoch \(epoch)"
    }
  }

  private func daemonConfigEpoch() -> UInt64 {
    guard let reply = try? DaemonSocket.roundTrip(["type": "status"]),
          let epoch = reply["configEpoch"] as? Int, epoch >= 0 else { return 0 }
    return UInt64(epoch)
  }

  // MARK: settings envelopes

  private func checkAndPushConfig() async throws -> String {
    let (response, client) = try await fetchManifest(app: BetamacsPlugin.configApp)
    let m = response.manifest
    let daemonEpoch = daemonConfigEpoch()
    if let epoch = m.epoch, epoch <= daemonEpoch {
      return "up to date (epoch \(daemonEpoch))"
    }
    let artifact = try await fetchArtifact(app: BetamacsPlugin.configApp, manifest: m, client: client)
    try deliver(type: "envelope", response: response, artifact: artifact)
    host.log.notice("betamacs: pushed config \(m.version) (epoch \(m.epoch ?? 0))")
    await MainActor.run {
      host.notify(title: "Betamacs settings updated",
                  body: "Configuration \(m.version) is verified and applied.")
    }
    return "pushed \(m.version) (epoch \(m.epoch ?? 0))"
  }

  // MARK: app updates

  private func installedAppVersion() -> String {
    let plist = NSDictionary(contentsOfFile: "/Applications/betamacs.app/Contents/Info.plist")
    return plist?["CFBundleShortVersionString"] as? String ?? ""
  }

  private func checkAndInstallApp() async throws -> String {
    let (response, client) = try await fetchManifest(app: BetamacsPlugin.appName)
    let m = response.manifest
    if let format = m.format, !format.isEmpty, format != BetamacsPlugin.appFormat {
      throw ReleaseError.badArchive("unexpected artifact format \(format)")
    }
    let installedEpoch = UInt64(host.settings.int("appEpoch"))
    if let epoch = m.epoch, epoch < installedEpoch {
      throw ReleaseError.rollback("epoch \(epoch) is below the installed \(installedEpoch)")
    }
    let installed = installedAppVersion()
    let newer = installed.isEmpty
      || Version.compare(m.version, installed) == .orderedDescending
      || (m.epoch ?? 0) > installedEpoch && Version.compare(m.version, installed) != .orderedAscending
    host.log.notice("betamacs: app installed \(installed.isEmpty ? "nothing" : installed), offered \(m.version) (epoch \(m.epoch ?? 0), channel \(response.channel)) -> \(newer ? "update" : "current")")
    guard newer else { return "up to date (\(installed))" }

    let artifact = try await fetchArtifact(app: BetamacsPlugin.appName, manifest: m, client: client)
    try deliver(type: "app", response: response, artifact: artifact)
    host.settings.set(Int(m.epoch ?? 0), for: "appEpoch")
    host.log.notice("betamacs: daemon installed app \(m.version)")
    await MainActor.run {
      host.notify(title: "Betamacs updated",
                  body: "betamacs \(m.version) is installed and running.")
    }
    return "installed \(m.version)"
  }

  // MARK: shared plumbing

  private func fetchManifest(app: String) async throws -> (ReleaseResponse, DeviceClient) {
    guard let identity = host.identity else { throw EnrollError.notEnrolled }
    let client = try host.deviceClient()
    let data = try await client.get(path: "firmware/manifest?app=\(app)&arch=\(BetamacsPlugin.arch)")
    let response = try JSONDecoder().decode(ReleaseResponse.self, from: data)
    try ManifestVerifier.verify(response, rootPEM: identity.rootPEM)
    guard response.manifest.app == app, response.manifest.arch == BetamacsPlugin.arch else {
      throw ReleaseError.badSignature(
        "manifest is for \(response.manifest.app)/\(response.manifest.arch), not \(app)/\(BetamacsPlugin.arch)")
    }
    return (response, client)
  }

  private func fetchArtifact(app: String, manifest m: ReleaseManifest, client: DeviceClient) async throws -> Data {
    let data = try await client.get(
      path: "firmware/artifacts/current?app=\(app)&arch=\(BetamacsPlugin.arch)&version=\(m.version)")
    let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    guard digest == m.sha256.lowercased() else { throw ReleaseError.hashMismatch }
    return data
  }

  /// Hand a verified release to betamacsd as a self-authenticating
  /// envelope; the daemon re-verifies, persists/installs, and replies.
  private func deliver(type: String, response: ReleaseResponse, artifact: Data) throws {
    let reply = try DaemonSocket.roundTrip(envelopeMessage(type: type, response: response, artifact: artifact))
    guard reply["ok"] as? Bool == true else {
      throw DaemonSocket.DaemonError(
        description: "betamacsd refused the envelope: \(reply["error"] as? String ?? "unknown error")")
    }
  }

  private func envelopeMessage(type: String, response: ReleaseResponse, artifact: Data) -> [String: Any] {
    let m = response.manifest
    var manifest: [String: Any] = [
      "app": m.app,
      "arch": m.arch,
      "version": m.version,
      "filename": m.filename,
      "sha256": m.sha256,
      "releasedAt": m.releasedAt,
    ]
    if let v = m.epoch { manifest["epoch"] = v }
    if let v = m.buildDtm { manifest["buildDtm"] = v }
    if let v = m.gitHash { manifest["gitHash"] = v }
    if let v = m.role { manifest["role"] = v }
    if let v = m.format { manifest["format"] = v }
    if let v = m.board { manifest["board"] = v }
    if let v = m.installMode { manifest["installMode"] = v }
    var message: [String: Any] = [
      "type": type,
      "manifest": manifest,
      "signature": response.signature,
      "artifact": artifact.base64EncodedString(),
    ]
    if let v = response.signatureAlgorithm { message["signatureAlgorithm"] = v }
    if let v = response.signingCertificate { message["signingCertificate"] = v }
    if let v = response.signingCertificateChain { message["signingCertificateChain"] = v }
    return message
  }
}
