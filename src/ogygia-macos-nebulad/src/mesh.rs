//! The mesh as this Mac sees it, from Nebula's debug commands and the fleet
//! topology.
//!
//! A node only knows its own tunnels, so the mesh is centred on this Mac:
//! every host it has a tunnel to or is handshaking with, how each is reached,
//! and the fleet's other hosts, which it has no tunnel to.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

/// The fleet's hosts by name, as the topology lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    /// Each host's mesh address.
    pub hosts: BTreeMap<String, String>,
    pub lighthouses: Vec<String>,
    pub relays: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mesh {
    /// Every other host, sorted by name.
    pub hosts: Vec<Peer>,
    /// Pairs of hosts this Mac relays traffic between, when it is a relay.
    pub forwarding: Vec<Forward>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// The host's name in the topology, or else in its certificate.
    pub name: Option<String>,
    pub address: String,
    pub state: PeerState,
    pub lighthouse: bool,
    pub relay: bool,
    /// The address outside the mesh the tunnel uses, when it is direct.
    pub remote: Option<String>,
    /// The relays the tunnel goes through, when it is relayed.
    pub via: Vec<String>,
    pub cert_expires: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    Connected,
    Handshaking,
    /// In the fleet, but with no tunnel from this Mac.
    NoTunnel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub from: String,
    pub to: String,
}

/// One entry of `list-hostmap -json` or `list-pending-hostmap -json`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostInfo {
    vpn_addrs: Vec<String>,
    /// Empty when the tunnel is relayed.
    #[serde(default)]
    current_remote: String,
    #[serde(default)]
    current_relays_to_me: Option<Vec<String>>,
    cert: Option<Cert>,
}

#[derive(Debug, Deserialize)]
struct Cert {
    details: CertDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CertDetails {
    name: Option<String>,
    not_after: Option<DateTime<Utc>>,
}

/// `print-relays`, which prints JSON without being asked.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Relays {
    relays: Option<Vec<Relay>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Relay {
    nebula_addr: String,
    relay_for_addrs: Option<Vec<RelayFor>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RelayFor {
    #[serde(rename = "Type")]
    kind: String,
    state: String,
    peer_addr: String,
}

/// What the debug commands printed.
pub struct Output<'a> {
    pub hostmap: &'a [u8],
    pub pending: &'a [u8],
    pub relays: &'a [u8],
}

/// Combine Nebula's view with the topology, leaving out this Mac itself.
pub fn assemble(output: Output, topology: &Topology, own_address: Option<&str>) -> Result<Mesh> {
    let hostmap: Vec<HostInfo> = parse(output.hostmap, "list-hostmap")?;
    let pending: Vec<HostInfo> = parse(output.pending, "list-pending-hostmap")?;
    let relays: Relays =
        serde_json::from_slice(output.relays).context("unexpected print-relays output")?;

    let names: BTreeMap<&str, &str> = topology
        .hosts
        .iter()
        .map(|(name, address)| (address.as_str(), name.as_str()))
        .collect();
    let role = |list: &[String], address: &str| {
        names
            .get(address)
            .is_some_and(|name| list.iter().any(|n| n == name))
    };

    let mut peers: BTreeMap<String, Peer> = BTreeMap::new();
    for (info, state) in hostmap
        .into_iter()
        .map(|i| (i, PeerState::Connected))
        .chain(pending.into_iter().map(|i| (i, PeerState::Handshaking)))
    {
        let Some(address) = info.vpn_addrs.first().cloned() else {
            continue;
        };
        // An established tunnel wins over a rehandshake for the same host.
        if peers.contains_key(&address) {
            continue;
        }
        let details = info.cert.map(|c| c.details);
        peers.insert(
            address.clone(),
            Peer {
                name: names
                    .get(address.as_str())
                    .map(|n| (*n).to_owned())
                    .or_else(|| details.as_ref().and_then(|d| d.name.clone())),
                lighthouse: role(&topology.lighthouses, &address),
                relay: role(&topology.relays, &address),
                remote: Some(info.current_remote).filter(|r| !r.is_empty()),
                via: info.current_relays_to_me.unwrap_or_default(),
                cert_expires: details.and_then(|d| d.not_after),
                state,
                address,
            },
        );
    }
    for (name, address) in &topology.hosts {
        peers.entry(address.clone()).or_insert_with(|| Peer {
            name: Some(name.clone()),
            address: address.clone(),
            state: PeerState::NoTunnel,
            lighthouse: topology.lighthouses.contains(name),
            relay: topology.relays.contains(name),
            remote: None,
            via: Vec::new(),
            cert_expires: None,
        });
    }
    if let Some(own) = own_address {
        peers.remove(own);
    }

    let mut hosts: Vec<Peer> = peers.into_values().collect();
    hosts.sort_by(|a, b| {
        (a.name.is_none(), &a.name, &a.address).cmp(&(b.name.is_none(), &b.name, &b.address))
    });

    // Nebula lists each forwarded pair under both of its ends.
    let forwarding: BTreeSet<(String, String)> = relays
        .relays
        .unwrap_or_default()
        .into_iter()
        .flat_map(|relay| {
            let from = relay.nebula_addr;
            relay
                .relay_for_addrs
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.kind == "forwarding" && r.state == "established")
                .map(move |r| {
                    let (a, b) = (from.clone(), r.peer_addr);
                    if a <= b { (a, b) } else { (b, a) }
                })
        })
        .collect();

    Ok(Mesh {
        hosts,
        forwarding: forwarding
            .into_iter()
            .map(|(from, to)| Forward { from, to })
            .collect(),
    })
}

/// Nebula prints `null` rather than `[]` for an empty hostmap.
fn parse(json: &[u8], command: &str) -> Result<Vec<HostInfo>> {
    let parsed: Option<Vec<HostInfo>> =
        serde_json::from_slice(json).with_context(|| format!("unexpected {command} output"))?;
    Ok(parsed.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn topology() -> Topology {
        Topology {
            hosts: [
                ("lh", "10.0.0.1"),
                ("relay", "10.0.0.2"),
                ("nas", "10.0.0.3"),
                ("tv", "10.0.0.4"),
                ("mac", "10.0.0.20"),
                ("laptop", "10.0.0.21"),
            ]
            .into_iter()
            .map(|(n, a)| (n.to_owned(), a.to_owned()))
            .collect(),
            lighthouses: vec!["lh".into()],
            relays: vec!["relay".into()],
        }
    }

    const HOSTMAP: &str = r#"[
        {"vpnAddrs":["10.0.0.1"],"localIndex":1,"remoteIndex":2,"remoteAddrs":["192.0.2.1:4242"],
         "cert":{"details":{"name":"lh","networks":["10.0.0.1/24"],"notAfter":"2027-01-15T11:02:29Z"},"version":2},
         "messageCounter":9,"currentRemote":"192.0.2.1:4242","currentRelaysToMe":[],"currentRelaysThroughMe":[]},
        {"vpnAddrs":["10.0.0.2"],"localIndex":3,"remoteIndex":4,"remoteAddrs":["192.0.2.2:4242"],
         "cert":{"details":{"name":"relay","notAfter":"2027-01-15T11:02:29Z"}},
         "messageCounter":9,"currentRemote":"192.0.2.2:4242","currentRelaysToMe":null,"currentRelaysThroughMe":["10.0.0.4"]},
        {"vpnAddrs":["10.0.0.4"],"localIndex":5,"remoteIndex":6,"remoteAddrs":null,
         "cert":{"details":{"name":"tv","notAfter":"2027-01-15T11:02:29Z"}},
         "messageCounter":9,"currentRemote":"","currentRelaysToMe":["10.0.0.2"],"currentRelaysThroughMe":null},
        {"vpnAddrs":["10.0.0.99"],"localIndex":7,"remoteIndex":8,"remoteAddrs":["192.0.2.99:1"],
         "cert":{"details":{"name":"stranger"}},
         "messageCounter":1,"currentRemote":"192.0.2.99:1","currentRelaysToMe":null,"currentRelaysThroughMe":null}
    ]"#;

    const PENDING: &str = r#"[
        {"vpnAddrs":["10.0.0.3"],"localIndex":9,"remoteIndex":0,"remoteAddrs":["198.51.100.3:5000"],
         "cert":null,"messageCounter":0,"currentRemote":"","currentRelaysToMe":null,"currentRelaysThroughMe":null},
        {"vpnAddrs":["10.0.0.4"],"localIndex":10,"remoteIndex":0,"remoteAddrs":null,
         "cert":null,"messageCounter":0,"currentRemote":"","currentRelaysToMe":null,"currentRelaysThroughMe":null}
    ]"#;

    fn mesh() -> Mesh {
        assemble(
            Output {
                hostmap: HOSTMAP.as_bytes(),
                pending: PENDING.as_bytes(),
                relays: br#"{"Relays":null}"#,
            },
            &topology(),
            Some("10.0.0.20"),
        )
        .unwrap()
    }

    fn peer(mesh: &Mesh, address: &str) -> Peer {
        mesh.hosts
            .iter()
            .find(|p| p.address == address)
            .unwrap()
            .clone()
    }

    #[test]
    fn tunnels_are_named_from_the_topology() {
        let mesh = mesh();
        let names: Vec<_> = mesh
            .hosts
            .iter()
            .map(|p| p.name.as_deref().unwrap())
            .collect();
        assert_eq!(names, ["laptop", "lh", "nas", "relay", "stranger", "tv"]);
        assert_eq!(
            peer(&mesh, "10.0.0.1"),
            Peer {
                name: Some("lh".into()),
                address: "10.0.0.1".into(),
                state: PeerState::Connected,
                lighthouse: true,
                relay: false,
                remote: Some("192.0.2.1:4242".into()),
                via: vec![],
                cert_expires: Some(Utc.with_ymd_and_hms(2027, 1, 15, 11, 2, 29).unwrap()),
            }
        );
    }

    #[test]
    fn a_relayed_tunnel_names_its_relay() {
        let tv = peer(&mesh(), "10.0.0.4");
        assert_eq!(
            tv.state,
            PeerState::Connected,
            "the tunnel wins over its rehandshake"
        );
        assert_eq!(tv.remote, None);
        assert_eq!(tv.via, ["10.0.0.2"]);
        assert!(peer(&mesh(), "10.0.0.2").relay);
    }

    #[test]
    fn handshakes_and_hosts_without_tunnels_are_listed() {
        let mesh = mesh();
        assert_eq!(peer(&mesh, "10.0.0.3").state, PeerState::Handshaking);
        assert_eq!(peer(&mesh, "10.0.0.21").state, PeerState::NoTunnel);
        assert!(
            mesh.hosts.iter().all(|p| p.address != "10.0.0.20"),
            "this Mac is left out"
        );
    }

    #[test]
    fn empty_hostmaps_are_null() {
        let mesh = assemble(
            Output {
                hostmap: b"null\n",
                pending: b"null\n",
                relays: b"{\"Relays\":null}\n",
            },
            &Topology::default(),
            None,
        )
        .unwrap();
        assert_eq!(mesh, Mesh::default());
    }

    #[test]
    fn forwarded_pairs_are_listed_once() {
        let relays = br#"{"Relays":[
            {"NebulaAddr":"10.0.0.3","RelayForAddrs":[{"Error":null,"Type":"forwarding","State":"established","PeerAddr":"10.0.0.4","LocalIndex":1,"RemoteIndex":2,"RelayedThrough":null}]},
            {"NebulaAddr":"10.0.0.4","RelayForAddrs":[{"Error":null,"Type":"forwarding","State":"established","PeerAddr":"10.0.0.3","LocalIndex":3,"RemoteIndex":4,"RelayedThrough":null},
                                                      {"Error":null,"Type":"terminal","State":"established","PeerAddr":"10.0.0.9","LocalIndex":5,"RemoteIndex":6,"RelayedThrough":null}]},
            {"NebulaAddr":"10.0.0.5","RelayForAddrs":[{"Error":{},"Type":"forwarding","State":"requested","PeerAddr":"10.0.0.6","LocalIndex":7,"RemoteIndex":0,"RelayedThrough":null}]}
        ]}"#;
        let mesh = assemble(
            Output {
                hostmap: b"[]",
                pending: b"[]",
                relays,
            },
            &Topology::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            mesh.forwarding,
            [Forward {
                from: "10.0.0.3".into(),
                to: "10.0.0.4".into()
            }]
        );
    }
    /// Output captured from Nebula 1.11.2 with one lighthouse tunnel.
    #[test]
    fn nebulas_own_output_parses() {
        let hostmap = r#"[{"vpnAddrs":["10.42.0.1"],"localIndex":2763167793,"remoteIndex":353554085,"remoteAddrs":["127.0.0.1:14242"],"cert":{"curve":"CURVE25519","details":{"groups":null,"isCa":false,"issuer":"d5830a145199a23fb36bea19b628c338a647d4271262b72beac964f3d6e1423f","name":"lh","networks":["10.42.0.1/24"],"notAfter":"2027-10-24T08:51:53+01:00","notBefore":"2026-10-24T08:51:54+01:00","unsafeNetworks":null},"fingerprint":"8dd0b8e268c2716362bc301e00f7e740083ad94d4c0455f989a22fcd5dd13fbd","publicKey":"dd4daaff56490379f37aeae58ed319c36e9222280d786ae06710a252e480be09","signature":"d006f3fa8427e7405e57b3dbdd7383f20b7a0e38663d71ebe2b2bcee323617b2031028717b947938e8579e35bda5cab0052c49bddb28afb839b6549da002d80d","version":2},"messageCounter":6,"currentRemote":"127.0.0.1:14242","currentRelaysToMe":[],"currentRelaysThroughMe":[]}]"#;
        let mesh = assemble(
            Output {
                hostmap: hostmap.as_bytes(),
                pending: b"[]\n",
                relays: b"{\"Relays\":null}\n",
            },
            &Topology::default(),
            Some("10.42.0.20"),
        )
        .unwrap();
        assert_eq!(
            mesh.hosts,
            [Peer {
                name: Some("lh".into()),
                address: "10.42.0.1".into(),
                state: PeerState::Connected,
                lighthouse: false,
                relay: false,
                remote: Some("127.0.0.1:14242".into()),
                via: vec![],
                cert_expires: Some(Utc.with_ymd_and_hms(2027, 10, 24, 7, 51, 53).unwrap()),
            }]
        );
    }
}
