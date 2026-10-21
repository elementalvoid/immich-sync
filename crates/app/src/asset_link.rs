//! The asset link: one uploaded asset's Immich web address (#44, GLOSSARY
//! "Asset link").
//!
//! The configured server URL is the API base, so a trailing `/api` is dropped
//! before the web page's path is added; a reverse-proxy prefix in front of it
//! stays. Only an `http` or `https` base produces a URL, so a hand-edited
//! settings file cannot make the client open a `file:` or `mailto:` target.
//!
//! Both links the app builds share [`web_root`]: the asset link here and the
//! sync tag's page in [`crate::settings_view::sync_tag_url`].

use std::sync::Arc;

use gpui_kit::App;

/// What opening a web address runs.
///
/// The product opens the platform's default browser; a test injects a recorder,
/// so a click can be asserted without launching anything.
pub type LinkOpener = Arc<dyn Fn(&str, &App)>;

/// The opener the product uses: the default browser.
#[must_use]
pub fn browser_opener() -> LinkOpener {
    Arc::new(|url, cx| cx.open_url(url))
}

/// The web root of `server_url`: its scheme, host, and any reverse-proxy
/// prefix, without the API base's `/api` and without a trailing separator.
///
/// `None` when the configured value is not an `http`/`https` address with a
/// host, or holds a character a URL cannot carry as written.
#[must_use]
pub fn web_root(server_url: &str) -> Option<String> {
    let trimmed = server_url.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    if rest
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    let web = rest.strip_suffix("/api").unwrap_or(rest);
    let web = web.trim_end_matches('/');
    if web.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{web}"))
}

/// The Immich web address of one uploaded asset (#44).
///
/// For example `https://immich.example.com/api` and the asset
/// `b1e0…` give `https://immich.example.com/photos/b1e0…`.
///
/// `None` when there is no web root to open, or the asset id is not one this
/// client could have received: an id with a separator or a query character
/// would build a different page than the asset's own.
#[must_use]
pub fn asset_url(server_url: &str, asset_id: &str) -> Option<String> {
    if asset_id.is_empty()
        || !asset_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return None;
    }
    Some(format!("{}/photos/{asset_id}", web_root(server_url)?))
}

/// Whether `url` is an absolute `http`/`https` address the client may open.
#[must_use]
pub fn is_web_url(url: &str) -> bool {
    web_root(url).is_some()
}

/// Opens `url` with `opener` when it is a web address, and returns whether it
/// did (#44).
///
/// A malformed or non-`http(s)` target is never handed to the browser, whatever
/// a hand-edited settings file or a damaged row holds.
pub fn open_asset(url: &str, opener: &LinkOpener, cx: &App) -> bool {
    if !is_web_url(url) {
        return false;
    }
    opener(url, cx);
    true
}

#[cfg(test)]
mod tests {
    use super::{asset_url, is_web_url, open_asset, web_root};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// #44: the link is the configured server URL minus a trailing `/api`, plus
    /// `/photos/{assetId}`; a reverse-proxy prefix stays.
    #[test]
    fn the_asset_link_is_the_web_root_plus_the_asset() {
        assert_eq!(
            asset_url("https://immich.example.com/api", "b1e0c3"),
            Some("https://immich.example.com/photos/b1e0c3".to_string())
        );
        assert_eq!(
            asset_url("https://immich.example.com/immich/api/", "b1e0c3"),
            Some("https://immich.example.com/immich/photos/b1e0c3".to_string()),
            "a reverse-proxy path prefix stays"
        );
        assert_eq!(
            asset_url(" https://immich.example.com ", "b1e0c3"),
            Some("https://immich.example.com/photos/b1e0c3".to_string()),
            "a server URL with no API suffix works too"
        );
        assert_eq!(
            web_root("https://immich.example.com/immich/api"),
            Some("https://immich.example.com/immich".to_string())
        );
    }

    /// #44: without a web URL there is no link, and an id that would build a
    /// different page is refused.
    #[test]
    fn there_is_no_asset_link_without_a_web_url_or_a_plain_asset_id() {
        assert_eq!(asset_url("", "b1e0c3"), None, "no server URL means no link");
        assert_eq!(asset_url("  ", "b1e0c3"), None);
        assert_eq!(
            asset_url("file:///tmp/immich", "b1e0c3"),
            None,
            "only http(s) is ever opened"
        );
        assert_eq!(asset_url("mailto:someone@example.com", "b1e0c3"), None);
        assert_eq!(asset_url("https:///api", "b1e0c3"), None, "no host to open");
        assert_eq!(
            asset_url("https://immich.example.com/api", ""),
            None,
            "an asset with no id has no page"
        );
        for id in ["../settings", "b1e0?x=1", "b1e0#frag", "b1e 0"] {
            assert_eq!(
                asset_url("https://immich.example.com", id),
                None,
                "{id} would not address the asset's own page"
            );
        }
    }

    /// #44: the opener only ever answers a web address.
    #[test]
    fn only_an_http_address_reaches_the_browser() {
        assert!(is_web_url("https://immich.example.com/photos/b1e0c3"));
        assert!(is_web_url("http://localhost:2283/photos/b1e0c3"));
        for refused in [
            "",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://immich.example.com/a b",
            "immich.example.com/photos/b1e0c3",
        ] {
            assert!(!is_web_url(refused), "{refused} must not be opened");
        }

        let opened: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = opened.clone();
        let opener: super::LinkOpener = Arc::new(move |url, _| {
            recorder
                .lock()
                .expect("the recorder lock")
                .push(url.to_string());
        });
        // The recorder never reads the app, so a bare test app serves.
        let cx = gpui_kit::TestAppContext::single();
        cx.update(|cx| {
            assert!(open_asset(
                "https://immich.example.com/photos/b1e0c3",
                &opener,
                cx
            ));
            assert!(!open_asset("file:///etc/passwd", &opener, cx));
        });
        assert_eq!(
            opened.lock().expect("the recorder lock").as_slice(),
            ["https://immich.example.com/photos/b1e0c3"],
            "#44: only the web address reached the opener"
        );
    }
}
