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

/// Read an interface's MTU straight from sysfs.
///
/// `/sys/class/net/<name>/mtu` rather than a netlink socket or an `ioctl`:
/// the value is a single integer the kernel already publishes as a plain
/// text file, so reading it costs nothing beyond `std::fs` and doesn't pull
/// in a netlink dependency (or an ioctl + raw socket dance) for one number
/// this project only ever needs once per sender open. Linux-only, same as
/// the rest of this module — the project is ALSA-only, so there is nowhere
/// else this runs.
///
/// Returns `None` if the file doesn't exist (interface renamed or gone since
/// it was pinned — same "fall back, don't fail to start" contract as
/// [`resolve`]) or doesn't parse as a plain integer.
pub fn mtu_of(name: &str) -> Option<u32> {
    // `name` is config that can be copied between machines or edited by
    // hand, and it becomes a path component below. Refuse anything that
    // could walk it out of /sys/class/net (a `/` or `..` segment) or that
    // would corrupt the path outright (an embedded NUL) rather than let
    // `std::fs::read_to_string` chase it wherever it leads.
    if name.is_empty() || name.contains('/') || name.contains('\0') || name.contains("..") {
        return None;
    }
    let raw = std::fs::read_to_string(format!("/sys/class/net/{name}/mtu")).ok()?;
    raw.trim().parse().ok()
}

/// Interfaces worth offering to the operator: IPv4, not loopback, not
/// link-local (169.254/16 self-assigned addresses aren't useful to bind
/// audio to).
pub fn list_interfaces() -> Vec<NetInterface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| {
            usable_ipv4(&iface).map(|ip| NetInterface {
                name: iface.name,
                addr: ip.to_string(),
            })
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
    ifaces
        .iter()
        .find(|i| i.name == name)
        .and_then(usable_ipv4)
        .map(IpAddr::V4)
}

/// First non-loopback, non-link-local IPv4 interface in OS enumeration
/// order. Used when nothing is pinned — same fallback behaviour as before
/// this feature existed, just moved out of `main.rs`.
pub fn first_non_loopback_ipv4() -> Option<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .find_map(usable_ipv4)
        .map(IpAddr::V4)
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

    /// The one assertion here that touches the real host: sysfs is readable
    /// and the value parses. `lo` is the only interface every Linux machine
    /// has, so it is the only safe subject.
    ///
    /// Deliberately not asserting the exact number. 65536 is the usual
    /// loopback default, but it is a kernel default rather than a guarantee
    /// and a container or a tuned host can set it otherwise — and this test
    /// exists to prove the read and the parse work, not to police somebody's
    /// loopback configuration. Pinning the exact value would fail for a
    /// reason that has nothing to do with the code under test.
    #[test]
    fn mtu_of_reads_a_real_interface() {
        let mtu = mtu_of("lo").expect("every Linux host has lo with an mtu in sysfs");
        assert!(
            mtu >= 1000,
            "lo reported an implausible mtu of {mtu} — the parse is probably wrong"
        );
    }

    #[test]
    fn mtu_of_missing_interface_returns_none() {
        assert_eq!(mtu_of("nonexistent0"), None);
    }

    /// `name` is config, not something this process chose, so it must not be
    /// trusted as a path component.
    ///
    /// The first case is the one that earns this test. `../net/lo` traverses
    /// straight back into the directory it started from — `/sys/class/net/..`
    /// is `/sys/class`, so `/sys/class/net/../net/lo/mtu` resolves to a real,
    /// readable, parseable file. Without the guard this returns `Some`, which
    /// is a traversal that *worked*.
    ///
    /// The rest are documentation rather than proof, and saying so matters:
    /// they come back `None` with or without the guard, because `/mtu` is
    /// appended to whatever is given and none of them lands on a directory
    /// that contains such a file. A test that passes for a reason unrelated
    /// to the code it names is worse than no test — it looks like coverage.
    #[test]
    fn mtu_of_rejects_path_escaping_names() {
        assert_eq!(
            mtu_of("../net/lo"),
            None,
            "a `..` segment walked back into /sys/class/net and read a real mtu"
        );
        assert_eq!(mtu_of("../../etc/passwd"), None);
        assert_eq!(mtu_of("eth0/../../etc/passwd"), None);
        assert_eq!(mtu_of("eth0/mtu"), None);
        assert_eq!(mtu_of("eth0\0"), None);
        assert_eq!(mtu_of(""), None);
    }
}
