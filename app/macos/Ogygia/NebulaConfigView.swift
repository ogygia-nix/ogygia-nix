import SwiftUI

/// Whether Nebula runs the fleet's config or one owned on this Mac. Following
/// the fleet, the pane shows the fleet's values and nothing can be changed;
/// managed locally, everything here comes from the Mac, while the
/// certificate and CA still come from the fleet.
struct NebulaConfigView: View {
    let nebula: NebulaModel

    @State private var mode: DaemonProtocol.ConfigMode = .fleet
    @State private var local = DaemonProtocol.LocalConfig()
    @State private var loaded = false
    @State private var saving = false
    @State private var message: String?

    var body: some View {
        Form {
            Section {
                Picker("Nebula config", selection: $mode) {
                    Text("Follow the fleet").tag(DaemonProtocol.ConfigMode.fleet)
                    Text("Manage on this Mac").tag(DaemonProtocol.ConfigMode.local)
                }
                .pickerStyle(.segmented)
                .disabled(managed)
                .onChange(of: mode) { _, mode in
                    // An empty local config would leave Nebula with no
                    // lighthouses, so start from the fleet's.
                    if mode == .local, local == DaemonProtocol.LocalConfig(),
                        let fleet = nebula.status?.fleetConfig
                    {
                        local = fleet
                    }
                }
                Text(explanation)
                    .foregroundStyle(.secondary)
                if let message {
                    Text(message)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }
                HStack {
                    if mode == .local {
                        Button("Copy from Fleet") {
                            if let fleet = nebula.status?.fleetConfig { local = fleet }
                        }
                        .disabled(nebula.status?.fleetConfig == nil)
                    }
                    Spacer()
                    Button("Revert") { loaded = false; load() }
                        .disabled(!changed)
                    Button("Save") { save() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(saving || managed || !changed)
                }
            }

            // Following a fleet not fetched yet, there is nothing to show.
            if mode == .local || nebula.status?.fleetConfig != nil {
                ConfigFields(config: shown, editable: mode == .local && !managed)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Nebula Config")
        .onAppear { load() }
        .onChange(of: nebula.status) { load() }
    }

    private var managed: Bool { nebula.status?.configModeManaged ?? false }

    /// The fleet's values while following it, and the local draft otherwise.
    private var shown: Binding<DaemonProtocol.LocalConfig> {
        if mode == .local { return $local }
        return .constant(nebula.status?.fleetConfig ?? DaemonProtocol.LocalConfig())
    }

    private var changed: Bool {
        guard let status = nebula.status else { return false }
        return mode != (status.configMode ?? .fleet)
            || (mode == .local && local != (status.localConfig ?? DaemonProtocol.LocalConfig()))
    }

    private var explanation: String {
        if managed {
            return "A configuration profile sets which config this Mac runs."
        }
        switch mode {
        case .fleet:
            return nebula.status?.fleetConfig == nil
                ? "Nebula will run the fleet's config once it has been fetched."
                : "Nebula runs the fleet's config, shown below."
        case .local:
            return "Nebula runs the config below. Anything left out uses Nebula's default, "
                + "not the fleet's. The certificate always comes from the fleet."
        }
    }

    private func load() {
        guard !loaded, let status = nebula.status else { return }
        mode = status.configMode ?? .fleet
        local = status.localConfig ?? DaemonProtocol.LocalConfig()
        loaded = true
    }

    private func save() {
        saving = true
        message = nil
        Task {
            message = await nebula.saveConfig(mode: mode, local: local)
            saving = false
        }
    }
}

/// The parts of the Nebula config a Mac may own.
private struct ConfigFields: View {
    @Binding var config: DaemonProtocol.LocalConfig
    let editable: Bool

    var body: some View {
        Section("Lighthouses") {
            ForEach($config.lighthouses) { $lighthouse in
                HStack {
                    TextField("Address", text: $lighthouse.address, prompt: prompt("10.42.0.1"))
                    TextField(
                        "Endpoints", value: $lighthouse.endpoints, format: CommaList(),
                        prompt: prompt("lighthouse.example.com:4242"))
                    removeButton { config.lighthouses.removeAll { $0.id == lighthouse.id } }
                }
                .labelsHidden()
            }
            addButton("Add Lighthouse") {
                config.lighthouses.append(.init(address: "", endpoints: []))
            }
        }
        .disabled(!editable)

        Section("Relays") {
            Toggle("Reach peers through relays", isOn: $config.useRelays)
            TextField("Relays", value: $config.relays, format: CommaList(), prompt: prompt("10.42.0.2, 10.42.0.3"))
            TextField(
                "Listen port", value: $config.listenPort, format: .number.grouping(.never), prompt: prompt("0 for any"))
        }
        .disabled(!editable)

        RulesSection(title: "Inbound firewall", rules: $config.inbound, editable: editable)
        RulesSection(title: "Outbound firewall", rules: $config.outbound, editable: editable)
    }

    /// Example text, which read-only fields leave out so it is not taken for
    /// a value.
    private func prompt(_ text: String) -> Text? {
        editable ? Text(text) : nil
    }

    private func addButton(_ title: String, action: @escaping () -> Void) -> some View {
        HStack {
            Spacer()
            Button(title, action: action)
        }
    }
}

private struct RulesSection: View {
    let title: String
    @Binding var rules: [DaemonProtocol.FirewallRule]
    let editable: Bool

    var body: some View {
        Section {
            ForEach($rules) { $rule in
                HStack {
                    TextField("Port", text: $rule.port, prompt: prompt("any"))
                        .frame(width: 80)
                    Picker("Protocol", selection: $rule.proto) {
                        ForEach(["any", "tcp", "udp", "icmp"], id: \.self) { Text($0).tag($0) }
                    }
                    .frame(width: 90)
                    TextField("Host", text: optional($rule.host), prompt: prompt("Host"))
                    TextField("Groups", value: groups($rule.groups), format: CommaList(), prompt: prompt("Groups"))
                    TextField("CIDR", text: optional($rule.cidr), prompt: prompt("CIDR"))
                    removeButton { rules.removeAll { $0.id == rule.id } }
                }
                .labelsHidden()
            }
            HStack {
                Spacer()
                Button("Add Rule") {
                    rules.append(.init(port: "any", proto: "any", host: "any"))
                }
            }
        } header: {
            Text(title)
        } footer: {
            if rules.isEmpty {
                Text(title.hasPrefix("Inbound") ? "Nothing may connect to this Mac." : "This Mac may connect to nothing.")
                    .foregroundStyle(.secondary)
            }
        }
        .disabled(!editable)
    }

    private func prompt(_ text: String) -> Text? {
        editable ? Text(text) : nil
    }

    /// An emptied field removes the groups rather than leaving an empty
    /// list, which Nebula would read as matching no group.
    private func groups(_ binding: Binding<[String]?>) -> Binding<[String]> {
        Binding(
            get: { binding.wrappedValue ?? [] },
            set: { binding.wrappedValue = $0.isEmpty ? nil : $0 })
    }
}

private func removeButton(_ action: @escaping () -> Void) -> some View {
    Button(action: action) {
        Image(systemName: "minus.circle")
    }
    .buttonStyle(.borderless)
    .help("Remove")
}

/// A comma-separated field for a list of strings, read when the field is
/// committed so a comma being typed is not lost.
private struct CommaList: ParseableFormatStyle {
    var parseStrategy: Strategy { Strategy() }

    func format(_ value: [String]) -> String {
        value.joined(separator: ", ")
    }

    struct Strategy: ParseStrategy {
        func parse(_ value: String) throws -> [String] {
            value.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        }
    }
}

/// An empty field for an absent value.
private func optional(_ binding: Binding<String?>) -> Binding<String> {
    Binding(
        get: { binding.wrappedValue ?? "" },
        set: {
            let trimmed = $0.trimmingCharacters(in: .whitespaces)
            binding.wrappedValue = trimmed.isEmpty ? nil : trimmed
        })
}
