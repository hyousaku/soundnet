//! Network interface enumeration and selection.
//!
//! `soundnet-engine` runs on multi-homed hosts (wired + wireless on the same
//! subnet), so "the" IP address of a machine is ambiguous. This module turns
//! an operator-chosen interface *name* (persisted in `Config::interface`,
//! since DHCP can reassign the IP but not rename the NIC) into the IPv4
//! address to actually advertise and send audio from, and lists the
//! candidates for the UI to offer.

use anyhow::{anyhow, Result};
use soundnet_protocol::NetInterface;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use crate::discovery;
use crate::routing;
use crate::state::EngineState;

/// Interfaces worth offering to the operator: IPv4, not loopback, not
/// link-local (169.254/16 self-assigned addresses aren't useful to bind
/// audio to).
pub fn list_interfaces() -> Vec<NetInterface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| {
            usable_ipv4(&iface).map(|ip| NetInterface { name: iface.name, addr: ip.to_string() })
        })
        .collect()
}

/// Resolve a pinned interface name to its current IPv4 address. `None` means
/// the name doesn't currently exist on this host (renamed, unplugged, or a
/// config copied from a different machine) — callers must fall back to
/// automatic selection rather than fail to start or wedge.
pub fn resolve(name: &str) -> Option<IpAddr> {
    resolve_in(name, &if_addrs::get_if_addrs().unwrap_or_default())
}

fn resolve_in(name: &str, ifaces: &[if_addrs::Interface]) -> Option<IpAddr> {
    ifaces.iter().find(|i| i.name == name).and_then(usable_ipv4).map(IpAddr::V4)
}

/// First non-loopback, non-link-local IPv4 interface in OS enumeration
/// order. Used when nothing is pinned — same fallback behaviour as before
/// this feature existed, just moved out of `main.rs`.
pub fn first_non_loopback_ipv4() -> Option<IpAddr> {
    if_addrs::get_if_addrs().unwrap_or_default().iter().find_map(usable_ipv4).map(IpAddr::V4)
}

fn usable_ipv4(iface: &if_addrs::Interface) -> Option<Ipv4Addr> {
    if iface.is_loopback() {
        return None;
    }
    match iface.ip() {
        IpAddr::V4(v4) if !v4.is_unspecified() && !v4.is_link_local() => Some(v4),
        _ => None,
    }
}

/// Apply a new interface selection at runtime: resolve it, update the
/// effective address, persist the choice, re-register mDNS under the new
/// address, and restart routes so senders pick up the new outgoing
/// interface.
///
/// Unlike the startup path (`main.rs`), a request to pin a name that doesn't
/// currently resolve is rejected outright rather than silently falling back
/// — the UI only ever offers names from `list_interfaces()`, so a failure
/// here means the interface disappeared between the browser loading its list
/// and the operator clicking it, which is worth surfacing rather than
/// papering over.
pub async fn set_selected(state: &Arc<EngineState>, name: Option<String>) -> Result<()> {
    let new_addr = match &name {
        Some(n) => resolve(n).ok_or_else(|| anyhow!("interface {n:?} not found on this host"))?,
        None => first_non_loopback_ipv4().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    };

    *state.identity.addr.write().unwrap() = new_addr;
    *state.selected_interface.write().await = name;

    // Persist before touching mDNS/routes: if either of those fails partway
    // through, the choice the operator made is still on disk for the next
    // restart to pick up.
    routing::persist(state).await;

    if let Err(err) = discovery::reregister(state, new_addr).await {
        tracing::warn!("mDNS re-register after interface change failed: {err:#}");
    }

    // Senders capture their outgoing address at spawn time (see
    // transport/sender.rs), so a route already running won't move to the new
    // interface on its own. Tear everything down and let
    // spawn_route_supervisor's periodic sweep bring it back up bound to the
    // new address — same machinery that already recovers a route after any
    // other kind of restart.
    routing::shutdown_all(state).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use if_addrs::{IfAddr, Ifv4Addr, Interface};

    fn fake_iface(name: &str, ip: Ipv4Addr) -> Interface {
        Interface {
            name: name.to_string(),
            addr: IfAddr::V4(Ifv4Addr {
                ip,
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                prefixlen: 24,
                broadcast: None,
            }),
            index: Some(1),
            #[cfg(windows)]
            adapter_name: String::new(),
        }
    }

    #[test]
    fn resolve_finds_matching_interface_by_name() {
        let ifaces = vec![
            fake_iface("lo", Ipv4Addr::new(127, 0, 0, 1)),
            fake_iface("eth0", Ipv4Addr::new(192, 168, 10, 135)),
            fake_iface("wlan0", Ipv4Addr::new(192, 168, 10, 129)),
        ];
        assert_eq!(
            resolve_in("eth0", &ifaces),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 10, 135)))
        );
    }

    /// A NIC named in a config that's stale (renamed, unplugged, or copied
    /// from the other deployed machine) must resolve to `None`, never panic
    /// — this is what lets `main.rs` fall back to automatic selection
    /// instead of failing to start.
    #[test]
    fn resolve_missing_or_renamed_interface_returns_none() {
        let ifaces = vec![fake_iface("eth0", Ipv4Addr::new(192, 168, 10, 135))];
        assert_eq!(resolve_in("eth1", &ifaces), None);
        assert_eq!(resolve_in("nonexistent0", &[]), None);
    }
}
