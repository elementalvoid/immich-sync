//! The update check (SPEC R12, ADR 0009).
//!
//! The client reads GitHub's newest published release at startup and then at
//! most once every 24 hours, and it never installs anything. The answer is one
//! system notification, one item in the tray and the app menu, and the Updates
//! page. Failure is silence: a check that cannot complete is one line in the
//! log, and the next attempt is the next interval.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use gpui_kit::*;
use serde::Deserialize;

use crate::connection::worker;

/// How often the check runs while the client runs (R12).
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The one request the check makes (R12).
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/elementalvoid/immich-sync/releases/latest";

/// The notification tag, so a later release replaces the announcement.
pub const NOTIFICATION_TAG: &str = "update";

/// One `major.minor.patch` version number (R12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// The first number.
    pub major: u32,
    /// The second number.
    pub minor: u32,
    /// The third number.
    pub patch: u32,
}

impl Version {
    /// The version `text` names, when it is three numbers with an optional
    /// leading `v` and nothing else (R12).
    ///
    /// Every other shape — a bare `v1.2`, a fourth number, a pre-release
    /// suffix — names no version, and the check reads that as "no newer
    /// release" rather than carrying a semver crate for a dozen lines.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One published release, as far as the check reads it (R12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version the tag names.
    pub version: Version,
    /// The release page the response carried (R12).
    pub url: String,
    /// The release notes, as Markdown.
    pub notes: String,
}

/// The fields the response is read for (R12). Nothing else is kept.
#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
}

/// The release a `/releases/latest` body names (R12).
///
/// A body that is not JSON, a missing field, or a tag this client cannot
/// compare names no release.
#[must_use]
pub fn parse_response(body: &str) -> Option<Release> {
    let latest: LatestRelease = serde_json::from_str(body).ok()?;
    Some(Release {
        version: Version::parse(&latest.tag_name)?,
        url: latest.html_url,
        notes: latest.body.unwrap_or_default(),
    })
}

/// The future one request awaits on the worker runtime (R12).
pub type Latest<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Option<Release>>> + Send + 'a>>;

/// Where the check reads the newest published release (R12).
///
/// One trait is the seam: the app uses [`GitHubSource`], and a test supplies
/// its own so no test reaches the network.
pub trait ReleaseSource: Send + Sync + 'static {
    /// The newest published release, or an answer that names none.
    ///
    /// # Errors
    ///
    /// Returns why the request could not complete. The caller treats that like
    /// an answer that names no release.
    fn latest(&self) -> Latest<'_>;
}

/// The real source: one unauthenticated GitHub request (R12).
pub struct GitHubSource {
    /// The client, or why it could not be built: a client that lost its
    /// timeout would hang the check forever, so the failure is carried.
    client: Result<reqwest::Client, String>,
    /// The version the request names itself with (R12).
    version: &'static str,
}

impl Default for GitHubSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GitHubSource {
    /// A client that names this build and gives up rather than hang (R12).
    #[must_use]
    pub fn new() -> Self {
        Self {
            // A check that hangs would hold the one task and the next interval
            // forever, so the client's own bound is the request's timeout.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|error| format!("could not build the HTTP client: {error}")),
            version: crate::version::package_version(),
        }
    }
}

impl ReleaseSource for GitHubSource {
    fn latest(&self) -> Latest<'_> {
        Box::pin(async move {
            let client = self
                .client
                .as_ref()
                .map_err(|reason| anyhow::anyhow!("{reason}"))?;
            let response = client
                .get(RELEASES_URL)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header(
                    reqwest::header::USER_AGENT,
                    format!("immich-sync/{}", self.version),
                )
                .send()
                .await
                .context("ask GitHub for the newest release")?;
            let response = response
                .error_for_status()
                .context("read GitHub's answer to the update check")?;
            let body = response
                .text()
                .await
                .context("read the newest release's notes")?;
            let release = parse_response(&body);
            if release.is_none() {
                // A body that is not JSON, a missing field, or a tag this
                // client cannot compare names no release (R12). The reason is
                // worth one line, because the page can only say "up to date".
                tracing::info!("R12: GitHub's answer named no release this client can compare");
            }
            Ok(release)
        })
    }
}

/// What the last check found (R12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    /// The switch is off, so nothing is known.
    Off,
    /// One request is in flight.
    Checking,
    /// The running version is the newest published one.
    UpToDate,
    /// The last check could not complete; the next one is the next interval.
    Failed,
    /// A newer release exists.
    Available(Release),
}

/// The update check, the release it found, and the latches behind the notice
/// (R12).
pub struct UpdateModel {
    source: Arc<dyn ReleaseSource>,
    interval: Duration,
    /// The package version a release is compared with (R12). It carries no
    /// revision: a build from a branch compares equal to the release it came
    /// from.
    current: Version,
    /// The **Check for updates** switch (R12).
    enabled: bool,
    /// The version the user silenced (R12).
    dismissed: Option<String>,
    status: CheckStatus,
    /// The version the last notification announced, so one release is
    /// announced once per process (R12).
    announced: Option<String>,
    /// The task that sleeps between checks (R12).
    timer: Option<tokio::task::JoinHandle<()>>,
    /// The UI-side task that applies each answer.
    answer: Option<Task<()>>,
}

impl UpdateModel {
    /// A model over the settings file's values, with nothing known yet (R12).
    #[must_use]
    pub fn new(
        source: Arc<dyn ReleaseSource>,
        interval: Duration,
        enabled: bool,
        dismissed: Option<String>,
    ) -> Self {
        Self {
            source,
            interval,
            current: Version::parse(crate::version::package_version()).unwrap_or_else(|| {
                // A version this client cannot compare names no newer release:
                // the maximum makes every tag "not newer" rather than 0.0.0,
                // which would call every release newer (R12).
                tracing::warn!(
                    version = %crate::version::package_version(),
                    "R12: the package version is not major.minor.patch"
                );
                Version {
                    major: u32::MAX,
                    minor: u32::MAX,
                    patch: u32::MAX,
                }
            }),
            enabled,
            dismissed,
            status: if enabled {
                CheckStatus::Checking
            } else {
                CheckStatus::Off
            },
            announced: None,
            timer: None,
            answer: None,
        }
    }

    /// Runs the startup check when the switch is on (R12).
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.enabled {
            self.run(cx);
        }
    }

    /// What the last check found (R12).
    #[must_use]
    pub fn status(&self) -> &CheckStatus {
        &self.status
    }

    /// The one sentence the Updates page's status row shows (R12).
    #[must_use]
    pub fn status_text(&self) -> String {
        match &self.status {
            CheckStatus::Off => "Checks are off.".to_string(),
            CheckStatus::Checking => "Checking…".to_string(),
            CheckStatus::UpToDate => format!("Up to date ({}).", self.current),
            CheckStatus::Failed => {
                "Could not check; it will try again at the next check.".to_string()
            }
            CheckStatus::Available(release) => format!("Version {} is available.", release.version),
        }
    }

    /// The newest release the last check found, dismissed or not (R12).
    #[must_use]
    pub fn release(&self) -> Option<&Release> {
        match &self.status {
            CheckStatus::Available(release) => Some(release),
            _ => None,
        }
    }

    /// Whether the user silenced the release that is known (R12).
    #[must_use]
    pub fn is_dismissed(&self) -> bool {
        match self.release() {
            Some(release) => {
                self.dismissed.as_deref().and_then(Version::parse) == Some(release.version)
            }
            None => false,
        }
    }

    /// The release both menus and the notification speak about (R12).
    ///
    /// `None` while the switch is off, while nothing newer is known, and while
    /// the user has silenced that release.
    #[must_use]
    pub fn available(&self) -> Option<&Release> {
        self.release().filter(|_| !self.is_dismissed())
    }

    /// Applies the switch's new value (R12).
    ///
    /// Turning it on checks straight away; turning it off cancels the timer,
    /// drops the known release, and takes the notice down.
    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.run(cx);
            return;
        }
        self.stop();
        self.status = CheckStatus::Off;
        cx.dismiss_system_notification(NOTIFICATION_TAG);
        cx.notify();
    }

    /// Silences or re-announces the known release (R12).
    ///
    /// The stored version, not a flag, is the whole mechanism: the next release
    /// has a different version, so it speaks again.
    pub fn set_dismissed(&mut self, dismissed: Option<String>, cx: &mut Context<Self>) {
        if self.dismissed == dismissed {
            return;
        }
        self.dismissed = dismissed;
        if self.dismissed.is_none() {
            // Remind me: the next check can announce this release again.
            self.announced = None;
        } else {
            cx.dismiss_system_notification(NOTIFICATION_TAG);
        }
        cx.notify();
    }

    /// Starts one check now and then one per interval until the switch goes off
    /// (R12).
    fn run(&mut self, cx: &mut Context<Self>) {
        self.stop();
        self.status = CheckStatus::Checking;
        let Ok(runtime) = worker() else {
            // The runtime is the only thing a check runs on, so its absence is
            // a check that could not complete, not an error to show.
            tracing::info!("R12: the update check could not start");
            self.status = CheckStatus::Failed;
            return;
        };
        let source = self.source.clone();
        let interval = self.interval;
        let (answers, incoming) = async_channel::unbounded();
        self.timer = Some(runtime.spawn(async move {
            loop {
                if answers.send(source.latest().await).await.is_err() {
                    break;
                }
                tokio::time::sleep(interval).await;
            }
        }));
        self.answer = Some(cx.spawn(async move |this, cx| {
            while let Ok(result) = incoming.recv().await {
                let _ = this.update(cx, |model, cx| model.apply(result, cx));
            }
        }));
        cx.notify();
    }

    /// Cancels the timer and the wait for its answer (R12).
    fn stop(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
        self.answer = None;
    }

    /// Records what one request answered, and announces it once (R12).
    fn apply(&mut self, result: anyhow::Result<Option<Release>>, cx: &mut Context<Self>) {
        self.status = match result {
            Ok(Some(release)) if release.version > self.current => CheckStatus::Available(release),
            Ok(_) => CheckStatus::UpToDate,
            Err(error) => {
                tracing::info!(
                    error = %format!("{error:#}"),
                    "R12: the update check could not read GitHub"
                );
                CheckStatus::Failed
            }
        };
        if let Some(release) = self.available().cloned() {
            let version = release.version.to_string();
            if self.announced.as_deref() != Some(version.as_str()) {
                tracing::info!(
                    version = %version,
                    "R12: a newer release raised a notification"
                );
                self.announced = Some(version);
                cx.show_system_notification(update_notification(release.version));
            }
        }
        cx.notify();
    }
}

/// The system notification a newer release raises (R12).
///
/// The stable tag replaces a previous announcement where the platform supports
/// it, and it offers no action: the action lives in the tray, the app menu, and
/// the Updates page, which are the surfaces that persist.
#[must_use]
pub fn update_notification(version: Version) -> SystemNotification {
    SystemNotification {
        tag: NOTIFICATION_TAG.into(),
        title: "Immich Sync".into(),
        body: format!("Version {version} is available.").into(),
        actions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CheckStatus, INTERVAL, NOTIFICATION_TAG, Release, UpdateModel, Version, parse_response,
        update_notification,
    };
    use gpui_kit::{AppContext as _, Entity, TestAppContext};
    use std::sync::Arc;

    fn version(text: &str) -> Version {
        Version::parse(text).expect("a version this test named")
    }

    fn release(text: &str) -> Release {
        Release {
            version: version(text),
            url: format!("https://github.com/elementalvoid/immich-sync/releases/tag/v{text}"),
            notes: "Fixes and a new folder view.".to_string(),
        }
    }

    /// A source that answers "nothing newer", so a model can be built here
    /// without a request. The async checks are tested from `tests/it/update.rs`.
    struct Silent;

    impl super::ReleaseSource for Silent {
        fn latest(&self) -> super::Latest<'_> {
            Box::pin(async { Ok(None) })
        }
    }

    fn model(enabled: bool, dismissed: Option<&str>) -> UpdateModel {
        UpdateModel::new(
            Arc::new(Silent),
            INTERVAL,
            enabled,
            dismissed.map(str::to_string),
        )
    }

    /// Applies `answer` the way a landed check does.
    fn apply(cx: &mut TestAppContext, updates: &Entity<UpdateModel>, answer: Option<Release>) {
        cx.update(|cx| updates.update(cx, |model, cx| model.apply(Ok(answer), cx)));
    }

    fn text(cx: &TestAppContext, updates: &Entity<UpdateModel>) -> String {
        cx.read(|cx| updates.read(cx).status_text())
    }

    #[test]
    fn a_tag_becomes_three_numbers() {
        assert_eq!(
            version("v1.2.3"),
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
        assert_eq!(
            version("0.4.0"),
            Version {
                major: 0,
                minor: 4,
                patch: 0
            },
            "R12: a tag without the leading v reads the same"
        );
    }

    #[test]
    fn every_other_tag_shape_names_no_version() {
        for tag in ["v1.2", "v1.2.3.4", "v1.x.3", "latest", "", "v", "1.2.3-rc1"] {
            assert_eq!(
                Version::parse(tag),
                None,
                "R12: {tag:?} is not a release this client compares"
            );
        }
    }

    #[test]
    fn versions_order_by_major_then_minor_then_patch() {
        assert!(version("0.4.0") > version("0.3.9"), "R12: a newer release");
        assert!(version("1.0.0") > version("0.9.9"));
        assert_eq!(version("0.3.0"), version("v0.3.0"), "R12: an equal release");
        assert!(version("0.2.9") < version("0.3.0"), "R12: an older release");
    }

    #[test]
    fn the_response_is_read_for_its_three_fields() {
        let body = r#"{
            "tag_name": "v0.4.0",
            "html_url": "https://github.com/elementalvoid/immich-sync/releases/tag/v0.4.0",
            "body": "What changed:\n\n- One fix.",
            "draft": false
        }"#;
        let release = parse_response(body).expect("a valid response names a release");
        assert_eq!(release.version, version("0.4.0"));
        assert_eq!(
            release.url, "https://github.com/elementalvoid/immich-sync/releases/tag/v0.4.0",
            "R12: the release page is the URL the response carried"
        );
        assert_eq!(release.notes, "What changed:\n\n- One fix.");
    }

    #[test]
    fn a_response_without_the_fields_names_no_release() {
        assert_eq!(parse_response("{}"), None);
        assert_eq!(
            parse_response(r#"{"tag_name": "v0.4.0"}"#),
            None,
            "R12: the release page is required"
        );
        assert_eq!(
            parse_response(r#"{"html_url": "https://example.com", "body": "b"}"#),
            None,
            "R12: the tag is required"
        );
        assert_eq!(parse_response("not json"), None);
        assert_eq!(
            parse_response(r#"{"tag_name": "nightly", "html_url": "u", "body": "b"}"#),
            None,
            "R12: a tag it cannot compare names no release"
        );
        assert_eq!(
            parse_response(r#"{"tag_name": "v0.5.0", "html_url": "u", "body": null}"#),
            Some(Release {
                version: version("0.5.0"),
                url: "u".to_string(),
                notes: String::new(),
            }),
            "R12: a release published without notes is still a release"
        );
    }

    #[test]
    fn the_notification_names_the_release_it_announces() {
        assert_eq!(
            update_notification(version("0.4.0")),
            gpui_kit::SystemNotification {
                tag: NOTIFICATION_TAG.into(),
                title: "Immich Sync".into(),
                body: "Version 0.4.0 is available.".into(),
                actions: Vec::new(),
            }
        );
    }

    /// R12: one release is announced once per process, and a newer release is a
    /// new announcement.
    #[gpui_kit::test]
    fn one_release_is_announced_once_and_the_next_one_again(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_app_identity(crate::BUNDLE_ID, "Immich Sync"));
        let updates = cx.update(|cx| cx.new(|_| model(true, None)));
        apply(cx, &updates, None);
        assert_eq!(
            cx.read(|cx| updates.read(cx).status().clone()),
            CheckStatus::UpToDate
        );
        assert_eq!(text(cx, &updates), "Up to date (0.3.0).");

        apply(cx, &updates, Some(release("0.4.0")));
        apply(cx, &updates, Some(release("0.4.0")));
        assert_eq!(
            cx.shown_system_notifications()
                .iter()
                .filter(|notification| notification.tag == NOTIFICATION_TAG)
                .count(),
            1,
            "R12: the same release is announced once per process"
        );
        assert_eq!(text(cx, &updates), "Version 0.4.0 is available.");

        apply(cx, &updates, Some(release("0.5.0")));
        assert_eq!(
            cx.delivered_system_notifications().len(),
            1,
            "R12: the next release replaces the announcement"
        );
        assert_eq!(
            cx.delivered_system_notifications()[0].body,
            "Version 0.5.0 is available."
        );
    }

    /// R12: a dismissed release raises no notification and carries no menu
    /// item, and the next version speaks again.
    #[gpui_kit::test]
    fn a_dismissed_release_stays_quiet_until_the_next_version(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_app_identity(crate::BUNDLE_ID, "Immich Sync"));
        let updates = cx.update(|cx| cx.new(|_| model(true, Some("0.4.0"))));
        apply(cx, &updates, Some(release("0.4.0")));
        assert!(
            cx.read(|cx| updates.read(cx).available().is_none()),
            "R12: a dismissed release carries no menu item"
        );
        assert_eq!(
            text(cx, &updates),
            "Version 0.4.0 is available.",
            "the page still says what is there"
        );
        assert!(
            cx.delivered_system_notifications().is_empty(),
            "R12: a dismissed release raises no notification"
        );

        apply(cx, &updates, Some(release("0.5.0")));
        assert_eq!(
            cx.delivered_system_notifications().len(),
            1,
            "R12: the next release is a new announcement"
        );
        assert!(
            cx.read(|cx| updates.read(cx).available().is_some()),
            "the next release carries the menu item again"
        );
    }

    /// R12: the four states the page's status row spells out.
    #[gpui_kit::test]
    fn the_status_row_spells_out_each_state(cx: &mut TestAppContext) {
        let off = cx.update(|cx| cx.new(|_| model(false, None)));
        assert_eq!(text(cx, &off), "Checks are off.");

        let checking = cx.update(|cx| cx.new(|_| model(true, None)));
        assert_eq!(text(cx, &checking), "Checking…");

        let failed = cx.update(|cx| cx.new(|_| model(true, None)));
        cx.update(|cx| failed.update(cx, |model, cx| model.apply(Err(anyhow::anyhow!("403")), cx)));
        assert_eq!(
            text(cx, &failed),
            "Could not check; it will try again at the next check."
        );
    }

    /// R12: turning the switch off drops the known release and takes the notice
    /// down; turning it back on checks again.
    #[gpui_kit::test]
    fn the_switch_drops_the_release_and_the_notice(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_app_identity(crate::BUNDLE_ID, "Immich Sync"));
        let updates = cx.update(|cx| cx.new(|_| model(true, None)));
        apply(cx, &updates, Some(release("0.4.0")));
        assert_eq!(cx.delivered_system_notifications().len(), 1);

        cx.update(|cx| updates.update(cx, |model, cx| model.set_enabled(false, cx)));
        assert_eq!(text(cx, &updates), "Checks are off.");
        assert!(cx.read(|cx| updates.read(cx).release().is_none()));
        assert!(cx.delivered_system_notifications().is_empty());
    }
}
