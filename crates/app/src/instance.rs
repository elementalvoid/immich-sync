//! Single instance: one running client per user.
//!
//! The first launch binds a namespaced local socket (a named pipe on Windows,
//! a Unix-domain socket elsewhere). A later launch connects, asks the running
//! instance to show its window, and exits.

use async_channel::Sender;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
use std::io::{self, Read as _, Write as _};

/// The per-user socket name that marks the running instance.
pub const SOCKET_NAME: &str = "immich-sync.sock";

/// A message a second launch sends to the running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Bring the main window to the front, opening it if needed.
    Show,
}

impl Request {
    /// Wire form of [`Request::Show`].
    pub const SHOW: &'static [u8] = b"show\n";

    /// Parses a wire message. Unknown payloads return `None`.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        (bytes == Self::SHOW).then_some(Self::Show)
    }
}

/// Claims the single instance, or hands off to the running one.
///
/// Returns `Some(listener)` for the primary and `None` after asking the
/// running instance to focus. Connect first and bind on failure: binding
/// first would make a second launch look like the primary until the bind
/// fails.
///
/// # Errors
///
/// Returns an error when the socket can be neither reached nor bound.
pub fn claim(name: &str) -> io::Result<Option<LocalSocketListener>> {
    let name = name.to_ns_name::<GenericNamespaced>()?;
    if let Ok(mut stream) = LocalSocketStream::connect(name.clone()) {
        // The connect already proved a primary exists; a dropped handshake
        // must not turn this process into a duplicate primary.
        let _ = stream.write_all(Request::SHOW);
        return Ok(None);
    }

    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    Ok(Some(listener))
}

/// Blocks for the next recognised request, skipping unknown messages.
///
/// # Errors
///
/// Returns an error when the listener itself fails.
pub fn next_request(listener: &LocalSocketListener) -> io::Result<Request> {
    loop {
        let Some(result) = listener.incoming().next() else {
            return Err(io::Error::other("single-instance listener stopped"));
        };
        let mut connection = result?;
        let mut buffer = [0u8; 64];
        let read = connection.read(&mut buffer)?;
        if let Some(request) = Request::parse(&buffer[..read]) {
            return Ok(request);
        }
    }
}

/// Forwards every request from `listener` to `requests`. Runs on its own thread.
pub fn forward(listener: LocalSocketListener, requests: Sender<Request>) {
    loop {
        match next_request(&listener) {
            Ok(request) => {
                if requests.send_blocking(request).is_err() {
                    break;
                }
            }
            Err(error) => {
                eprintln!("immich-sync: single-instance listener stopped: {error}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_show_request() {
        assert_eq!(Request::parse(b"show\n"), Some(Request::Show));
    }

    #[test]
    fn ignores_an_unknown_request() {
        assert_eq!(Request::parse(b"pause\n"), None);
    }

    #[test]
    fn second_launch_hands_off_to_the_primary() {
        let name = format!("immich-sync-test-{}.sock", std::process::id());

        let listener = claim(&name)
            .expect("bind the single-instance socket")
            .expect("the first claim is the primary");
        let second = claim(&name).expect("connect to the primary");
        assert!(second.is_none(), "the second claim is a secondary");

        assert_eq!(
            next_request(&listener).expect("read the handoff"),
            Request::Show
        );
    }
}
