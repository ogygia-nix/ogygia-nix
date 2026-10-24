//! Whether this Mac's Nebula config follows the fleet or is owned on the Mac.
//!
//! Following the fleet, Nebula runs exactly the `settings` the repository
//! evaluates to. Managed locally, the parts of it in [`LocalConfig`] come
//! from the Mac instead, as a whole: a value the local config leaves out is
//! Nebula's default, never the fleet's, so turning local management on
//! cannot keep half of a fleet setting. The certificate, the CA and this
//! Mac's lighthouse and relay roles always come from the fleet, as only the
//! fleet can sign for them. The local config is kept while following the
//! fleet, so switching back restores it.

use std::net::IpAddr;

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::mesh::Topology;

/// What the daemon evaluates from the fleet repository, kept so a change to
/// the local config can be applied without fetching the repository again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fleet {
    /// The device's whole Nebula config, but for its key.
    pub settings: Value,
    pub topology: Topology,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigMode {
    #[default]
    Fleet,
    Local,
}

/// The parts of the Nebula config a Mac may own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalConfig {
    #[serde(default)]
    pub lighthouses: Vec<Lighthouse>,
    #[serde(default)]
    pub use_relays: bool,
    /// The mesh addresses of the relays to reach peers through.
    #[serde(default)]
    pub relays: Vec<String>,
    /// The UDP port to listen on; 0 picks one at random.
    #[serde(default)]
    pub listen_port: u16,
    #[serde(default)]
    pub inbound: Vec<FirewallRule>,
    #[serde(default)]
    pub outbound: Vec<FirewallRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lighthouse {
    /// The lighthouse's mesh address.
    pub address: String,
    /// Where to reach it outside the mesh, as `host:port`.
    #[serde(default)]
    pub endpoints: Vec<String>,
}

/// One Nebula firewall rule, with the fields Nebula reads. It reads every
/// spelling of a rule Nebula accepts, and writes one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawFirewallRule")]
pub struct FirewallRule {
    /// A port, a range like `200-300`, `fragment` or `any`. Nebula ignores
    /// it for ICMP.
    pub port: String,
    pub proto: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_cidr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_sha: Option<String>,
}

/// A rule as Nebula reads it. `code` is an older name for `port`; Nebula
/// takes either but not both, and needs neither for ICMP. `group` may be a
/// list of one, and `groups` a single group.
#[derive(Deserialize)]
struct RawFirewallRule {
    #[serde(default, deserialize_with = "string_or_number")]
    port: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    code: Option<String>,
    proto: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    group: Option<OneOrMany>,
    #[serde(default)]
    groups: Option<OneOrMany>,
    #[serde(default)]
    cidr: Option<String>,
    #[serde(default)]
    local_cidr: Option<String>,
    #[serde(default)]
    ca_name: Option<String>,
    #[serde(default)]
    ca_sha: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl TryFrom<RawFirewallRule> for FirewallRule {
    type Error = String;

    fn try_from(raw: RawFirewallRule) -> Result<Self, String> {
        let port = match (raw.port, raw.code) {
            (Some(_), Some(_)) => return Err("a rule has both a port and a code".into()),
            (Some(port), None) | (None, Some(port)) => port,
            (None, None) if raw.proto == "icmp" => "any".into(),
            (None, None) => return Err("a rule has no port".into()),
        };
        let group = match raw.group {
            None => None,
            Some(OneOrMany::One(group)) => Some(group),
            Some(OneOrMany::Many(groups)) => match <[String; 1]>::try_from(groups) {
                Ok([group]) => Some(group),
                Err(_) => return Err("a rule's group is a list of more than one".into()),
            },
        };
        let groups = raw.groups.map(|groups| match groups {
            OneOrMany::One(group) => vec![group],
            OneOrMany::Many(groups) => groups,
        });
        Ok(Self {
            port,
            proto: raw.proto,
            host: raw.host,
            group,
            groups,
            cidr: raw.cidr,
            local_cidr: raw.local_cidr,
            ca_name: raw.ca_name,
            ca_sha: raw.ca_sha,
        })
    }
}

/// Nebula writes a port as either a number or a string.
fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected a port, got {other}"
        ))),
    }
}

impl LocalConfig {
    /// The fleet's values for everything a local config owns, so the app can
    /// show them and start a local config from them.
    pub fn from_settings(settings: &Value) -> Self {
        let strings = |v: Option<&Value>| -> Vec<String> {
            v.and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let rules = |direction: &str| -> Vec<FirewallRule> {
            settings
                .pointer(&format!("/firewall/{direction}"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|r| serde_json::from_value(r.clone()).ok())
                        .collect()
                })
                .unwrap_or_default()
        };
        let static_hosts = settings.get("static_host_map");
        Self {
            lighthouses: strings(settings.pointer("/lighthouse/hosts"))
                .into_iter()
                .map(|address| Lighthouse {
                    endpoints: strings(static_hosts.and_then(|m| m.get(&address))),
                    address,
                })
                .collect(),
            use_relays: settings
                .pointer("/relay/use_relays")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            relays: strings(settings.pointer("/relay/relays")),
            listen_port: settings
                .pointer("/listen/port")
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(0),
            inbound: rules("inbound"),
            outbound: rules("outbound"),
        }
    }

    /// Replace everything this config owns in the fleet's `settings`.
    pub fn apply(&self, settings: &mut Value) -> Result<()> {
        let Some(root) = settings.as_object_mut() else {
            bail!("the evaluated settings are not an object");
        };
        let static_host_map: Map<String, Value> = self
            .lighthouses
            .iter()
            .filter(|l| !l.endpoints.is_empty())
            .map(|l| (l.address.clone(), json!(l.endpoints)))
            .collect();
        root.insert("static_host_map".into(), Value::Object(static_host_map));

        let section = |root: &mut Map<String, Value>, name: &str| -> Result<Map<String, Value>> {
            match root.remove(name) {
                None => Ok(Map::new()),
                Some(Value::Object(m)) => Ok(m),
                Some(_) => bail!("the evaluated settings' `{name}` is not an object"),
            }
        };
        let mut lighthouse = section(root, "lighthouse")?;
        lighthouse.insert(
            "hosts".into(),
            json!(
                self.lighthouses
                    .iter()
                    .map(|l| &l.address)
                    .collect::<Vec<_>>()
            ),
        );
        root.insert("lighthouse".into(), Value::Object(lighthouse));

        let mut relay = section(root, "relay")?;
        relay.insert("use_relays".into(), json!(self.use_relays));
        relay.insert("relays".into(), json!(self.relays));
        root.insert("relay".into(), Value::Object(relay));

        let mut listen = section(root, "listen")?;
        listen.insert("port".into(), json!(self.listen_port));
        root.insert("listen".into(), Value::Object(listen));

        let mut firewall = section(root, "firewall")?;
        firewall.insert("inbound".into(), serde_json::to_value(&self.inbound)?);
        firewall.insert("outbound".into(), serde_json::to_value(&self.outbound)?);
        root.insert("firewall".into(), Value::Object(firewall));
        Ok(())
    }

    /// Refuse anything Nebula would misread or that could leave the config
    /// it is written into.
    pub fn validate(&self) -> Result<()> {
        for lighthouse in &self.lighthouses {
            address(&lighthouse.address, "lighthouse")?;
            for endpoint in &lighthouse.endpoints {
                validate_endpoint(endpoint)?;
            }
        }
        for relay in &self.relays {
            address(relay, "relay")?;
        }
        for rule in self.inbound.iter().chain(&self.outbound) {
            rule.validate()?;
        }
        Ok(())
    }
}

fn address(address: &str, what: &str) -> Result<IpAddr> {
    address
        .parse()
        .map_err(|_| anyhow::anyhow!("{address:?} is not a {what} address"))
}

/// `host:port`, where the host is a name or an address (IPv6 in brackets).
fn validate_endpoint(endpoint: &str) -> Result<()> {
    let bad = || anyhow::anyhow!("{endpoint:?} is not a host:port endpoint");
    let (host, port) = endpoint.rsplit_once(':').ok_or_else(bad)?;
    port.parse::<u16>().map_err(|_| bad())?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.');
    if host.is_empty() || !(host.parse::<IpAddr>().is_ok() || host.chars().all(name)) {
        return Err(bad());
    }
    Ok(())
}

impl FirewallRule {
    fn validate(&self) -> Result<()> {
        if !matches!(self.proto.as_str(), "any" | "tcp" | "udp" | "icmp") {
            bail!(
                "{:?} is not a firewall protocol: use any, tcp, udp or icmp",
                self.proto
            );
        }
        let port = |p: &str| p.parse::<u16>().is_ok();
        let valid_port = matches!(self.port.as_str(), "any" | "fragment")
            || port(&self.port)
            || self
                .port
                .split_once('-')
                .is_some_and(|(lo, hi)| port(lo) && port(hi));
        if !valid_port {
            bail!(
                "{:?} is not a firewall port: use a port, a range or any",
                self.port
            );
        }
        for (cidr, what) in [(&self.cidr, "CIDR"), (&self.local_cidr, "local CIDR")] {
            if let Some(cidr) = cidr
                && cidr != "any"
                && !is_cidr(cidr)
            {
                bail!("{cidr:?} is not a firewall {what}: use an address/length or any");
            }
        }
        // Nebula refuses a rule that matches no one.
        if self.host.is_none()
            && self.group.is_none()
            && self.groups.as_ref().is_none_or(Vec::is_empty)
            && self.cidr.is_none()
            && self.local_cidr.is_none()
            && self.ca_name.is_none()
            && self.ca_sha.is_none()
        {
            bail!("each firewall rule needs a host, group, CIDR or CA to match");
        }
        Ok(())
    }
}

/// An address and prefix length, as Go's `netip.ParsePrefix`, which Nebula
/// uses, reads one: no zone, and a length in range without leading zeros.
fn is_cidr(cidr: &str) -> bool {
    let Some((address, length)) = cidr.split_once('/') else {
        return false;
    };
    let max = match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => 32,
        Ok(IpAddr::V6(_)) => 128,
        Err(_) => return false,
    };
    length
        .parse::<u8>()
        .is_ok_and(|l| l <= max && l.to_string() == length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet() -> Value {
        json!({
            "pki": { "ca": "CA", "cert": "CERT" },
            "static_host_map": { "10.0.0.1": ["lh.example.com:4242"] },
            "lighthouse": { "am_lighthouse": false, "hosts": ["10.0.0.1"] },
            "relay": { "am_relay": false, "use_relays": true, "relays": ["10.0.0.2"] },
            "listen": { "host": "[::]", "port": 0 },
            "firewall": {
                "inbound": [{ "port": 22, "proto": "tcp", "groups": ["ssh"] }],
                "outbound": [{ "port": "any", "proto": "any", "host": "any" }],
            },
        })
    }

    #[test]
    fn the_fleet_values_are_read() {
        assert_eq!(
            LocalConfig::from_settings(&fleet()),
            LocalConfig {
                lighthouses: vec![Lighthouse {
                    address: "10.0.0.1".into(),
                    endpoints: vec!["lh.example.com:4242".into()],
                }],
                use_relays: true,
                relays: vec!["10.0.0.2".into()],
                listen_port: 0,
                inbound: vec![FirewallRule {
                    port: "22".into(),
                    proto: "tcp".into(),
                    groups: Some(vec!["ssh".into()]),
                    ..FirewallRule::default()
                }],
                outbound: vec![FirewallRule {
                    port: "any".into(),
                    proto: "any".into(),
                    host: Some("any".into()),
                    ..FirewallRule::default()
                }],
            }
        );
    }

    #[test]
    fn applying_the_fleet_values_changes_only_their_spelling() {
        let mut settings = fleet();
        LocalConfig::from_settings(&fleet())
            .apply(&mut settings)
            .unwrap();
        let mut expected = fleet();
        expected["firewall"]["inbound"][0]["port"] = json!("22");
        assert_eq!(settings, expected);
    }

    #[test]
    fn a_local_config_replaces_what_it_owns_and_keeps_the_rest() {
        let local = LocalConfig {
            lighthouses: vec![Lighthouse {
                address: "10.0.0.9".into(),
                endpoints: vec![],
            }],
            listen_port: 4242,
            ..LocalConfig::default()
        };
        let mut settings = fleet();
        local.apply(&mut settings).unwrap();
        assert_eq!(settings["pki"], fleet()["pki"]);
        assert_eq!(settings["lighthouse"]["am_lighthouse"], json!(false));
        assert_eq!(settings["lighthouse"]["hosts"], json!(["10.0.0.9"]));
        // Left out locally means Nebula's default, not the fleet's value.
        assert_eq!(settings["static_host_map"], json!({}));
        assert_eq!(settings["relay"]["use_relays"], json!(false));
        assert_eq!(settings["relay"]["relays"], json!([]));
        assert_eq!(settings["listen"], json!({ "host": "[::]", "port": 4242 }));
        assert_eq!(
            settings["firewall"],
            json!({ "inbound": [], "outbound": [] })
        );
    }

    #[test]
    fn the_fleet_config_is_valid() {
        LocalConfig::from_settings(&fleet()).validate().unwrap();
    }

    #[test]
    fn bad_addresses_and_endpoints_are_refused() {
        for lighthouse in [
            Lighthouse {
                address: "lh".into(),
                endpoints: vec![],
            },
            Lighthouse {
                address: "10.0.0.1".into(),
                endpoints: vec!["lh.example.com".into()],
            },
            Lighthouse {
                address: "10.0.0.1".into(),
                endpoints: vec!["lh example:1".into()],
            },
            Lighthouse {
                address: "10.0.0.1".into(),
                endpoints: vec![":4242".into()],
            },
        ] {
            let config = LocalConfig {
                lighthouses: vec![lighthouse.clone()],
                ..LocalConfig::default()
            };
            assert!(config.validate().is_err(), "{lighthouse:?}");
        }
        let v6 = LocalConfig {
            lighthouses: vec![Lighthouse {
                address: "10.0.0.1".into(),
                endpoints: vec!["[2001:db8::1]:4242".into(), "192.0.2.1:4242".into()],
            }],
            ..LocalConfig::default()
        };
        v6.validate().unwrap();
    }

    #[test]
    fn rules_must_match_someone_on_a_real_port() {
        let rule = |port: &str, proto: &str, host: Option<&str>| FirewallRule {
            port: port.into(),
            proto: proto.into(),
            host: host.map(str::to_owned),
            ..FirewallRule::default()
        };
        for bad in [
            rule("22", "tcp", None),
            rule("ssh", "tcp", Some("any")),
            rule("22", "sctp", Some("any")),
            rule("1-x", "tcp", Some("any")),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        rule("200-300", "udp", Some("any")).validate().unwrap();
        rule("fragment", "any", Some("any")).validate().unwrap();
    }

    #[test]
    fn cidrs_must_parse_as_nebula_parses_them() {
        let rule = |cidr: &str, local_cidr: Option<&str>| FirewallRule {
            port: "any".into(),
            proto: "any".into(),
            cidr: Some(cidr.into()),
            local_cidr: local_cidr.map(str::to_owned),
            ..FirewallRule::default()
        };
        for bad in [
            "10.42.0.0/99",
            "10.42.0.0",
            "10.42.0.0/024",
            "10.42.0/24",
            "lighthouse/24",
            "fe80::1%en0/64",
            "2001:db8::/129",
        ] {
            assert!(rule(bad, None).validate().is_err(), "{bad}");
            assert!(rule("any", Some(bad)).validate().is_err(), "local {bad}");
        }
        for good in [
            "10.42.0.0/16",
            "10.42.0.1/32",
            "0.0.0.0/0",
            "2001:db8::/32",
            "any",
        ] {
            rule(good, Some(good)).validate().unwrap();
        }
    }

    #[test]
    fn a_local_cidr_alone_matches_someone() {
        FirewallRule {
            port: "any".into(),
            proto: "any".into(),
            local_cidr: Some("10.42.0.0/16".into()),
            ..FirewallRule::default()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn every_spelling_nebula_accepts_is_read() {
        let read = |rule: Value| -> FirewallRule { serde_json::from_value(rule).unwrap() };
        let icmp = read(json!({ "proto": "icmp", "host": "any" }));
        assert_eq!(icmp.port, "any");
        icmp.validate().unwrap();
        assert_eq!(
            read(json!({ "proto": "icmp", "code": 8, "host": "any" })).port,
            "8"
        );
        assert_eq!(
            read(json!({ "proto": "tcp", "code": "22", "host": "any" })).port,
            "22"
        );
        assert_eq!(
            read(json!({ "port": 22, "proto": "tcp", "groups": "ssh" })).groups,
            Some(vec!["ssh".into()])
        );
        assert_eq!(
            read(json!({ "port": 22, "proto": "tcp", "group": ["ssh"] })).group,
            Some("ssh".into())
        );
        assert_eq!(
            read(json!({ "port": 22, "proto": "tcp", "groups": null, "host": "any" })).groups,
            None
        );
    }

    #[test]
    fn spellings_nebula_refuses_are_not_read() {
        for rule in [
            json!({ "proto": "tcp", "host": "any" }),
            json!({ "port": 22, "code": 22, "proto": "tcp", "host": "any" }),
            json!({ "port": 22, "proto": "tcp", "group": ["a", "b"] }),
        ] {
            assert!(
                serde_json::from_value::<FirewallRule>(rule.clone()).is_err(),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_portless_icmp_rule_survives_copying_the_fleet() {
        let mut settings = fleet();
        settings["firewall"]["inbound"] = json!([{ "proto": "icmp", "host": "any" }]);
        let local = LocalConfig::from_settings(&settings);
        assert_eq!(
            local.inbound,
            vec![FirewallRule {
                port: "any".into(),
                proto: "icmp".into(),
                host: Some("any".into()),
                ..FirewallRule::default()
            }]
        );
        local.validate().unwrap();
    }
}
