//! The platform's current network name (SPEC R8.8).
//!
//! One backend per platform, behind [`NetworkSource`]. The backends only answer
//! "which network is this"; the decision, the holds and the fallback poll live
//! in `sync_core::network` and the engine, so nothing here decides anything.
//!
//! Each backend fails closed by returning a [`NetworkProblem`] rather than an
//! invented name: an OS that cannot be asked pauses the sync while the list is
//! on (R8.8).

use std::sync::Arc;

use sync_core::network::{CurrentNetwork, NetworkProblem, NetworkSource};

/// The backend for this platform (R8.8).
#[must_use]
pub fn platform_network() -> Arc<dyn NetworkSource> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::LinuxNetwork)
    }
    #[cfg(windows)]
    {
        Arc::new(windows_backend::WindowsNetwork)
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacNetwork)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Arc::new(Unavailable::new(
            "this platform has no network check".to_string(),
        ))
    }
}

/// Asks for the Location permission the macOS SSID needs (R8.8).
///
/// The Settings switch calls this on the UI thread when the user turns the
/// known-network list on. It is a no-op everywhere else, and a no-op on a
/// machine that already answered the prompt.
#[cfg(target_os = "macos")]
pub fn request_location_permission() {
    macos::request_location_permission();
}

/// Asks for the Location permission the macOS SSID needs (R8.8).
///
/// Every other platform has no permission to ask for, so this is a no-op.
#[cfg(not(target_os = "macos"))]
pub fn request_location_permission() {}

/// A backend that cannot answer on this machine, with the reason the Settings
/// page shows.
#[derive(Debug)]
pub struct Unavailable {
    reason: String,
}

impl Unavailable {
    /// A backend that always reports `reason` (R8.8).
    #[must_use]
    pub fn new(reason: String) -> Self {
        Self { reason }
    }
}

impl NetworkSource for Unavailable {
    fn current(&self) -> Result<CurrentNetwork, NetworkProblem> {
        Err(NetworkProblem::new(self.reason.clone()))
    }
}

/// NetworkManager's primary connection, the access point's SSID, and the
/// connection's id (R8.8).
#[cfg(target_os = "linux")]
mod linux {
    use std::time::Duration;

    use dbus::blocking::Connection;
    use dbus::message::MatchRule;
    use sync_core::network::{CurrentNetwork, NetworkProblem, NetworkSource};
    use tokio::sync::mpsc::Receiver;

    const MANAGER: &str = "org.freedesktop.NetworkManager";
    const MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
    const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
    const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    const TIMEOUT: Duration = Duration::from_secs(2);

    /// The NetworkManager backend.
    pub struct LinuxNetwork;

    /// One property, through `org.freedesktop.DBus.Properties.Get`.
    fn property<T>(
        connection: &Connection,
        path: &str,
        interface: &str,
        name: &str,
    ) -> Result<T, NetworkProblem>
    where
        T: for<'z> dbus::arg::Get<'z>,
    {
        let proxy = connection.with_proxy(MANAGER, path, TIMEOUT);
        let (value,): (dbus::arg::Variant<T>,) = proxy
            .method_call(PROPERTIES, "Get", (interface, name))
            .map_err(|error| {
                NetworkProblem::new(format!("NetworkManager did not answer: {error}"))
            })?;
        Ok(value.0)
    }

    impl NetworkSource for LinuxNetwork {
        fn current(&self) -> Result<CurrentNetwork, NetworkProblem> {
            // A connection per check: the blocking connection is not shared
            // between threads, and one check runs every 30 seconds.
            let connection = Connection::new_system().map_err(|error| {
                NetworkProblem::new(format!("NetworkManager not found: {error}"))
            })?;
            // The primary connection is the one carrying the default route, so
            // a VPN or a second interface cannot be mistaken for it.
            let primary: dbus::Path<'static> =
                property(&connection, MANAGER_PATH, MANAGER, "PrimaryConnection")?;
            let active = primary.to_string();
            if active == "/" {
                return Err(NetworkProblem::new("no active network connection"));
            }
            let kind: String = property(&connection, &active, ACTIVE, "Type")?;
            match kind.as_str() {
                "802-11-wireless" => {}
                // A link with no SSID to match: Ethernet, or a compound of it.
                // It always syncs (R8.8).
                "802-3-ethernet" | "802-3-wire" | "bond" | "bridge" | "vlan" => {
                    return Ok(CurrentNetwork::Wired);
                }
                // Mobile broadband, Bluetooth tethering, a VPN or a tunnel:
                // metered, or carried over a network this check cannot name, so
                // it must not be mistaken for a wired link.
                other => {
                    return Err(NetworkProblem::new(format!(
                        "the connection type {other} has no network name to match"
                    )));
                }
            }
            // The access point's SSID is the name the OS shows. A profile with
            // no access point (connecting, or a hidden network) falls back to
            // the connection id, which NetworkManager sets from the SSID.
            let ssid =
                property::<dbus::Path<'static>>(&connection, &active, ACTIVE, "SpecificObject")
                    .ok()
                    .map(|ap| ap.to_string())
                    .filter(|ap| ap != "/")
                    .and_then(|ap| property::<Vec<u8>>(&connection, &ap, ACCESS_POINT, "Ssid").ok())
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .filter(|name| !name.is_empty());
            let name = match ssid {
                Some(name) => name,
                None => property::<String>(&connection, &active, ACTIVE, "Id")?,
            };
            Ok(CurrentNetwork::Wireless(name))
        }

        fn changes(&self) -> Option<Receiver<()>> {
            // A second connection on its own thread: the blocking connection's
            // message processing must not run beside its calls (dbus crate).
            let (sender, receiver) = tokio::sync::mpsc::channel(4);
            std::thread::spawn(move || {
                let Ok(connection) = Connection::new_system() else {
                    return;
                };
                let state = sender.clone();
                let mut rule = MatchRule::new_signal(MANAGER, "StateChanged");
                rule.sender = Some(MANAGER.into());
                let added = connection.add_match(
                    rule,
                    move |_: (u32,), _: &Connection, _: &dbus::Message| {
                        let _ = state.try_send(());
                        true
                    },
                );
                if added.is_err() {
                    return;
                }
                let properties = sender.clone();
                // Every NetworkManager property change, from its own sender:
                // the manager's primary connection, a device's link state and
                // an access point's SSID all move with it, and no other
                // application's property changes wake the check.
                let mut rule =
                    MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
                rule.sender = Some(MANAGER.into());
                let added =
                    connection.add_match(rule, move |_: (), _: &Connection, _: &dbus::Message| {
                        let _ = properties.try_send(());
                        true
                    });
                if added.is_err() {
                    return;
                }
                while !sender.is_closed() {
                    if connection.process(Duration::from_secs(1)).is_err() {
                        break;
                    }
                }
            });
            Some(receiver)
        }
    }
}

/// The Network List Manager's connected profile name, and the adapter list's
/// media type (R8.8).
#[cfg(windows)]
mod windows_backend {
    use std::time::Duration;

    use sync_core::network::{CurrentNetwork, NetworkProblem, NetworkSource};
    use tokio::sync::mpsc::Receiver;
    use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, IF_TYPE_ETHERNET_CSMACD,
        IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::NetworkListManager::{
        INetworkListManager, INetworkListManagerEvents, INetworkListManagerEvents_Impl,
        NLM_CONNECTIVITY, NLM_ENUM_NETWORK_CONNECTED, NetworkListManager,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
        IConnectionPoint, IConnectionPointContainer,
    };
    use windows::core::{Interface, implement};

    /// The Windows backend.
    pub struct WindowsNetwork;

    impl NetworkSource for WindowsNetwork {
        fn current(&self) -> Result<CurrentNetwork, NetworkProblem> {
            match adapter_kind()? {
                Adapter::Wired => Ok(CurrentNetwork::Wired),
                Adapter::Wireless => wireless_name().map(CurrentNetwork::Wireless),
            }
        }

        fn changes(&self) -> Option<Receiver<()>> {
            let (sender, receiver) = tokio::sync::mpsc::channel(4);
            std::thread::spawn(move || watch(sender));
            Some(receiver)
        }
    }

    /// The media type of the adapter carrying a default route (R8.8).
    enum Adapter {
        Wired,
        Wireless,
    }

    /// Which adapter type is up with a gateway, preferring wired so an
    /// Ethernet connection always syncs (R8.8).
    fn adapter_kind() -> Result<Adapter, NetworkProblem> {
        let _com = Com::init();
        let mut size = 16 * 1024_u32;
        loop {
            let mut buffer = vec![0_u8; size as usize];
            // SAFETY: the buffer is `size` bytes and the layout of
            // `IP_ADAPTER_ADDRESSES_LH` is the C struct the API fills; the
            // pointer comes from a suitably sized, aligned `Vec<u8>`.
            let status = unsafe {
                GetAdaptersAddresses(
                    0, // AF_UNSPEC: every family.
                    GAA_FLAG_INCLUDE_GATEWAYS,
                    None,
                    Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                    &mut size,
                )
            };
            if status == ERROR_BUFFER_OVERFLOW.0 {
                continue;
            }
            if status != 0 {
                return Err(NetworkProblem::new(format!(
                    "the Windows adapter list failed ({status})"
                )));
            }
            let mut wired = false;
            let mut wireless = false;
            let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            while let Some(current) = unsafe { adapter.as_ref() } {
                let up = current.OperStatus == IfOperStatusUp;
                let routed = !current.FirstGatewayAddress.is_null();
                if up && routed {
                    if current.IfType == IF_TYPE_IEEE80211 {
                        wireless = true;
                    } else if current.IfType == IF_TYPE_ETHERNET_CSMACD {
                        wired = true;
                    }
                }
                adapter = current.Next;
            }
            if wired {
                return Ok(Adapter::Wired);
            }
            if wireless {
                return Ok(Adapter::Wireless);
            }
            return Err(NetworkProblem::new("no active network connection"));
        }
    }

    /// The connected profile's name, which is the SSID on Wi-Fi (R8.8).
    fn wireless_name() -> Result<String, NetworkProblem> {
        let _com = Com::init();
        // SAFETY: `NetworkListManager` is the registered coclass, and the
        // manager keeps its COM apartment for the duration of the call.
        unsafe {
            let manager: INetworkListManager =
                CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).map_err(|error| {
                    NetworkProblem::new(format!("the Network List Manager failed: {error}"))
                })?;
            let networks = manager
                .GetNetworks(NLM_ENUM_NETWORK_CONNECTED)
                .map_err(|error| {
                    NetworkProblem::new(format!("the connected networks failed: {error}"))
                })?;
            // The connected enum is a linked list read in pages; the first
            // connected profile is the network this machine is on.
            let mut page: [Option<windows::Win32::Networking::NetworkListManager::INetwork>; 8] =
                Default::default();
            loop {
                let mut fetched = 0;
                networks
                    .Next(&mut page, Some(&mut fetched))
                    .map_err(|error| {
                        NetworkProblem::new(format!("a network could not be read: {error}"))
                    })?;
                if fetched == 0 {
                    return Err(NetworkProblem::new("no connected network profile"));
                }
                for network in page.iter_mut().take(fetched as usize) {
                    let Some(network) = network.as_ref() else {
                        continue;
                    };
                    if network
                        .IsConnected()
                        .is_ok_and(|connected| connected.as_bool())
                    {
                        let name = network.GetName().map_err(|error| {
                            NetworkProblem::new(format!("the network name failed: {error}"))
                        })?;
                        return Ok(name.to_string());
                    }
                }
            }
        }
    }

    /// COM for the length of one call, abandoned when another initialisation
    /// already owns the thread.
    struct Com {
        initialized: bool,
    }

    impl Com {
        fn init() -> Self {
            // SAFETY: COM initialisation has no memory contract; an already
            // initialised thread answers with an error, which is not ours to
            // uninitialise.
            let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
            Self { initialized }
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.initialized {
                // SAFETY: this thread initialised COM above.
                unsafe { CoUninitialize() };
            }
        }
    }

    /// The connectivity sink the Network List Manager calls (R8.8).
    ///
    /// It only wakes the engine's check: the next check reads the real name.
    #[implement(INetworkListManagerEvents)]
    struct ConnectivitySink {
        wake: tokio::sync::mpsc::Sender<()>,
    }

    impl INetworkListManagerEvents_Impl for ConnectivitySink_Impl {
        fn ConnectivityChanged(&self, _: NLM_CONNECTIVITY) -> windows::core::Result<()> {
            let _ = self.wake.try_send(());
            Ok(())
        }
    }

    /// Keeps a COM subscription alive for as long as the channel is open.
    fn watch(wake: tokio::sync::mpsc::Sender<()>) {
        let _com = Com::init();
        let advised = unsafe { subscribe(&wake) };
        let Some((_manager, _point, _cookie)) = advised else {
            return;
        };
        while !wake.is_closed() {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// The manager, the sink, and the advice cookie, kept alive together.
    type Advised = (INetworkListManager, IConnectionPoint, u32);

    unsafe fn subscribe(wake: &tokio::sync::mpsc::Sender<()>) -> Option<Advised> {
        unsafe {
            let manager: INetworkListManager =
                CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).ok()?;
            let container: IConnectionPointContainer = manager.cast().ok()?;
            let point: IConnectionPoint = container
                .FindConnectionPoint(&INetworkListManagerEvents::IID)
                .ok()?;
            let sink: INetworkListManagerEvents = ConnectivitySink { wake: wake.clone() }.into();
            let cookie = point.Advise(&sink).ok()?;
            Some((manager, point, cookie))
        }
    }
}

/// The Wi-Fi SSID through CoreWLAN and CaptiveNetwork, and Location Services
/// for its permission (R8.8).
// Two bindings macOS itself has deprecated, kept on purpose: the class
// `authorizationStatus` is the only form that can be read off the main thread,
// and CaptiveNetwork's `CNCopyCurrentNetworkInfo` is the API R8.8 names. The
// newer instance method would need a run-loop thread for the check, which the
// engine's blocking pool is not.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
mod macos {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::time::Duration;

    use dispatch2::{DispatchQueue, GlobalQueueIdentifier};
    use objc2_core_foundation::{CFArray, CFRetained, CFString};
    use objc2_core_location::{CLAuthorizationStatus, CLLocationManager};
    use objc2_core_wlan::{CWInterface, CWWiFiClient};
    use objc2_system_configuration::{CNCopyCurrentNetworkInfo, SCDynamicStore};
    use sync_core::network::{CurrentNetwork, NetworkProblem, NetworkSource};
    use tokio::sync::mpsc::Receiver;

    /// The macOS backend.
    pub struct MacNetwork;

    /// The authorization statuses that let `CNCopyCurrentNetworkInfo` answer.
    fn location_granted(status: CLAuthorizationStatus) -> bool {
        status == CLAuthorizationStatus::AuthorizedWhenInUse
            || status == CLAuthorizationStatus::AuthorizedAlways
    }

    impl NetworkSource for MacNetwork {
        fn current(&self) -> Result<CurrentNetwork, NetworkProblem> {
            // SAFETY: every call below is a read of the shared Wi-Fi client's
            // state; no pointer outlives the frame.
            unsafe {
                let client = CWWiFiClient::sharedWiFiClient();
                let interface = client
                    .interface()
                    .filter(|interface| interface.powerOn() && interface.serviceActive());
                let Some(interface) = interface else {
                    // No Wi-Fi network in use. A link that carries something
                    // else (Ethernet) always syncs; with no link at all,
                    // nothing is provable and uploads wait (R8.8).
                    return if link_up() {
                        Ok(CurrentNetwork::Wired)
                    } else {
                        Err(NetworkProblem::new("no active network connection"))
                    };
                };
                let status = CLLocationManager::authorizationStatus_class();
                if !location_granted(status) {
                    return Err(NetworkProblem::new(
                        "Location permission is not granted, so macOS hides the Wi-Fi name",
                    ));
                }
                let name = ssid(&interface)?;
                Ok(CurrentNetwork::Wireless(name))
            }
        }

        fn changes(&self) -> Option<Receiver<()>> {
            let (sender, receiver) = tokio::sync::mpsc::channel(4);
            std::thread::spawn(move || watch(sender));
            Some(receiver)
        }
    }

    /// Whether any non-loopback interface is up, so a Mac with Wi-Fi off is
    /// only treated as wired when a link actually carries traffic (R8.8).
    fn link_up() -> bool {
        use std::ffi::CStr;
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `getifaddrs` fills a list this function frees, and every
        // entry is read while the list is alive.
        unsafe {
            if libc::getifaddrs(&mut head) != 0 {
                return false;
            }
            let mut up = false;
            let mut cursor = head;
            while let Some(entry) = cursor.as_ref() {
                let is_link = entry
                    .ifa_addr
                    .as_ref()
                    .is_some_and(|address| i32::from(address.sa_family) == libc::AF_LINK);
                let flags = u32::try_from(entry.ifa_flags).unwrap_or(0);
                let loopback = flags & u32::try_from(libc::IFF_LOOPBACK).unwrap_or(0) != 0;
                let running = flags & u32::try_from(libc::IFF_UP).unwrap_or(0) != 0;
                let name = CStr::from_ptr(entry.ifa_name);
                if is_link && running && !loopback && !name.to_bytes().is_empty() {
                    up = true;
                    break;
                }
                cursor = entry.ifa_next;
            }
            libc::freeifaddrs(head);
            up
        }
    }

    /// The Wi-Fi service's SSID, through CaptiveNetwork (R8.8).
    ///
    /// SAFETY: the interface name is a live NSString from the Wi-Fi client.
    unsafe fn ssid(interface: &CWInterface) -> Result<String, NetworkProblem> {
        unsafe {
            let Some(interface_name) = interface.interfaceName() else {
                return Err(NetworkProblem::new(
                    "macOS reported no Wi-Fi interface name",
                ));
            };
            // NSString and CFString are toll-free bridged.
            let interface_name: &CFString = interface_name.as_ref();
            let info = CNCopyCurrentNetworkInfo(interface_name)
                .ok_or_else(|| NetworkProblem::new("macOS reported no network information"))?;
            let key = CFString::from_str("SSID");
            let info = info.cast_unchecked::<CFString, CFString>();
            let name = info
                .get(&key)
                .ok_or_else(|| NetworkProblem::new("macOS hid the Wi-Fi network name"))?;
            let name = name.to_string();
            if name.is_empty() {
                return Err(NetworkProblem::new("macOS reported an empty Wi-Fi name"));
            }
            Ok(name)
        }
    }

    /// The address the dynamic store hands back to its callback.
    struct WatchInfo {
        wake: tokio::sync::mpsc::Sender<()>,
    }

    /// The System Configuration store fires on any interface change, which is
    /// what the SSID and the link state both move with (R8.8).
    fn watch(wake: tokio::sync::mpsc::Sender<()>) {
        let name = CFString::from_str("com.elementalvoid.immich-sync");
        let info = Box::into_raw(Box::new(WatchInfo { wake: wake.clone() }));
        let mut context = objc2_system_configuration::SCDynamicStoreContext {
            version: 0,
            info: info.cast(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // SAFETY: the context and its info live until this thread drops them
        // below; the callback only reads the leaked `WatchInfo`.
        let store = unsafe { SCDynamicStore::new(None, &name, Some(store_changed), &mut context) };
        let Some(store) = store else {
            unsafe { drop(Box::from_raw(info)) };
            return;
        };
        let pattern = CFString::from_str("State:/Network/Interface/.*");
        let patterns: CFRetained<CFArray> =
            unsafe { CFRetained::cast_unchecked(CFArray::from_retained_objects(&[pattern])) };
        // SAFETY: the patterns array is a CFArray of CFStrings.
        let keys_set = unsafe { store.set_notification_keys(None, Some(&patterns)) };
        let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::Priority(
            dispatch2::DispatchQueueGlobalPriority::Default,
        ));
        // SAFETY: the queue outlives the store subscription, which is why this
        // thread keeps the store alive.
        let queued = keys_set && unsafe { store.set_dispatch_queue(Some(&queue)) };
        if queued {
            while !wake.is_closed() {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        drop(store);
        drop(queue);
        // SAFETY: this thread allocated `info` and the store is gone.
        unsafe { drop(Box::from_raw(info)) };
    }

    /// The dynamic store's change callback: wakes the engine's check (R8.8).
    unsafe extern "C-unwind" fn store_changed(
        _store: NonNull<SCDynamicStore>,
        _keys: NonNull<CFArray>,
        info: *mut c_void,
    ) {
        if info.is_null() {
            return;
        }
        // SAFETY: `watch` leaks a `WatchInfo` for exactly the store's lifetime.
        let info = unsafe { &*(info as *const WatchInfo) };
        let _ = info.wake.try_send(());
    }

    /// Asks macOS for Location permission, which the SSID needs (R8.8).
    pub fn request_location_permission() {
        // SAFETY: the manager is created and dropped on the caller's (UI)
        // thread, which is where CoreLocation wants it.
        unsafe {
            if !matches!(
                CLLocationManager::authorizationStatus_class(),
                CLAuthorizationStatus::NotDetermined
            ) {
                return;
            }
            let manager = CLLocationManager::new();
            manager.requestWhenInUseAuthorization();
            // The manager must outlive the prompt, so it is leaked on purpose:
            // one authorization request per process.
            std::mem::forget(manager);
        }
    }
}
