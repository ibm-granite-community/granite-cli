//! Configured hosts: machines that run model servers, and the link from a
//! provider to the host it runs on.
//!
//! A host has an id, an address (a host name or an IP address) and a list of
//! devices, each named by a hardware profile id. The list may repeat an id,
//! for two identical cards, and may be empty, when the hardware is not known.
//! `local` is the machine granite-cli runs on: its address is `localhost`
//! and its devices follow detection.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::config::hardware::LOCAL_HARDWARE_ID;

/*-- public --*/

/// The id of the host granite-cli runs on.
pub(crate) const LOCAL_HOST_ID: &str = "local";

/// The address of the `local` host.
pub(crate) const LOCAL_ADDRESS: &str = "localhost";

/// What a provider's `host` field holds when it runs on no known machine: a
/// hosted service.
pub(crate) const UNSPECIFIED_HOST: &str = "unspecified";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HostConfig {
    /// Instance id: the file name this host is saved under.
    pub(crate) host_id: String,
    /// A host name or an IP address.
    pub(crate) address: String,
    /// One hardware profile id per device. Empty when the hardware is not
    /// known.
    #[serde(default)]
    pub(crate) hardware: Vec<String>,
}

impl HostConfig {
    /// The `local` host as it is written the first time it is needed: the
    /// address `localhost` and the `local` hardware profile.
    pub(crate) fn local() -> Self {
        Self {
            host_id: LOCAL_HOST_ID.to_string(),
            address: LOCAL_ADDRESS.to_string(),
            hardware: vec![LOCAL_HARDWARE_ID.to_string()],
        }
    }
}

/// The id `host setup` offers for a host at `address`: the first label of a
/// host name (`pgx` for `pgx.lan`), and for an IP address the first hardware
/// profile id, or `host` when it has none.
pub(crate) fn default_id(address: &str, first_hardware: Option<&str>) -> String {
    let is_ip = address.parse::<std::net::IpAddr>().is_ok()
        || address.starts_with('[') && address.ends_with(']');
    if is_ip {
        return first_hardware.unwrap_or("host").to_string();
    }
    address.split('.').next().unwrap_or(address).to_string()
}

/// `id`, or `id-2`, `id-3` and so on, whichever is first not taken by a
/// configured host and is not `local` or `unspecified`, which a provider's
/// link reads as no host.
pub(crate) fn free_id(id: &str, config: &Config) -> String {
    let taken = |candidate: &str| {
        candidate == LOCAL_HOST_ID
            || candidate == UNSPECIFIED_HOST
            || config.get_host(candidate).is_some()
    };
    if !taken(id) {
        return id.to_string();
    }
    (2..)
        .map(|n| format!("{id}-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded range has a free id")
}

/// Which host a provider runs on. Saved as a string: `local`, `unspecified`,
/// or a host id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub(crate) enum HostLink {
    /// The `local` host: the provider runs on the machine granite-cli runs
    /// on. A provider saved before providers had a link also reads as this,
    /// because fit was estimated against the local machine then. It may run
    /// elsewhere; `provider list` marks it as not set.
    #[default]
    Local,
    /// A configured host other than `local`.
    Host(String),
    /// No host: a hosted service, for which fit cannot be estimated.
    Unspecified,
}

impl HostLink {
    /// What a provider of `provider_type` is linked to unless the user
    /// chooses: "not specified" for a hosted one, `local` for any other. A
    /// generic provider, such as openai-compatible, often points at a server
    /// the user runs (vLLM, a llama.cpp server), so it gets `local`.
    pub(crate) fn default_for(provider_type: &str) -> Self {
        let hosted = crate::providers::PROVIDER_REGISTRY
            .get(provider_type)
            .is_some_and(|p| p.provider_type == crate::providers::ProviderType::Hosted);
        if hosted {
            Self::Unspecified
        } else {
            Self::Local
        }
    }

    /// The id of the host this names, if any.
    pub(crate) fn host_id(&self) -> Option<&str> {
        match self {
            Self::Local => Some(LOCAL_HOST_ID),
            Self::Host(id) => Some(id),
            Self::Unspecified => None,
        }
    }
}

impl From<String> for HostLink {
    fn from(value: String) -> Self {
        match value.as_str() {
            LOCAL_HOST_ID => Self::Local,
            UNSPECIFIED_HOST => Self::Unspecified,
            _ => Self::Host(value),
        }
    }
}

impl From<HostLink> for String {
    fn from(link: HostLink) -> Self {
        match link {
            HostLink::Local => LOCAL_HOST_ID.to_string(),
            HostLink::Host(id) => id,
            HostLink::Unspecified => UNSPECIFIED_HOST.to_string(),
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_round_trips_through_yaml() {
        let hosts = [
            HostConfig {
                host_id: "pgx".to_string(),
                address: "pgx.lan".to_string(),
                hardware: vec!["dgx-spark".to_string()],
            },
            HostConfig {
                host_id: "box".to_string(),
                address: "10.0.0.7".to_string(),
                hardware: vec!["rtx-4090".to_string(), "rtx-4090".to_string()],
            },
            HostConfig {
                host_id: "lab".to_string(),
                address: "lab.lan".to_string(),
                hardware: Vec::new(),
            },
        ];
        for host in hosts {
            let yaml = serde_yaml::to_string(&host).unwrap();
            assert_eq!(serde_yaml::from_str::<HostConfig>(&yaml).unwrap(), host);
        }
    }

    #[test]
    fn a_host_saves_one_line_per_device() {
        let host = HostConfig {
            host_id: "box".to_string(),
            address: "box.lan".to_string(),
            hardware: vec!["rtx-4090".to_string(), "rtx-4090".to_string()],
        };
        assert_eq!(
            serde_yaml::to_string(&host).unwrap(),
            "host_id: box\naddress: box.lan\nhardware:\n- rtx-4090\n- rtx-4090\n"
        );
    }

    #[test]
    fn local_is_localhost_with_the_local_profile() {
        assert_eq!(
            HostConfig::local(),
            HostConfig {
                host_id: "local".to_string(),
                address: "localhost".to_string(),
                hardware: vec!["local".to_string()],
            }
        );
    }

    #[test]
    fn the_default_id_is_the_first_label_or_for_an_ip_the_hardware() {
        assert_eq!(default_id("pgx.lan", Some("dgx-spark")), "pgx");
        assert_eq!(default_id("box", None), "box");
        assert_eq!(default_id("10.0.0.12", Some("dgx-spark")), "dgx-spark");
        assert_eq!(default_id("10.0.0.12", None), "host");
        assert_eq!(default_id("[fe80::1]", None), "host");
    }

    #[test]
    fn a_taken_id_gets_a_number() {
        let mut config = Config::default();
        assert_eq!(free_id("pgx", &config), "pgx");
        assert_eq!(free_id("local", &config), "local-2");
        assert_eq!(free_id("unspecified", &config), "unspecified-2");
        config.hosts.insert(
            "pgx".to_string(),
            HostConfig {
                host_id: "pgx".to_string(),
                address: "pgx.lan".to_string(),
                hardware: Vec::new(),
            },
        );
        assert_eq!(free_id("pgx", &config), "pgx-2");
    }

    #[test]
    fn each_link_round_trips_through_yaml() {
        for link in [
            HostLink::Local,
            HostLink::Unspecified,
            HostLink::Host("pgx".to_string()),
        ] {
            let yaml = serde_yaml::to_string(&link).unwrap();
            assert_eq!(serde_yaml::from_str::<HostLink>(&yaml).unwrap(), link);
        }
        assert_eq!(
            serde_yaml::to_string(&HostLink::Unspecified).unwrap(),
            "unspecified\n"
        );
    }

    #[test]
    fn a_hosted_provider_defaults_to_not_specified_and_any_other_to_local() {
        assert_eq!(HostLink::default_for("openrouter"), HostLink::Unspecified);
        assert_eq!(HostLink::default_for("ollama"), HostLink::Local);
        assert_eq!(HostLink::default_for("openai-compatible"), HostLink::Local);
    }
}
