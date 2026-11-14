//! Headless tests for the update check's scheduler (R12, ADR 0009).
//!
//! The source is the fake in `common`, so nothing here reaches GitHub. The
//! answers themselves — the tag comparison, the response fields, the notice —
//! are unit tests beside the code.

use super::common;

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use immich_sync::update::{CheckStatus, INTERVAL, ReleaseSource, UpdateModel};

/// A started check over `source`, with `interval` between its requests.
fn start(
    cx: &mut TestAppContext,
    source: Arc<dyn ReleaseSource>,
    interval: Duration,
    enabled: bool,
) -> Entity<UpdateModel> {
    let updates = cx.update(|cx| cx.new(|_| UpdateModel::new(source, interval, enabled, None)));
    cx.update(|cx| updates.update(cx, |model, cx| model.start(cx)));
    updates
}

/// Pumps the executor until `wanted` holds.
async fn wait_until(cx: &mut TestAppContext, wanted: impl Fn(&TestAppContext) -> bool) {
    for _ in 0..600 {
        cx.run_until_parked();
        if wanted(cx) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

fn status(cx: &TestAppContext, updates: &Entity<UpdateModel>) -> CheckStatus {
    cx.read(|cx| updates.read(cx).status().clone())
}

/// R12: the switch on means one request at startup.
#[gpui_kit::test]
async fn the_check_asks_once_at_startup(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let source = common::FakeReleases::new(vec![common::Answer::UpToDate]);
    let updates = start(cx, source.clone(), INTERVAL, true);

    wait_until(cx, |cx| status(cx, &updates) == CheckStatus::UpToDate).await;
    assert_eq!(source.requests(), 1, "R12: one startup request");
}

/// R12: the switch off means no request at all.
#[gpui_kit::test]
async fn a_check_that_is_off_asks_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let source = common::FakeReleases::new(vec![common::Answer::UpToDate]);
    let updates = start(cx, source.clone(), INTERVAL, false);

    cx.executor().timer(Duration::from_millis(200)).await;
    cx.run_until_parked();

    assert_eq!(status(cx, &updates), CheckStatus::Off);
    assert_eq!(
        source.requests(),
        0,
        "R12: a check that is off asks nothing"
    );
}

/// R12: one request every interval, never two inside one.
#[gpui_kit::test]
async fn a_second_check_waits_for_the_interval(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let source = common::FakeReleases::new(vec![common::Answer::UpToDate]);
    let updates = start(cx, source.clone(), INTERVAL, true);

    wait_until(cx, |cx| status(cx, &updates) == CheckStatus::UpToDate).await;
    // Well inside the 24 hours the interval is.
    cx.executor().timer(Duration::from_millis(200)).await;
    cx.run_until_parked();

    assert_eq!(
        source.requests(),
        1,
        "R12: a second check inside the interval does not happen"
    );
}

/// R12: the task checks again once the interval passes.
#[gpui_kit::test]
async fn the_check_runs_again_after_the_interval(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let source = common::FakeReleases::new(vec![common::Answer::UpToDate]);
    let _updates = start(cx, source.clone(), Duration::from_millis(20), true);

    wait_until(cx, |_| source.requests() >= 2).await;
}

/// R12: turning the switch off cancels the timer and drops the answer it found.
#[gpui_kit::test]
async fn turning_the_check_off_cancels_the_timer(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let release = common::newer_release();
    let source = common::FakeReleases::new(vec![common::Answer::Release(release.clone())]);
    let updates = start(cx, source.clone(), Duration::from_millis(20), true);

    wait_until(cx, |cx| {
        cx.read(|cx| updates.read(cx).release() == Some(&release))
    })
    .await;
    cx.update(|cx| updates.update(cx, |model, cx| model.set_enabled(false, cx)));
    let settled = source.requests();
    cx.executor().timer(Duration::from_millis(200)).await;
    cx.run_until_parked();

    assert_eq!(status(cx, &updates), CheckStatus::Off);
    assert!(
        cx.read(|cx| updates.read(cx).release().is_none()),
        "R12: the known release is dropped"
    );
    assert_eq!(
        source.requests(),
        settled,
        "R12: a cancelled timer makes no further request"
    );
}
