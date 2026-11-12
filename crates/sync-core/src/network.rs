//! The known-network gate (SPEC R8.8).
//!
//! The client pauses uploads unless it can prove the current network is one the
//! user listed. The platform answers "which network is this" behind
//! [`NetworkSource`]; the decision itself is pure, so every case runs without a
//! platform.

use std::fmt;

use tokio::sync::mpsc::Receiver;

/// What the OS reports about the current connection (R8.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentNetwork {
    /// A connection with no wireless name to match: Ethernet, or a machine
    /// whose Wi-Fi is off while a link still carries traffic. It always syncs.
    Wired,
    /// A wireless network, under the name the OS reports.
    Wireless(String),
}

/// Why the client cannot tell which network it is on (R8.8).
///
/// The reason reaches the Settings page, so it says what the user can fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkProblem(String);

impl NetworkProblem {
    /// A problem with the reason the user reads.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }

    /// The reason, without the surrounding sentence.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NetworkProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NetworkProblem {}

/// The user's known-network list (R8.8).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownNetworks {
    /// Whether the list filters uploads at all. Off by default, which is the
    /// behaviour every build had before the list existed.
    pub enabled: bool,
    /// The names the user typed.
    pub names: Vec<String>,
}

impl KnownNetworks {
    /// The names that can match, trimmed, so a stray space in the settings file
    /// does not pause a network the user listed.
    fn listed(&self) -> impl Iterator<Item = &str> {
        self.names
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
    }
}

/// What the gate decided for the current network (R8.8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkDecision {
    /// Uploads may run: the list is off, the connection is wired, or its name
    /// is listed.
    #[default]
    Allowed,
    /// The network has a name the user did not list, so uploads wait (R8.8).
    NotListed,
    /// The client cannot prove which network this is, so uploads wait (R8.8).
    Unknown,
}

/// The pure check (R8.8): the current network, or the reason it is unknown,
/// decides whether uploads may run under `known`.
///
/// It fails closed: only a wired connection, or a wireless name the user
/// listed, allows uploads while the list is on.
#[must_use]
pub fn decide(
    known: &KnownNetworks,
    current: Result<&CurrentNetwork, &NetworkProblem>,
) -> NetworkDecision {
    if !known.enabled {
        return NetworkDecision::Allowed;
    }
    match current {
        Ok(CurrentNetwork::Wired) => NetworkDecision::Allowed,
        // An empty name proves nothing, so it is unknown rather than an
        // unlisted network the user could act on.
        Ok(CurrentNetwork::Wireless(name)) if name.trim().is_empty() => NetworkDecision::Unknown,
        Ok(CurrentNetwork::Wireless(name)) if known.listed().any(|listed| listed == name) => {
            NetworkDecision::Allowed
        }
        Ok(CurrentNetwork::Wireless(_)) => NetworkDecision::NotListed,
        Err(_) => NetworkDecision::Unknown,
    }
}

/// One platform's answer to "which network is this" (R8.8).
///
/// The backends live in the shell; the engine only asks. Every call may block,
/// so the engine runs it off its async workers.
pub trait NetworkSource: Send + Sync {
    /// The current connection, or why the client cannot name it.
    fn current(&self) -> Result<CurrentNetwork, NetworkProblem>;

    /// A channel that fires when the OS reports a network change, where the
    /// platform has one. The 30-second fallback poll covers the rest (R8.8).
    fn changes(&self) -> Option<Receiver<()>> {
        None
    }
}

/// What the shell shows about the current network (R8.8).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkStatus {
    /// The wireless network's name, when the OS reported one.
    pub name: Option<String>,
    /// Whether the connection has no wireless name to match.
    pub wired: bool,
    /// Why the name is unknown, when it is.
    pub problem: Option<String>,
}

impl NetworkStatus {
    /// The status for one OS answer, or for the reason it failed.
    #[must_use]
    pub fn from_result(result: &Result<CurrentNetwork, NetworkProblem>) -> Self {
        match result {
            Ok(CurrentNetwork::Wired) => Self {
                wired: true,
                ..Self::default()
            },
            Ok(CurrentNetwork::Wireless(name)) if !name.trim().is_empty() => Self {
                name: Some(name.clone()),
                ..Self::default()
            },
            Ok(CurrentNetwork::Wireless(_)) => Self {
                problem: Some("the OS reported no network name".to_string()),
                ..Self::default()
            },
            Err(problem) => Self {
                problem: Some(problem.reason().to_string()),
                ..Self::default()
            },
        }
    }

    /// The sentence the Settings page shows beside the list (R8.8).
    #[must_use]
    pub fn line(&self) -> String {
        if self.wired {
            return "Current network: wired".to_string();
        }
        match (&self.name, &self.problem) {
            (Some(name), _) => format!("Current network: {name}"),
            (None, Some(problem)) => format!("Current network: unknown — {problem}"),
            (None, None) => "Current network: unknown".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CurrentNetwork, NetworkProblem, NetworkStatus};

    /// R8.8: the Settings line names the network when it can, and says why
    /// when it cannot.
    #[test]
    fn the_settings_line_names_the_network_or_the_reason() {
        assert_eq!(
            NetworkStatus::from_result(&Ok(CurrentNetwork::Wireless("Home".to_string()))).line(),
            "Current network: Home"
        );
        assert_eq!(
            NetworkStatus::from_result(&Ok(CurrentNetwork::Wired)).line(),
            "Current network: wired"
        );
        assert_eq!(
            NetworkStatus::from_result(&Err(NetworkProblem::new("NetworkManager not found")))
                .line(),
            "Current network: unknown — NetworkManager not found"
        );
        assert_eq!(NetworkStatus::default().line(), "Current network: unknown");
        assert_eq!(
            NetworkStatus::from_result(&Ok(CurrentNetwork::Wireless("  ".to_string()))).line(),
            "Current network: unknown — the OS reported no network name"
        );
    }
}
