import SwiftUI

/// The settings window: a sidebar of panes, like System Settings. What comes
/// from the fleet repository sits under Fleet; each service the app runs
/// gets its own section below it.
struct SettingsView: View {
    let nebula: NebulaModel
    let updater: UpdaterModel

    @State private var pane: Pane = .general

    enum Pane: Hashable {
        case general
        case fleet
        case nebulaOverview
        case nebulaMesh
        case nebulaPeers
        case nebulaConfig
    }

    var body: some View {
        NavigationSplitView {
            List(selection: $pane) {
                Label("General", systemImage: "gearshape")
                    .tag(Pane.general)
                Label("Fleet", systemImage: "server.rack")
                    .tag(Pane.fleet)
                Section("Nebula") {
                    Label("Overview", systemImage: "point.3.connected.trianglepath.dotted")
                        .tag(Pane.nebulaOverview)
                    Label("Mesh", systemImage: "circle.hexagongrid")
                        .tag(Pane.nebulaMesh)
                    Label("Peers", systemImage: "list.bullet")
                        .tag(Pane.nebulaPeers)
                    Label("Config", systemImage: "slider.horizontal.3")
                        .tag(Pane.nebulaConfig)
                }
            }
            .navigationSplitViewColumnWidth(190)
            .toolbar(removing: .sidebarToggle)
        } detail: {
            switch pane {
            case .general:
                GeneralSettingsView(updater: updater)
            case .fleet:
                FleetSettingsView(nebula: nebula)
            case .nebulaOverview:
                NebulaOverviewView(nebula: nebula)
            case .nebulaMesh:
                NebulaMeshView(nebula: nebula)
            case .nebulaPeers:
                NebulaPeersView()
            case .nebulaConfig:
                NebulaConfigView(nebula: nebula)
            }
        }
        .frame(minWidth: 680, minHeight: 440)
        .onAppear {
            // A menu bar app is never frontmost on its own.
            NSApplication.shared.activate()
        }
    }
}
