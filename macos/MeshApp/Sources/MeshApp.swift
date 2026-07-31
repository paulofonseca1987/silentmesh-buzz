import AppKit
import SwiftUI

@main
struct MeshApp: App {
    @StateObject private var model = WorkspaceModel() ?? WorkspaceModel.unconfigured

    var body: some Scene {
        WindowGroup("Silent Mesh") {
            Group {
                if model.isConfigured {
                    ContentView(model: model)
                        .task {
                            // The snapshot races the load rather than
                            // following it: a capture that only happens on
                            // success is missing exactly when a picture
                            // would explain the most.
                            if let snapshot = SnapshotRequest.currentIfAny() {
                                Task { await snapshot.capture(model: model) }
                            }
                            await model.connect()
                        }
                } else {
                    NeedsConfigurationView()
                }
            }
        }
        .windowResizability(.contentSize)
    }
}

/// Headless self-capture.
///
/// Screenshotting the *display* over SSH needs macOS Screen Recording
/// permission, which an ssh-spawned process does not have. Rendering the
/// app's own view tree needs no permission at all, because the app is
/// capturing itself rather than the screen — so the visual feedback loop
/// works whether or not anyone is sitting at the machine.
///
/// `MESH_SNAPSHOT=/path/to.png` renders the window content once the
/// workspace has loaded, then exits.
struct SnapshotRequest {
    let path: String
    /// Seconds to let the relay round-trip complete before rendering, so
    /// the capture shows real data rather than an empty shell.
    let settle: TimeInterval

    static func currentIfAny(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> SnapshotRequest? {
        guard let path = environment["MESH_SNAPSHOT"] else { return nil }
        let settle = TimeInterval(environment["MESH_SNAPSHOT_SETTLE"] ?? "") ?? 2.5
        return SnapshotRequest(path: path, settle: settle)
    }

    @MainActor
    func capture(model: WorkspaceModel) async {
        try? await Task.sleep(for: .seconds(settle))

        // Render the REAL window's view tree, not a detached copy.
        //
        // `ImageRenderer` cannot render scene-level containers — handed a
        // `NavigationSplitView` it produces SwiftUI's unrenderable
        // placeholder (a red slash on yellow), which is a convincing image
        // of nothing. AppKit's `cacheDisplay` draws the actual on-screen
        // view hierarchy, so the capture shows the app as it really is —
        // and because the app is drawing itself rather than reading the
        // display, it needs no Screen Recording permission and works over
        // SSH.
        guard let window = NSApplication.shared.windows.first(where: { $0.isVisible }),
            let view = window.contentView,
            let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
        else {
            FileHandle.standardError.write(Data("snapshot: no visible window\n".utf8))
            NSApplication.shared.terminate(nil)
            return
        }
        view.cacheDisplay(in: view.bounds, to: rep)
        // `cacheDisplay` walks the view's draw() path and misses
        // layer-backed subviews; the layer tree misses window-server
        // composited materials (a SwiftUI sidebar `List` is both). The PDF
        // display list catches AppKit-drawn content the other two paths
        // skip, so it is tried first and the bitmap is the fallback.
        if let pdf = try? view.dataWithPDF(inside: view.bounds),
            let pdfImage = NSImage(data: pdf),
            let pdfTiff = pdfImage.tiffRepresentation,
            let pdfRep = NSBitmapImageRep(data: pdfTiff),
            let pdfPNG = pdfRep.representation(using: .png, properties: [:]),
            pdfPNG.count > 20_000
        {
            try? pdfPNG.write(to: URL(fileURLWithPath: path))
            FileHandle.standardError.write(
                Data("snapshot: wrote \(path) via PDF display list\n".utf8))
            FileHandle.standardError.write(
                Data("snapshot: model has \(model.channels.count) channels, \(model.messages.count) messages, status=\(model.status)\n".utf8))
            NSApplication.shared.terminate(nil)
            return
        }
        guard let png = rep.representation(using: .png, properties: [:]) else {
            FileHandle.standardError.write(Data("snapshot: encode failed\n".utf8))
            NSApplication.shared.terminate(nil)
            return
        }
        do {
            try png.write(to: URL(fileURLWithPath: path))
            FileHandle.standardError.write(
                Data("snapshot: model has \(model.channels.count) channels, \(model.messages.count) messages, status=\(model.status)\n".utf8))
            FileHandle.standardError.write(
                Data("snapshot: wrote \(path) (\(Int(view.bounds.width))x\(Int(view.bounds.height)))\n".utf8))
        } catch {
            FileHandle.standardError.write(Data("snapshot: \(error)\n".utf8))
        }
        NSApplication.shared.terminate(nil)
    }
}
