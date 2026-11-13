//! The traffic builder: the rules the tests and the benchmarks share.
//!
//! Every test that puts events in by hand used to write the same twenty lines
//! of struct literal. They all went through one builder instead — and this is
//! what that builder promises.

use sumimi_service::models::HitKind;
use sumimi_service::testing::HitBuilder;

/// A page view, with only what the caller named.
#[test]
fn a_builder_names_only_what_the_caller_cares_about() {
    let hit = HitBuilder::new("w1", "s1", "v1").url("/one").build();

    assert_eq!(hit.website_id, "w1");
    assert_eq!(hit.url.as_deref(), Some("/one"));

    // Everything else keeps the defaults it has everywhere else.
    assert_eq!(hit.title, None);
    assert_eq!(hit.referrer, None);
    assert_eq!(hit.country, None);
    assert_eq!(hit.user_id, None);
    assert_eq!(hit.retention_days, 0);
    assert_eq!(hit.kind, HitKind::Pageview);
}

/// The same text always means the same session.
#[test]
fn the_same_name_means_the_same_session() {
    let one = HitBuilder::new("w1", "s1", "v1").build();
    let two = HitBuilder::new("w1", "s1", "v1").build();
    let other_visit = HitBuilder::new("w1", "s2", "v1").build();
    let other_visitor = HitBuilder::new("w1", "s1", "v2").build();

    assert_eq!(one.visit_id, two.visit_id, "the same visit");
    assert_eq!(one.ip_prefix, two.ip_prefix, "the same visitor");
    assert_eq!(one.user_agent, two.user_agent, "the same visitor");
    assert_ne!(one.visit_id, other_visit.visit_id, "a different visit");
    assert_ne!(
        (one.ip_prefix.as_str(), one.user_agent.as_str()),
        (other_visitor.ip_prefix.as_str(), other_visitor.user_agent.as_str()),
        "a different visitor"
    );
}

/// No value and an empty value are two different things.
///
/// A row with no referrer and a row with an empty referrer are two different
/// rows — the deep-read check says so, and the builder has to keep them apart.
#[test]
fn no_value_and_an_empty_value_are_different() {
    let hit = HitBuilder::new("w1", "s1", "v1")
        .url("/one")
        .title("")
        .referrer(None)
        .build();

    assert_eq!(hit.url.as_deref(), Some("/one"));
    assert_eq!(hit.title.as_deref(), Some(""), "an empty title is an empty title");
    assert_eq!(hit.referrer, None, "no referrer at all");
}

/// The moment it happened.
#[test]
fn the_moment_is_the_one_that_was_asked_for() {
    let at = chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
        .unwrap()
        .and_hms_opt(12, 30, 0)
        .unwrap();

    let hit = HitBuilder::new("w1", "s1", "v1").at(at).build();
    assert_eq!(hit.created_at, at);
}

/// The kind of hit it is.
#[test]
fn the_kind_is_the_one_that_was_asked_for() {
    let hit = HitBuilder::new("w1", "s1", "v1")
        .kind(HitKind::Event)
        .name("signup")
        .build();

    assert_eq!(hit.kind, HitKind::Event);
    assert_eq!(hit.name.as_deref(), Some("signup"));
}

/// The identity the site owner knows the visitor by, when there is one.
#[test]
fn the_user_is_named() {
    let hit = HitBuilder::new("w1", "s1", "v1").user("user-1").build();
    assert!(hit.user_id.is_some());

    let same = HitBuilder::new("w1", "s1", "v1").user("user-1").build();
    assert_eq!(hit.user_id, same.user_id);
}

/// The numbers are numbers.
#[test]
fn the_numbers_are_stored_as_numbers() {
    let hit = HitBuilder::new("w1", "s1", "v1")
        .latitude(52.52)
        .longitude(13.405)
        .lcp(600.0)
        .build();

    assert_eq!(hit.latitude, Some(52.52));
    assert_eq!(hit.longitude, Some(13.405));
    assert_eq!(hit.lcp, Some(600.0));
}
