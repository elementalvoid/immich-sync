# Hand checks: sync only on known networks (SPEC R8.8)

The gate itself is covered by `crates/sync-core/tests/it/network.rs`: the pure
check's table and a fake backend for startup, an OS change event, the fallback
poll, an OS that errors, leaving and returning to a listed network with the
queue kept, and an upload error that re-checks. What no CI runner can prove is
what each operating system answers, so each backend needs one run on real
hardware. Record the date, the OS version, and the observed name here when a
release ships a backend change.

## All platforms

1. With the list off, an upload starts on any network (the old behaviour).
2. Turn **Sync only on known networks** on and add the current network name
   (copied from **Settings › Sync › Current network**). The client resumes.
3. Switch to a network that is not on the list. Within 5 seconds the dashboard
   reads **Paused — not on a known network**, the tray tooltip says the same,
   and no upload request is sent (watch **Diagnostics** or the server log).
4. Put a file in a source folder while paused. It stays pending, no row turns
   into a failure, and returning to the listed network uploads it without a
   click.
5. Turn the list off. Uploads resume on the unlisted network.

## Windows

- **Name.** `NetworkListManager::GetNetworks(NLM_ENUM_NETWORK_CONNECTED)`
  reports the connected profile, whose name is the SSID on Wi-Fi.
- Check a domain-joined machine: the profile name is the domain profile, not
  the SSID, when the Wi-Fi is a Work network. Confirm which name the list has to
  hold.
- **Wired.** `GetAdaptersAddresses` with a gateway must classify the Ethernet
  adapter as wired, so an Ethernet-only machine syncs whatever the list says.
- **Events.** Disconnecting the Wi-Fi should pause within a second, not 30; a
  reconnect should resume. The sink is one COM advice on
  `INetworkListManagerEvents::ConnectivityChanged`.
- Check what an admin-owned machine without permission to query NLM reports: it
  must read as unknown (paused), never as allowed.

## macOS

- **Name.** `CNCopyCurrentNetworkInfo` returns the SSID only with Location
  permission. Turning the list on must show the prompt once; refusing it must
  leave **Current network: unknown — Location permission is not granted**, and
  the list must hold uploads.
- Check that the prompt reappears after an update, as the other TCC grants do
  (ADR 0008), and that allowing it then names the network.
- **Wired.** With Wi-Fi off and Ethernet plugged in, the client must report
  wired and sync. With Wi-Fi on and joined to an unlisted network, it must
  pause.
- **Events.** System Configuration's dynamic store should fire on an SSID
  change, a link drop, and a wake from sleep; confirm a change is noticed
  without waiting for the 30-second poll. Note that a hidden network's SSID may
  be absent even with the permission; that reads as unknown.
- Check an Ethernet-only Mac, and a Mac whose Wi-Fi is off with Ethernet plugged
  in: both must sync. A Wi-Fi-only Mac with Wi-Fi off has no link at all, so it
  must read unknown and hold, not wired.

## Linux

- **Name.** NetworkManager's `PrimaryConnection` carries the default route;
  `Type` `802-11-wireless` reads the access point's `Ssid`, falling back to the
  connection `Id` when there is no access point.
- Test on GNOME (NetworkManager running), with a VPN active (the primary
  connection must be the underlying link, not the VPN), and with a hidden
  network.
- **No NetworkManager.** On a host using systemd-networkd or plain `iwd`, the
  page must read **Current network: unknown — NetworkManager not found** and the
  list must hold uploads.
- **Wired.** An Ethernet-only host must report wired and sync with the list on.
- **Events.** NetworkManager's `StateChanged` and the D-Bus
  `PropertiesChanged` signal should pause a switch away from the listed network
  within a second.
- Watch the process while it runs with the list on for a day: the event thread
  must not spin (it blocks in `Connection::process(1s)`), and the 30-second poll
  must keep working after a suspend and resume.
