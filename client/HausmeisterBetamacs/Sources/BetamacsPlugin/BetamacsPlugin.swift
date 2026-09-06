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
  static let tasksApp = "betamacs-tasks"
  static let arch = "arm64"
  static let appFormat = "macos-app-zip"

  private let host: HostServices
  private var checking = false
  /// The Login Items approval prompt should not nag hourly; the
  /// bootstrap is attempted once per hausmeister run.
  private var bootstrapAttempted = false
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
    guard entitled else { return }
    guard DaemonSocket.available else {
      bootstrapIfNeeded()
      return
    }
    guard !checking else { return }
    checking = true
    Task { [weak self] in
      guard let self else { return }
      do { _ = try await self.checkAndPushConfig() } catch {
        self.host.log.error("betamacs: config: \(error)")
      }
      if self.tasksEntitled {
        do { _ = try await self.checkAndPushTasks() } catch {
          self.host.log.error("betamacs: tasks: \(error)")
        }
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

  /// The host's Check for Updates… table: a row each for the app, the
  /// settings envelope, and the task bank (when granted), with what the
  /// daemon holds beside what otactl offers. Nothing is delivered here; a
  /// row that is behind carries the push for its Update button.
  public func updateItems() async -> [UpdateItem] {
    guard entitled else {
      return host.entitlements == nil ? [] : [.note(.application, "betamacs", status: "Not authorized", detail: "Betamacs is not authorized for this Mac.")]
    }
    guard DaemonSocket.available else {
      return [.note(.application, "betamacs", installed: installedAppVersion(), status: "Not installed",
                    detail: "betamacsd is not installed — run the managed installer once.")]
    }
    guard !checking else {
      return [.note(.application, "betamacs", status: "Busy", detail: "A background check is running; refresh in a moment.")]
    }
    checking = true
    defer { checking = false }
    var items: [UpdateItem] = []
    do {
      let found = try await checkApp()
      let m = found.release.manifest
      // The release's commit is the installed one once the release is
      // what is installed; before that it is not on record.
      items.append(UpdateItem(.application, "betamacs", installed: found.installed, available: m.version,
                              commit: found.installed == m.version ? m.gitHash ?? "" : "",
                              install: found.newer ? { [weak self] in try await self?.fromTable { try await self?.installApp(found) } } : nil))
    } catch {
      items.append(.note(.application, "betamacs", installed: installedAppVersion(), status: "Check failed", detail: "\(error)"))
    }
    items.append(await envelopeItem(name: "betamacs settings", app: BetamacsPlugin.configApp, daemonEpoch: daemonConfigEpoch()) { [weak self] found in
      try await self?.pushConfig(found)
    })
    if tasksEntitled {
      items.append(await envelopeItem(name: "betamacs task bank", app: BetamacsPlugin.tasksApp, daemonEpoch: daemonTasksEpoch()) { [weak self] found in
        try await self?.pushTasks(found)
      })
    }
    return items
  }

  /// One envelope row, versions side by side, and the push when otactl's
  /// epoch is ahead of the daemon's. The daemon reports only an epoch, so
  /// the installed version is the manifest's when the epochs agree, else
  /// the version this plugin pushed for that epoch (see recordPushed),
  /// else unknown. Epochs are otactl's anti-rollback counter, opaque to
  /// the user, and stay out of the table.
  private func envelopeItem(name: String, app: String, daemonEpoch: UInt64,
                            push: @escaping (EnvelopeCheck) async throws -> Void) async -> UpdateItem {
    let recorded = pushedVersion(app: app, epoch: daemonEpoch)
    do {
      let found = try await checkEnvelope(app: app, daemonEpoch: daemonEpoch)
      let m = found.response.manifest
      let current = daemonEpoch != 0 && m.epoch == daemonEpoch
      return UpdateItem(.configuration, name, installed: current ? m.version : recorded, available: m.version,
                        commit: current ? m.gitHash ?? "" : "",
                        install: found.newer ? { [weak self] in try await self?.fromTable { try await push(found) } } : nil)
    } catch {
      return .note(.configuration, name, installed: recorded, status: "Check failed", detail: "\(error)")
    }
  }

  /// The version last delivered for an app, if the daemon still holds
  /// that epoch.
  private func pushedVersion(app: String, epoch: UInt64) -> String {
    guard epoch != 0, UInt64(host.settings.int("\(app).pushedEpoch")) == epoch else { return "" }
    return host.settings.string("\(app).pushedVersion") ?? ""
  }

  private func recordPushed(app: String, manifest m: ReleaseManifest) {
    host.settings.set(m.version, for: "\(app).pushedVersion")
    host.settings.set(Int(m.epoch ?? 0), for: "\(app).pushedEpoch")
  }

  /// An Update button's work, guarded against the hourly check that may
  /// be running by then, and followed by a status refresh for the menu.
  private func fromTable(_ work: () async throws -> Void) async throws {
    guard !checking else { throw Busy() }
    checking = true
    defer { checking = false }
    try await work()
    await MainActor.run { refreshStatus() }
  }

  struct Busy: Error, CustomStringConvertible {
    var description: String { "a background check is running; try again in a moment" }
  }

  // MARK: entitlement + status

  /// The cross-app firmware fetches need ext: grants; the server enforces
  /// the same with 403 NOT_ENTITLED, so this is about not asking.
  private var entitled: Bool {
    host.entitlements?.extensions.contains { $0.app == BetamacsPlugin.configApp } ?? false
  }

  /// The task bank is a separate cross-app fetch with its own ext: grant;
  /// skip it cleanly when ungranted rather than provoke a 403.
  private var tasksEntitled: Bool {
    host.entitlements?.extensions.contains { $0.app == BetamacsPlugin.tasksApp } ?? false
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

  private func daemonTasksEpoch() -> UInt64 {
    guard let reply = try? DaemonSocket.roundTrip(["type": "status"]),
          let epoch = reply["tasksEpoch"] as? Int, epoch >= 0 else { return 0 }
    return UInt64(epoch)
  }

  // MARK: envelopes (settings, task bank)

  /// What a manifest fetch says about an envelope artifact: otactl's
  /// release beside the epoch the daemon holds.
  struct EnvelopeCheck {
    let response: ReleaseResponse
    let client: DeviceClient
    let daemonEpoch: UInt64
    /// Behind, or an unversioned manifest, which is always delivered.
    var newer: Bool {
      guard let epoch = response.manifest.epoch else { return true }
      return epoch > daemonEpoch
    }
  }

  private func checkEnvelope(app: String, daemonEpoch: UInt64) async throws -> EnvelopeCheck {
    let (response, client) = try await fetchManifest(app: app)
    return EnvelopeCheck(response: response, client: client, daemonEpoch: daemonEpoch)
  }

  /// Fetches the artifact and hands it to the daemon as a signed envelope.
  private func pushEnvelope(type: String, app: String, _ found: EnvelopeCheck) async throws {
    let m = found.response.manifest
    let artifact = try await fetchArtifact(app: app, manifest: m, client: found.client)
    try deliver(type: type, response: found.response, artifact: artifact)
  }

  /// Fetch and deliver the challenge task bank (`betamacs-tasks`) when a
  /// newer epoch is available — same signed-envelope courier flow as
  /// config, to a separate artifact so questions version independently.
  private func checkAndPushTasks() async throws -> String {
    let found = try await checkEnvelope(app: BetamacsPlugin.tasksApp, daemonEpoch: daemonTasksEpoch())
    guard found.newer else { return "up to date (epoch \(found.daemonEpoch))" }
    try await pushTasks(found)
    let m = found.response.manifest
    return "pushed \(m.version) (epoch \(m.epoch ?? 0))"
  }

  private func pushTasks(_ found: EnvelopeCheck) async throws {
    try await pushEnvelope(type: "tasks", app: BetamacsPlugin.tasksApp, found)
    let m = found.response.manifest
    recordPushed(app: BetamacsPlugin.tasksApp, manifest: m)
    host.log.notice("betamacs: pushed task bank \(m.version) (epoch \(m.epoch ?? 0))")
  }

  private func checkAndPushConfig() async throws -> String {
    let found = try await checkEnvelope(app: BetamacsPlugin.configApp, daemonEpoch: daemonConfigEpoch())
    guard found.newer else { return "up to date (epoch \(found.daemonEpoch))" }
    try await pushConfig(found)
    let m = found.response.manifest
    return "pushed \(m.version) (epoch \(m.epoch ?? 0))"
  }

  private func pushConfig(_ found: EnvelopeCheck) async throws {
    try await pushEnvelope(type: "envelope", app: BetamacsPlugin.configApp, found)
    let m = found.response.manifest
    recordPushed(app: BetamacsPlugin.configApp, manifest: m)
    host.log.notice("betamacs: pushed config \(m.version) (epoch \(m.epoch ?? 0))")
    await MainActor.run {
      host.notify(title: "Betamacs settings updated",
                  body: "Configuration \(m.version) is verified and applied.")
    }
  }

  // MARK: app updates

  private func installedAppVersion() -> String {
    let plist = NSDictionary(contentsOfFile: "/Applications/betamacs.app/Contents/Info.plist")
    return plist?["CFBundleShortVersionString"] as? String ?? ""
  }

  /// What a manifest fetch says about the app: what /Applications holds,
  /// what otactl offers, and whether the offer is ahead.
  struct AppCheck {
    let installed: String
    let release: ReleaseResponse
    let client: DeviceClient
    let newer: Bool
  }

  private func checkAndInstallApp() async throws -> String {
    let found = try await checkApp()
    guard found.newer else { return "up to date (\(found.installed))" }
    try await installApp(found)
    return "installed \(found.release.manifest.version)"
  }

  private func checkApp() async throws -> AppCheck {
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
    return AppCheck(installed: installed, release: response, client: client, newer: newer)
  }

  /// Fetches the bundle `checkApp` found and has the daemon install it.
  private func installApp(_ found: AppCheck) async throws {
    let m = found.release.manifest
    let artifact = try await fetchArtifact(app: BetamacsPlugin.appName, manifest: m, client: found.client)
    try deliver(type: "app", response: found.release, artifact: artifact)
    host.settings.set(Int(m.epoch ?? 0), for: "appEpoch")
    host.log.notice("betamacs: daemon installed app \(m.version)")
    await MainActor.run {
      host.notify(title: "Betamacs updated",
                  body: "betamacs \(m.version) is installed and running.")
    }
  }

  // MARK: bootstrap

  /// Where betamacs is downloaded when /Applications isn't writable (the
  /// non-admin managed Mac): the user's Downloads folder, the natural
  /// place to drag an app to /Applications from. The drag triggers
  /// Finder's authenticated copy — macOS asks for an admin password and
  /// any admin can authorize — which is how the bundle reaches the
  /// privileged location without hausmeister ever elevating.
  private var downloadedAppURL: URL {
    FileManager.default.homeDirectoryForCurrentUser
      .appendingPathComponent("Downloads/betamacs.app", isDirectory: true)
  }

  /// No daemon on this Mac: it is being onboarded. Fetch and verify the
  /// app through the signed pipeline, then get it somewhere runnable.
  ///
  /// hausmeister runs as the logged-in user with no root, so it can only
  /// place the bundle where that user can write. On an admin Mac that is
  /// /Applications directly, and we then register betamacsd via
  /// SMAppService (one Login Items approval). On a NON-admin Mac
  /// /Applications is read-only, so we instead download the verified
  /// bundle to Downloads and reveal it: the user drags it to Applications,
  /// macOS asks for an admin password (Finder's authenticated copy), and
  /// then opens it — the hand launch self-installs the per-user agent and,
  /// on the managed build, raises the betamacsd approval an admin
  /// completes. hausmeister never tries to elevate; the download is the
  /// whole of its job here.
  private func bootstrapIfNeeded() {
    guard !bootstrapAttempted, !checking else { return }
    bootstrapAttempted = true
    // Already downloaded and waiting for the user to drag it across —
    // don't re-fetch every run. Once it reaches /Applications and is
    // opened (daemon approved for full managed mode) the socket appears
    // and the normal update path takes over.
    if FileManager.default.fileExists(atPath: downloadedAppURL.path),
       !FileManager.default.fileExists(atPath: "/Applications/betamacs.app") {
      statusLine = "downloaded — drag betamacs to Applications to install"
      return
    }
    checking = true
    Task { [weak self] in
      guard let self else { return }
      do {
        let alreadyInstalled = FileManager.default.fileExists(atPath: "/Applications/betamacs.app")
        let placed: (path: String, privileged: Bool) =
          alreadyInstalled ? ("/Applications/betamacs.app", true) : try await self.downloadAndPlaceApp()
        if placed.privileged {
          try BetamacsPlugin.run("\(placed.path)/Contents/MacOS/betamacs", ["install-daemon"])
          self.host.log.notice("betamacs: bootstrap: daemon registration requested")
          await MainActor.run {
            self.statusLine = "approve betamacsd: System Settings → Login Items"
            self.host.notify(
              title: "Betamacs needs one approval",
              body: "Allow \"betamacs\" under System Settings → Login Items to finish setup.")
          }
        } else {
          self.host.log.notice("betamacs: downloaded to \(placed.path) — awaiting drag to /Applications")
          await MainActor.run {
            self.statusLine = "downloaded — drag betamacs to Applications to install"
            // Reveal it so the drag target is obvious.
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: placed.path)])
            self.host.notify(
              title: "Betamacs is ready to install",
              body: "In your Downloads: drag betamacs to the Applications folder (you’ll be asked for an admin password), then open it.")
          }
        }
      } catch {
        self.host.log.error("betamacs: bootstrap: \(error)")
        await MainActor.run { self.statusLine = "bootstrap failed — \(error)" }
      }
      await MainActor.run { self.checking = false }
    }
  }

  /// First install only: fetch the verified bundle and put it somewhere
  /// runnable. Prefers /Applications (updates then flow through the root
  /// daemon); when the user can't write there — a non-admin managed Mac,
  /// the common case — downloads it to ~/Downloads instead and reports
  /// which happened, so the caller can either register the daemon or hand
  /// the download to the user to drag across.
  private func downloadAndPlaceApp() async throws -> (path: String, privileged: Bool) {
    let (response, client) = try await fetchManifest(app: BetamacsPlugin.appName)
    let m = response.manifest
    if let format = m.format, !format.isEmpty, format != BetamacsPlugin.appFormat {
      throw ReleaseError.badArchive("unexpected artifact format \(format)")
    }
    let artifact = try await fetchArtifact(app: BetamacsPlugin.appName, manifest: m, client: client)
    let fm = FileManager.default
    let staging = fm.temporaryDirectory
      .appendingPathComponent("betamacs-\(UUID().uuidString)", isDirectory: true)
    try fm.createDirectory(at: staging, withIntermediateDirectories: true)
    defer { try? fm.removeItem(at: staging) }
    let archive = staging.appendingPathComponent("app.zip")
    try artifact.write(to: archive)
    let unpacked = staging.appendingPathComponent("unpacked", isDirectory: true)
    try fm.createDirectory(at: unpacked, withIntermediateDirectories: true)
    try BetamacsPlugin.run("/usr/bin/ditto", ["-x", "-k", archive.path, unpacked.path])
    let entries = (try? fm.contentsOfDirectory(at: unpacked, includingPropertiesForKeys: nil))?
      .filter { !$0.lastPathComponent.hasPrefix(".") } ?? []
    guard let bundle = entries.first(where: { $0.pathExtension == "app" }) else {
      throw ReleaseError.badArchive("no .app at the archive root")
    }
    try BetamacsPlugin.run("/usr/bin/codesign", ["--verify", "--strict", bundle.path])
    host.settings.set(Int(m.epoch ?? 0), for: "appEpoch")

    // Privileged location first; fall back to the user's own Applications.
    let applications = URL(fileURLWithPath: "/Applications/betamacs.app")
    do {
      try? fm.removeItem(at: applications)
      try fm.moveItem(at: bundle, to: applications)
      host.log.notice("betamacs: bootstrap installed \(m.version) at \(applications.path)")
      return (applications.path, true)
    } catch {
      let downloads = downloadedAppURL.deletingLastPathComponent()
      try fm.createDirectory(at: downloads, withIntermediateDirectories: true)
      try? fm.removeItem(at: downloadedAppURL)
      try fm.moveItem(at: bundle, to: downloadedAppURL)
      host.log.notice(
        "betamacs: /Applications not writable, downloaded \(m.version) to \(downloadedAppURL.path) for drag-to-install")
      return (downloadedAppURL.path, false)
    }
  }

  static func run(_ tool: String, _ args: [String]) throws {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: tool)
    p.arguments = args
    let err = Pipe()
    p.standardError = err
    try p.run()
    p.waitUntilExit()
    guard p.terminationStatus == 0 else {
      let text = String(decoding: err.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
      throw ReleaseError.install("\(tool) failed: \(text.trimmingCharacters(in: .whitespacesAndNewlines))")
    }
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
