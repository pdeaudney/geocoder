//! Validates review item #3: `format_address` allocates many small `String`s
//! per call. This test installs a counting global allocator so allocations
//! through `std::alloc::System` are tallied, then asserts the current
//! allocation count is over an unreasonable threshold.
//!
//! The threshold is deliberately set to today's observed cost. After a fix
//! that builds a single pre-sized `String`, this should drop dramatically and
//! the asserted upper bound tightens.

use query_server::{format_address, AddressDetails};
use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

// --- Output-correctness checks so the single-buffer rewrite cannot
// silently diverge from the expected format per locale ---

fn details_full_au<'a>(hn: &'a str) -> AddressDetails<'a> {
    AddressDetails {
        house_number: Some(Cow::Borrowed(hn)),
        road: Some("Elizabeth Street"),
        city: Some("Sydney"),
        state: Some("New South Wales"),
        postcode: Some("2000"),
        country: Some("Australia"),
        country_code: Some(String::from("AU")),
        ..Default::default()
    }
}

#[test]
fn au_format_is_number_street_city_state_postcode_country() {
    let hn = String::from("123");
    let d = details_full_au(&hn);
    let out = format_address(&d).unwrap();
    assert_eq!(out, "123 Elizabeth Street, Sydney, New South Wales 2000, Australia");
}

#[test]
fn au_format_without_house_number() {
    let d = AddressDetails {
        road: Some("George Street"),
        city: Some("Brisbane"),
        state: Some("Queensland"),
        postcode: None,
        country: Some("Australia"),
        country_code: Some(String::from("AU")),
        ..Default::default()
    };
    let out = format_address(&d).unwrap();
    assert_eq!(out, "George Street, Brisbane, Queensland, Australia");
}

#[test]
fn au_format_admin_only() {
    // No road at all — the query missed every street. Current behaviour is
    // to emit the admin hierarchy only.
    let d = AddressDetails {
        city: Some("Sydney"),
        state: Some("New South Wales"),
        country: Some("Australia"),
        country_code: Some(String::from("AU")),
        ..Default::default()
    };
    let out = format_address(&d).unwrap();
    assert_eq!(out, "Sydney, New South Wales, Australia");
}

#[test]
fn european_format_is_street_number_postcode_city_country() {
    // Germany: number after street, postcode before city, no state.
    let hn = String::from("12");
    let d = AddressDetails {
        house_number: Some(Cow::Borrowed(&hn)),
        road: Some("Friedrichstraße"),
        city: Some("Berlin"),
        postcode: Some("10117"),
        country: Some("Germany"),
        country_code: Some(String::from("DE")),
        ..Default::default()
    };
    let out = format_address(&d).unwrap();
    assert_eq!(out, "Friedrichstraße 12, 10117 Berlin, Germany");
}

#[test]
fn japan_format_is_number_street_postcode_city_state() {
    // Japan: number before street, postcode before city, include state.
    let hn = String::from("1");
    let d = AddressDetails {
        house_number: Some(Cow::Borrowed(&hn)),
        road: Some("Chome-2 Nihonbashi"),
        city: Some("Chuo City"),
        state: Some("Tokyo"),
        postcode: Some("103-0027"),
        country: Some("Japan"),
        country_code: Some(String::from("JP")),
        ..Default::default()
    };
    let out = format_address(&d).unwrap();
    assert_eq!(out, "1 Chome-2 Nihonbashi, 103-0027 Chuo City, Tokyo, Japan");
}

#[test]
fn empty_details_returns_none() {
    let d = AddressDetails::default();
    assert!(format_address(&d).is_none());
}

fn count_allocs(f: impl FnOnce()) -> usize {
    let before = ALLOCS.load(Ordering::Relaxed);
    f();
    ALLOCS.load(Ordering::Relaxed) - before
}

#[test]
fn format_full_au_address_allocation_count_is_bounded() {
    let road = "Elizabeth Street";
    let hn = String::from("123");
    let city = "Sydney";
    let state = "New South Wales";
    let postcode = "2000";
    let country = "Australia";

    // Warm up so lazy globals don't pollute the count.
    for _ in 0..3 {
        let details = AddressDetails {
            house_number: Some(Cow::Borrowed(&hn)),
            road: Some(road),
            city: Some(city),
            state: Some(state),
            postcode: Some(postcode),
            country: Some(country),
            country_code: Some(String::from("AU")),
            ..Default::default()
        };
        let _ = format_address(&details);
    }

    let details = AddressDetails {
        house_number: Some(Cow::Borrowed(&hn)),
        road: Some(road),
        city: Some(city),
        state: Some(state),
        postcode: Some(postcode),
        country: Some(country),
        country_code: Some(String::from("AU")),
        ..Default::default()
    };

    let allocs = count_allocs(|| {
        let out = format_address(&details);
        assert!(out.is_some());
        std::hint::black_box(out);
    });

    eprintln!("format_address allocations (AU full address): {allocs}");

    // Post-fix: format_address builds a single pre-sized String. The only
    // allocation per call should be that one backing buffer. Allowing up to
    // 2 as a tolerance in case the capacity estimate underflows for some
    // address layouts and the String grows.
    const TARGET_MAX_ALLOCS: usize = 2;
    assert!(
        allocs <= TARGET_MAX_ALLOCS,
        "format_address allocated {allocs}x; target after fix is ≤{TARGET_MAX_ALLOCS}",
    );
}
