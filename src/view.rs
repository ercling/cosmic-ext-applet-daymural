// The popup's layout (Task 8). `app.rs` keeps state + update; everything
// visual lives here. The iced view code itself is exempt from unit tests
// (verified manually in the panel), but the pure decisions it renders —
// which entry is shown, which navigation targets exist, how the footer
// timestamp reads — are extracted below and tested.

use std::path::Path;

use chrono::NaiveDateTime;
use cosmic::{
    Element,
    applet::{menu_button, padded_control},
    iced::{Alignment, Length},
    widget,
};

use crate::app::{self, Message, Window};
use crate::catalogue::{Catalogue, ImageEntry};
use crate::thumbs;

/// Height of the placeholder box shown when a thumbnail is missing
/// (e.g. entries rebuilt from a folder scan that never went through a
/// fetch). The UI must never decode the full UHD file to fill this.
const PLACEHOLDER_HEIGHT: f32 = 160.0;

/// Shuffle interval choices, index-aligned with
/// [`SHUFFLE_INTERVAL_LABELS`].
const SHUFFLE_INTERVAL_SECS: [u32; 4] = [1_800, 3_600, 21_600, 86_400];

/// Dropdown labels for the shuffle intervals.
const SHUFFLE_INTERVAL_LABELS: &[&str] = &["30 minutes", "1 hour", "6 hours", "Daily"];

/// Retention choices in days (0 = keep forever), index-aligned with
/// [`RETENTION_LABELS`].
const RETENTION_DAYS: [u16; 4] = [3, 8, 30, 0];

/// Dropdown labels for the retention choices.
const RETENTION_LABELS: &[&str] = &["3 days", "8 days", "30 days", "Forever"];

/// Dropdown index of the default retention (8 days) — the fallback for
/// hand-edited config values that match no choice.
const RETENTION_DEFAULT_INDEX: usize = 1;

// ---------------------------------------------------------------------------
// Pure decisions (tested)
// ---------------------------------------------------------------------------

/// The entry the popup presents: the currently applied file when it is one
/// of ours, otherwise the newest downloaded image (the user's own wallpaper
/// cannot be thumbnailed without decoding it — accepted, matches the
/// reference which also only previews its own images).
pub(crate) fn displayed<'a>(
    catalogue: &'a Catalogue,
    current: Option<&Path>,
) -> Option<&'a ImageEntry> {
    current
        .and_then(|c| catalogue.images.iter().find(|e| e.filename == c))
        .or_else(|| catalogue.newest())
}

/// Target for the "previous" button. Browsing our history walks one step
/// older; a foreign (or unknown) current wallpaper enters the history at
/// its newest end. `None` = disabled (oldest end, or nothing downloaded).
pub(crate) fn prev_target<'a>(
    catalogue: &'a Catalogue,
    current: Option<&Path>,
) -> Option<&'a ImageEntry> {
    match current {
        Some(c) if catalogue.contains(c) => catalogue.prev(c),
        _ => catalogue.newest(),
    }
}

/// Target for the "next" button: one step newer, only meaningful while the
/// current wallpaper is inside our history. `None` = disabled (newest end,
/// foreign wallpaper, or nothing downloaded).
pub(crate) fn next_target<'a>(
    catalogue: &'a Catalogue,
    current: Option<&Path>,
) -> Option<&'a ImageEntry> {
    current
        .filter(|c| catalogue.contains(c))
        .and_then(|c| catalogue.next(c))
}

/// Target for the "jump to newest" button. `None` = disabled (already
/// applied, or nothing downloaded).
pub(crate) fn newest_target<'a>(
    catalogue: &'a Catalogue,
    current: Option<&Path>,
) -> Option<&'a ImageEntry> {
    catalogue
        .newest()
        .filter(|n| Some(n.filename.as_path()) != current)
}

/// Heading text for an entry. Entries rebuilt from a folder scan carry no
/// title until the next fetch refills it — fall back to the file stem so
/// the user still sees which image is applied.
pub(crate) fn display_title(entry: &ImageEntry) -> String {
    if !entry.title.is_empty() {
        return entry.title.clone();
    }
    entry
        .filename
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Bing wallpaper")
        .to_owned()
}

/// Footer timestamp: "Updated today at 09:12" / "… yesterday at …" /
/// "Updated Aug 5 at 09:12". Both instants are local wall-clock time.
pub(crate) fn format_updated(updated: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = updated.format("%H:%M");
    if updated.date() == now.date() {
        format!("Updated today at {time}")
    } else if now.date().pred_opt() == Some(updated.date()) {
        format!("Updated yesterday at {time}")
    } else {
        format!("Updated {} at {time}", updated.format("%b %-d"))
    }
}

/// Dropdown index for a stored interval. Unknown values (hand-edited
/// config) display as the daily default rather than crashing or showing
/// an empty selection.
pub(crate) fn shuffle_interval_index(secs: u32) -> usize {
    SHUFFLE_INTERVAL_SECS
        .iter()
        .position(|&s| s == secs)
        .unwrap_or(SHUFFLE_INTERVAL_SECS.len() - 1)
}

/// Interval seconds for a dropdown index. Out-of-range indices (cannot
/// happen through the UI) fall back to daily.
pub(crate) fn shuffle_interval_secs(index: usize) -> u32 {
    SHUFFLE_INTERVAL_SECS.get(index).copied().unwrap_or(86_400)
}

/// Dropdown index for a stored retention value. Unknown values
/// (hand-edited config) display as the 8-day default rather than crashing
/// or showing an empty selection.
pub(crate) fn retention_index(days: u16) -> usize {
    RETENTION_DAYS
        .iter()
        .position(|&d| d == days)
        .unwrap_or(RETENTION_DEFAULT_INDEX)
}

/// Retention days for a dropdown index. Out-of-range indices (cannot
/// happen through the UI) fall back to the 8-day default.
pub(crate) fn retention_days(index: usize) -> u16 {
    RETENTION_DAYS
        .get(index)
        .copied()
        .unwrap_or(RETENTION_DAYS[RETENTION_DEFAULT_INDEX])
}

// ---------------------------------------------------------------------------
// View (exempt from unit tests per plan)
// ---------------------------------------------------------------------------

/// The whole popup body, wrapped by the caller in
/// `core.applet.popup_container(..)`.
pub(crate) fn popup_view(window: &Window) -> Element<'_, Message> {
    let space = cosmic::theme::spacing();
    let mut content = widget::Column::new().padding([space.space_xxs, 0]);

    if let Some(entry) = displayed(&window.catalogue, window.current.as_deref()) {
        content = content
            .push(padded_control(thumbnail(entry)))
            .push(padded_control(header(entry)));
        if !entry.copyrightlink.is_empty() {
            content = content.push(
                menu_button(widget::text::body("About this image"))
                    .on_press(Message::Open(entry.copyrightlink.clone())),
            );
        }
        content = content
            .push(padded_control(controls(window)).align_x(Alignment::Center))
            .push(padded_control(widget::divider::horizontal::default()))
            .push(padded_control(shuffle_toggler(window)));
        if window.config.shuffle_enabled {
            content = content.push(padded_control(interval_row(window)));
        }
        content = content
            .push(padded_control(widget::divider::horizontal::default()))
            .push(padded_control(retention_row(window)));
    } else {
        // Empty catalogue: everything except the status footer is gone;
        // refresh stays reachable so an offline start can be retried by
        // hand once the network is back.
        content = content
            .push(padded_control(refresh_button(window)).align_x(Alignment::Center))
            .push(padded_control(widget::divider::horizontal::default()));
    }

    content = content.push(padded_control(widget::text::caption(window.status_line())));
    window.core.applet.popup_container(content).into()
}

/// Clickable thumbnail of the displayed wallpaper (cached 480×270 — the UI
/// never decodes the full ~5 MB UHD file). Click opens the full image in
/// the default viewer.
fn thumbnail(entry: &ImageEntry) -> Element<'_, Message> {
    let thumb = thumbs::thumbnail_path(&entry.filename, &app::state_dir());
    let inner: Element<'_, Message> = if thumb.is_file() {
        widget::image(widget::image::Handle::from_path(thumb))
            .width(Length::Fill)
            .into()
    } else {
        widget::container(widget::icon::from_name("image-x-generic-symbolic").size(64))
            .width(Length::Fill)
            .height(Length::Fixed(PLACEHOLDER_HEIGHT))
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
    };
    widget::button::custom_image_button(inner, None)
        .class(cosmic::theme::Button::Image)
        .on_press(Message::Open(entry.filename.display().to_string()))
        .into()
}

/// Title heading + dimmed copyright caption.
fn header(entry: &ImageEntry) -> Element<'_, Message> {
    let mut column = widget::Column::new()
        .spacing(2)
        .push(widget::text::title4(display_title(entry)));
    if !entry.copyright.is_empty() {
        column = column.push(widget::text::caption(entry.copyright.as_str()));
    }
    column.into()
}

/// Control row: ← prev · next → · ⇥ newest · ⟳ refresh now.
fn controls(window: &Window) -> Element<'_, Message> {
    let space = cosmic::theme::spacing();
    let catalogue = &window.catalogue;
    let current = window.current.as_deref();
    let apply =
        |target: Option<&ImageEntry>| target.map(|e| Message::ApplyImage(e.filename.clone()));

    widget::Row::new()
        .push(nav_button(
            "go-previous-symbolic",
            apply(prev_target(catalogue, current)),
        ))
        .push(nav_button(
            "go-next-symbolic",
            apply(next_target(catalogue, current)),
        ))
        .push(nav_button(
            "go-last-symbolic",
            apply(newest_target(catalogue, current)),
        ))
        .push(refresh_button(window))
        .spacing(space.space_s)
        .into()
}

/// Shuffle toggler row (idiom per cosmic-applet-tiling's toggler rows).
fn shuffle_toggler(window: &Window) -> Element<'_, Message> {
    widget::toggler(window.config.shuffle_enabled)
        .on_toggle(Message::SetShuffleEnabled)
        .text_size(14)
        .width(Length::Fill)
        .label("Shuffle".to_owned())
        .into()
}

/// "Every <interval>" dropdown row, shown while shuffle is on. Uses
/// `popup_dropdown` (as in libcosmic's own applet example) so the menu
/// opens as its own wayland popup instead of an overlay clipped to the
/// applet popup's bounds.
fn interval_row(window: &Window) -> Element<'_, Message> {
    widget::Row::new()
        .push(widget::text::body("Every").width(Length::Fill))
        .push(widget::dropdown::popup_dropdown(
            SHUFFLE_INTERVAL_LABELS,
            Some(shuffle_interval_index(window.config.shuffle_interval_secs)),
            Message::SetShuffleInterval,
            window.popup.unwrap_or(cosmic::iced::window::Id::NONE),
            Message::Surface,
            |message| message,
        ))
        .align_y(Alignment::Center)
        .into()
}

/// "Keep images <retention>" dropdown row. Same `popup_dropdown` idiom as
/// the shuffle interval (the menu opens as its own wayland popup).
fn retention_row(window: &Window) -> Element<'_, Message> {
    widget::Row::new()
        .push(widget::text::body("Keep images").width(Length::Fill))
        .push(widget::dropdown::popup_dropdown(
            RETENTION_LABELS,
            Some(retention_index(window.config.retention_days)),
            Message::SetRetention,
            window.popup.unwrap_or(cosmic::iced::window::Id::NONE),
            Message::Surface,
            |message| message,
        ))
        .align_y(Alignment::Center)
        .into()
}

/// Refresh-now button, disabled (no message) while a fetch is running.
fn refresh_button(window: &Window) -> Element<'_, Message> {
    nav_button(
        "view-refresh-symbolic",
        (!window.refresh_pending).then_some(Message::RefreshNow),
    )
}

/// One icon button; `None` renders it disabled.
fn nav_button<'a>(icon: &'static str, on_press: Option<Message>) -> Element<'a, Message> {
    widget::button::icon(widget::icon::from_name(icon))
        .on_press_maybe(on_press)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};
    use std::path::PathBuf;

    fn entry(startdate: &str, name: &str) -> ImageEntry {
        ImageEntry {
            urlbase: format!("/th?id=OHR.{name}"),
            startdate: startdate.to_owned(),
            fullstartdate: format!("{startdate}0700"),
            title: format!("Title {name}"),
            copyright: "© Someone".to_owned(),
            copyrightlink: "https://example.com".to_owned(),
            filename: PathBuf::from(format!("/pics/{startdate}-{name}_UHD.jpg")),
        }
    }

    fn catalogue3() -> (Catalogue, ImageEntry, ImageEntry, ImageEntry) {
        let a = entry("20260805", "A_ROW1");
        let b = entry("20260806", "B_ROW2");
        let c = entry("20260807", "C_ROW3");
        let cat = Catalogue {
            images: vec![a.clone(), b.clone(), c.clone()],
        };
        (cat, a, b, c)
    }

    #[test]
    fn displayed_prefers_current_falls_back_to_newest() {
        let (cat, _, b, c) = catalogue3();
        // Current is ours → that entry.
        assert_eq!(displayed(&cat, Some(&b.filename)), Some(&b));
        // Foreign wallpaper → newest.
        assert_eq!(displayed(&cat, Some(Path::new("/else.jpg"))), Some(&c));
        // No current at all → newest.
        assert_eq!(displayed(&cat, None), Some(&c));
        // Nothing downloaded → nothing to show.
        assert_eq!(displayed(&Catalogue::default(), None), None);
    }

    #[test]
    fn prev_walks_older_and_stops_at_the_oldest_end() {
        let (cat, a, b, _) = catalogue3();
        assert_eq!(prev_target(&cat, Some(&b.filename)), Some(&a));
        assert_eq!(prev_target(&cat, Some(&a.filename)), None); // oldest end
    }

    #[test]
    fn prev_from_a_foreign_wallpaper_enters_at_newest() {
        let (cat, _, _, c) = catalogue3();
        assert_eq!(prev_target(&cat, Some(Path::new("/else.jpg"))), Some(&c));
        assert_eq!(prev_target(&cat, None), Some(&c));
        assert_eq!(prev_target(&Catalogue::default(), None), None);
    }

    #[test]
    fn next_walks_newer_and_stops_at_the_newest_end() {
        let (cat, _, b, c) = catalogue3();
        assert_eq!(next_target(&cat, Some(&b.filename)), Some(&c));
        assert_eq!(next_target(&cat, Some(&c.filename)), None); // newest end
        // Foreign wallpaper: "next" has no meaning.
        assert_eq!(next_target(&cat, Some(Path::new("/else.jpg"))), None);
        assert_eq!(next_target(&cat, None), None);
    }

    #[test]
    fn newest_disabled_only_when_already_applied() {
        let (cat, _, b, c) = catalogue3();
        assert_eq!(newest_target(&cat, Some(&b.filename)), Some(&c));
        assert_eq!(newest_target(&cat, Some(Path::new("/else.jpg"))), Some(&c));
        assert_eq!(newest_target(&cat, Some(&c.filename)), None); // applied
        assert_eq!(newest_target(&Catalogue::default(), None), None);
    }

    #[test]
    fn display_title_falls_back_to_file_stem_for_rebuilt_entries() {
        let real = entry("20260807", "Foo_ROW1");
        assert_eq!(display_title(&real), "Title Foo_ROW1");

        let mut rebuilt = real;
        rebuilt.title = String::new();
        assert_eq!(display_title(&rebuilt), "20260807-Foo_ROW1_UHD");
    }

    fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, 0)
            .unwrap()
    }

    #[test]
    fn format_updated_today_yesterday_older() {
        let now = at(2026, 8, 7, 18, 0);
        assert_eq!(
            format_updated(at(2026, 8, 7, 9, 12), now),
            "Updated today at 09:12"
        );
        assert_eq!(
            format_updated(at(2026, 8, 6, 23, 59), now),
            "Updated yesterday at 23:59"
        );
        assert_eq!(
            format_updated(at(2026, 8, 5, 7, 5), now),
            "Updated Aug 5 at 07:05"
        );
    }

    #[test]
    fn shuffle_interval_mapping_roundtrips() {
        // The plan's exact choices, in dropdown order.
        assert_eq!(SHUFFLE_INTERVAL_SECS, [1_800, 3_600, 21_600, 86_400]);
        assert_eq!(SHUFFLE_INTERVAL_LABELS.len(), SHUFFLE_INTERVAL_SECS.len());
        for (index, &secs) in SHUFFLE_INTERVAL_SECS.iter().enumerate() {
            assert_eq!(shuffle_interval_index(secs), index);
            assert_eq!(shuffle_interval_secs(index), secs);
            // Full roundtrip both ways.
            assert_eq!(shuffle_interval_secs(shuffle_interval_index(secs)), secs);
        }
    }

    #[test]
    fn shuffle_interval_mapping_tolerates_garbage() {
        // Hand-edited config value → displays as the daily default.
        assert_eq!(shuffle_interval_index(1_234), 3);
        assert_eq!(shuffle_interval_index(0), 3);
        // Impossible dropdown index → daily seconds.
        assert_eq!(shuffle_interval_secs(99), 86_400);
    }

    #[test]
    fn retention_mapping_roundtrips() {
        // The plan's exact choices, in dropdown order (0 = forever).
        assert_eq!(RETENTION_DAYS, [3, 8, 30, 0]);
        assert_eq!(RETENTION_LABELS.len(), RETENTION_DAYS.len());
        for (index, &days) in RETENTION_DAYS.iter().enumerate() {
            assert_eq!(retention_index(days), index);
            assert_eq!(retention_days(index), days);
            // Full roundtrip both ways.
            assert_eq!(retention_days(retention_index(days)), days);
        }
    }

    #[test]
    fn retention_mapping_tolerates_garbage() {
        // Hand-edited config value → displays as the 8-day default.
        assert_eq!(retention_index(5), 1);
        assert_eq!(retention_index(9_999), 1);
        // Impossible dropdown index → 8-day default.
        assert_eq!(retention_days(99), 8);
    }

    #[test]
    fn format_updated_month_boundary_counts_calendar_days() {
        // Aug 1 00:10 vs Jul 31 23:50: twenty minutes apart, but a calendar
        // day apart — that's "yesterday", not "today".
        assert_eq!(
            format_updated(at(2026, 7, 31, 23, 50), at(2026, 8, 1, 0, 10)),
            "Updated yesterday at 23:50"
        );
    }
}
