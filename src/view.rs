// The popup's layout (Task 8). `app.rs` keeps state + update; everything
// visual lives here. The iced view code itself is exempt from unit tests
// (verified manually in the panel), but the pure decisions it renders —
// which entry is shown, which navigation targets exist, how the footer
// timestamp reads — are extracted below and tested.

use std::borrow::Cow;
use std::path::Path;

use chrono::NaiveDateTime;
use cosmic::{
    Element,
    applet::{menu_button, padded_control},
    iced::{Alignment, Length, window::Id},
    widget,
};

use crate::app::{self, Message, RefreshError, Window};
use crate::catalogue::{Catalogue, ImageEntry};
use crate::fl;
use crate::thumbs;

/// Height of the placeholder box shown when a thumbnail is missing
/// (e.g. entries rebuilt from a folder scan that never went through a
/// fetch). The UI must never decode the full UHD file to fill this.
///
/// Deliberately not a spacing token: the `Spacing` scale describes paddings
/// and gaps, not content dimensions, and first-party applets size their own
/// content areas with plain numbers too.
const PLACEHOLDER_HEIGHT: f32 = 160.0;

/// Shuffle interval choices, index-aligned with
/// [`shuffle_interval_labels`].
const SHUFFLE_INTERVAL_SECS: [u32; 4] = [1_800, 3_600, 21_600, 86_400];

/// Dropdown labels for the shuffle intervals, index-aligned with
/// [`SHUFFLE_INTERVAL_SECS`].
///
/// Built per call rather than cached in a `LazyLock`: a static would freeze
/// the labels in whatever language was loaded when it was first touched,
/// which is the wrong one if that happens before `localize::localize()`.
fn shuffle_interval_labels() -> Vec<String> {
    vec![
        fl!("interval-30-minutes"),
        fl!("interval-1-hour"),
        fl!("interval-6-hours"),
        fl!("interval-daily"),
    ]
}

/// Dropdown index of the default shuffle interval (daily) — the fallback
/// for hand-edited config values that match no choice.
const SHUFFLE_DEFAULT_INDEX: usize = SHUFFLE_INTERVAL_SECS.len() - 1;

/// Retention choices in days (0 = keep forever), index-aligned with
/// [`retention_labels`]. Shared with `AppletConfig::normalize` so a loaded
/// value the dropdown cannot display never drives prune/fetch behavior.
const RETENTION_DAYS: [u16; 4] = crate::config::RETENTION_CHOICES;

/// Dropdown labels for the retention choices, index-aligned with
/// [`RETENTION_DAYS`] (see [`shuffle_interval_labels`] on why this is a
/// function).
fn retention_labels() -> Vec<String> {
    vec![
        fl!("retention-3-days"),
        fl!("retention-8-days"),
        fl!("retention-30-days"),
        fl!("retention-forever"),
    ]
}

/// Dropdown index of the default retention (8 days) — the fallback for
/// hand-edited config values that match no choice.
const RETENTION_DEFAULT_INDEX: usize = 1;

/// Opacity applied to a *disabled* icon button's glyph.
///
/// The theme's own disabled appearance is a no-op for `Button::Icon`, so this
/// has to be explicit (verified against libcosmic rev `8a017a1`):
/// * `icon_button.on` and `.on_disabled` are the **same RGB** colour
///   (`control_steps_array[8]`), differing only in alpha (1.0 vs 0.65 — see
///   `Component::component`, `cosmic-theme/src/model/derivation.rs`), and the
///   SVG rasteriser tints RGB only, keeping each pixel's source alpha
///   (`iced/wgpu/src/image/vector.rs:173`). The alpha delta is discarded.
/// * the disabled background tweak (`background.a *= 0.5`,
///   `theme/style/button.rs`) is also a no-op because `icon_button.base` is
///   fully transparent.
///
/// `Icon::opacity` survives to the renderer as its own uniform
/// (`iced/wgpu/src/image/mod.rs:348`), so it is the one lever that works.
const DISABLED_ICON_OPACITY: f32 = 0.4;

// ---------------------------------------------------------------------------
// Pure decisions (tested)
// ---------------------------------------------------------------------------

/// The entry the popup presents: the currently applied file when it is one
/// of ours, otherwise the newest downloaded image (the user's own wallpaper
/// cannot be thumbnailed without decoding it — accepted, matches the
/// reference which also only previews its own images).
pub fn displayed<'a>(catalogue: &'a Catalogue, current: Option<&Path>) -> Option<&'a ImageEntry> {
    current
        .and_then(|c| catalogue.images.iter().find(|e| e.filename == c))
        .or_else(|| catalogue.newest())
}

/// Target for the "previous" button. Browsing our history walks one step
/// older; a foreign (or unknown) current wallpaper enters the history at
/// its newest end. `None` = disabled (oldest end, or nothing downloaded).
pub fn prev_target<'a>(catalogue: &'a Catalogue, current: Option<&Path>) -> Option<&'a ImageEntry> {
    match current {
        Some(c) if catalogue.contains(c) => catalogue.prev(c),
        _ => catalogue.newest(),
    }
}

/// Target for the "next" button: one step newer, only meaningful while the
/// current wallpaper is inside our history. `None` = disabled (newest end,
/// foreign wallpaper, or nothing downloaded).
pub fn next_target<'a>(catalogue: &'a Catalogue, current: Option<&Path>) -> Option<&'a ImageEntry> {
    current
        .filter(|c| catalogue.contains(c))
        .and_then(|c| catalogue.next(c))
}

/// Target for the "jump to newest" button. `None` = disabled (already
/// applied, or nothing downloaded).
pub fn newest_target<'a>(
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
pub fn display_title(entry: &ImageEntry) -> String {
    if !entry.title.is_empty() {
        return entry.title.clone();
    }
    entry
        .filename
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| fl!("bing-wallpaper"))
}

/// Footer timestamp: "Updated today at 09:12" / "… yesterday at …" /
/// "Updated Aug 5 at 09:12". Both instants are local wall-clock time.
pub fn format_updated(updated: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = updated.format("%H:%M").to_string();
    if updated.date() == now.date() {
        fl!("status-updated-today", time = time)
    } else if now.date().pred_opt() == Some(updated.date()) {
        fl!("status-updated-yesterday", time = time)
    } else {
        // Only the sentence frame is localized; the date itself stays English
        // month abbreviations (locale-aware dates would mean pulling in ICU).
        let date = updated.format("%b %-d").to_string();
        fl!("status-updated-on", date = date, time = time)
    }
}

/// Dropdown index for a stored interval. Unknown values (hand-edited
/// config) display as the daily default rather than crashing or showing
/// an empty selection.
pub fn shuffle_interval_index(secs: u32) -> usize {
    SHUFFLE_INTERVAL_SECS
        .iter()
        .position(|&s| s == secs)
        .unwrap_or(SHUFFLE_DEFAULT_INDEX)
}

/// Interval seconds for a dropdown index. Out-of-range indices (cannot
/// happen through the UI) fall back to daily.
pub fn shuffle_interval_secs(index: usize) -> u32 {
    SHUFFLE_INTERVAL_SECS
        .get(index)
        .copied()
        .unwrap_or(SHUFFLE_INTERVAL_SECS[SHUFFLE_DEFAULT_INDEX])
}

/// Dropdown index for a stored retention value. Unknown values
/// (hand-edited config) display as the 8-day default rather than crashing
/// or showing an empty selection.
pub fn retention_index(days: u16) -> usize {
    RETENTION_DAYS
        .iter()
        .position(|&d| d == days)
        .unwrap_or(RETENTION_DEFAULT_INDEX)
}

/// Retention days for a dropdown index. Out-of-range indices (cannot
/// happen through the UI) fall back to the 8-day default.
pub fn retention_days(index: usize) -> u16 {
    RETENTION_DAYS
        .get(index)
        .copied()
        .unwrap_or(RETENTION_DAYS[RETENTION_DEFAULT_INDEX])
}

/// Opacity for an icon button's glyph: full while it can be pressed, dimmed
/// once it cannot (see [`DISABLED_ICON_OPACITY`] for why the theme cannot do
/// this for us).
pub fn icon_opacity(enabled: bool) -> f32 {
    if enabled { 1.0 } else { DISABLED_ICON_OPACITY }
}

/// Status footer text.
pub fn status_line(window: &Window) -> String {
    if window.refresh_pending {
        fl!("status-checking")
    } else if let Some(error) = &window.last_error {
        match error {
            RefreshError::Network(_) => fl!("status-network-error"),
            RefreshError::Disk(_) => fl!("status-disk-error"),
        }
    } else if let Some(updated) = window.last_updated {
        format_updated(
            updated.with_timezone(&chrono::Local).naive_local(),
            chrono::Local::now().naive_local(),
        )
    } else if window.catalogue.images.is_empty() {
        fl!("status-no-images")
    } else {
        // Restored from the catalogue; no fetch has completed yet this
        // session.
        fl!("status-up-to-date")
    }
}

// ---------------------------------------------------------------------------
// View (exempt from unit tests per plan)
// ---------------------------------------------------------------------------

/// The whole popup body, wrapped by the caller in
/// `core.applet.popup_container(..)`.
pub fn popup_view(window: &Window) -> Element<'_, Message> {
    let space = cosmic::theme::spacing();
    let mut content = widget::Column::new().padding([space.space_xxs, 0]);

    if let Some(entry) = displayed(&window.catalogue, window.current.as_deref()) {
        content = content
            .push(padded_control(thumbnail(window, entry)))
            .push(padded_control(header(entry)));
        if !entry.copyrightlink.is_empty() {
            content = content.push(
                menu_button(widget::text::body(fl!("about-this-image")))
                    .on_press(Message::OpenUrl(entry.copyrightlink.clone())),
            );
        }
        content = content
            .push(padded_control(controls(window)).align_x(Alignment::Center))
            .push(divider())
            .push(padded_control(shuffle_toggler(window)));
        if window.config.shuffle_enabled {
            content = content.push(padded_control(interval_row(window)));
        }
        content = content
            .push(divider())
            .push(padded_control(retention_row(window)));
    } else {
        // Empty catalogue: everything except the status footer is gone;
        // refresh stays reachable so an offline start can be retried by
        // hand once the network is back.
        content = content
            .push(padded_control(refresh_button(window)).align_x(Alignment::Center))
            .push(divider());
    }

    content = content.push(padded_control(widget::text::caption(status_line(window))));
    window.core.applet.popup_container(content).into()
}

/// A separator between popup sections.
///
/// `padded_control` alone would inset it by `menu_control_padding()`
/// (`[space_xxs, space_m]`); every first-party applet overrides the
/// horizontal inset to `space_s` so the rule reaches closer to the popup
/// edge than the controls do (cosmic-applet-tiling `window.rs`, all four
/// dividers). Matching that is the whole point of the override.
fn divider<'a>() -> Element<'a, Message> {
    let space = cosmic::theme::spacing();
    padded_control(widget::divider::horizontal::default())
        .padding([space.space_xxs, space.space_s])
        .into()
}

/// Clickable thumbnail of the displayed wallpaper (cached 480×270 — the UI
/// never decodes the full ~5 MB UHD file). Click opens the full image in
/// the default viewer.
fn thumbnail<'a>(window: &'a Window, entry: &'a ImageEntry) -> Element<'a, Message> {
    let thumb = thumbs::thumbnail_path(&entry.filename, app::state_dir());
    let inner: Element<'_, Message> = if thumb.is_file() {
        // `theme::Button::Image` rounds the button's border to
        // `corner_radii.radius_s`, but nothing rounds the image inside it —
        // libcosmic's own `button::image` rounds the handle itself (with a
        // hardcoded `[9.0; 4]`; the token is the same corner, and follows the
        // user's corner-radius setting).
        widget::image(widget::image::Handle::from_path(thumb))
            .border_radius(cosmic::theme::active().cosmic().corner_radii.radius_s)
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
    let button = widget::button::custom_image_button(inner, None)
        .class(cosmic::theme::Button::Image)
        .on_press(Message::OpenFile(entry.filename.clone()));
    popup_tooltip(window, button, fl!("tooltip-open-image"))
}

/// Title heading + dimmed copyright caption.
fn header(entry: &ImageEntry) -> Element<'_, Message> {
    // `space_xxxs` is the stock label-column spacing (cosmic-applet-tiling's
    // "new workspace" column); the previous hardcoded `2` matched no token.
    let space = cosmic::theme::spacing();
    let mut column = widget::Column::new()
        .spacing(space.space_xxxs)
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
            window,
            "go-previous-symbolic",
            fl!("tooltip-previous"),
            apply(prev_target(catalogue, current)),
        ))
        .push(nav_button(
            window,
            "go-next-symbolic",
            fl!("tooltip-next"),
            apply(next_target(catalogue, current)),
        ))
        .push(nav_button(
            window,
            "go-last-symbolic",
            fl!("tooltip-newest"),
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
        .label(fl!("shuffle"))
        .into()
}

/// "Every <interval>" dropdown row, shown while shuffle is on. Uses
/// `popup_dropdown` (as in libcosmic's own applet example) so the menu
/// opens as its own wayland popup instead of an overlay clipped to the
/// applet popup's bounds.
fn interval_row(window: &Window) -> Element<'_, Message> {
    widget::Row::new()
        .push(widget::text::body(fl!("shuffle-every")).width(Length::Fill))
        .push(widget::dropdown::popup_dropdown(
            shuffle_interval_labels(),
            Some(shuffle_interval_index(window.config.shuffle_interval_secs)),
            Message::SetShuffleInterval,
            window.popup.unwrap_or(Id::NONE),
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
        .push(widget::text::body(fl!("keep-images")).width(Length::Fill))
        .push(widget::dropdown::popup_dropdown(
            retention_labels(),
            Some(retention_index(window.config.retention_days)),
            Message::SetRetention,
            window.popup.unwrap_or(Id::NONE),
            Message::Surface,
            |message| message,
        ))
        .align_y(Alignment::Center)
        .into()
}

/// Refresh-now button, disabled (no message) while a fetch is running.
fn refresh_button(window: &Window) -> Element<'_, Message> {
    nav_button(
        window,
        "view-refresh-symbolic",
        fl!("tooltip-refresh"),
        (!window.refresh_pending).then_some(Message::RefreshNow),
    )
}

/// One icon button with a hover tooltip; `None` renders it disabled — and
/// *visibly* so, via [`icon_opacity`].
///
/// This open-codes what `widget::button::icon` builds internally (a padded
/// row holding the glyph, wrapped in `button::custom` with
/// `theme::Button::Icon`) because that constructor takes a bare `Handle` and
/// gives no way to reach the `Icon`'s opacity.
fn nav_button<'a>(
    window: &Window,
    icon: &'static str,
    tooltip: impl Into<Cow<'static, str>>,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let space = cosmic::theme::spacing();
    let glyph = widget::icon::from_name(icon)
        .icon()
        .opacity(icon_opacity(on_press.is_some()));
    let button = widget::button::custom(
        widget::Row::new()
            .push(glyph)
            .padding(space.space_xxs)
            .align_y(Alignment::Center),
    )
    .padding(0)
    .class(cosmic::theme::Button::Icon)
    .on_press_maybe(on_press);
    popup_tooltip(window, button, tooltip)
}

/// Wrap a control living *inside* the applet popup in a hover tooltip.
///
/// Uses `applet_tooltip` (a real wayland popup) rather than the plain
/// `widget::tooltip` overlay, which would clip to the popup surface — the
/// same reason the dropdowns use `popup_dropdown` (see CLAUDE.md).
/// `has_popup: false` is required: `applet_tooltip` only builds the tooltip
/// surface when it is `false`, and `parent_id` must be *our* popup so the
/// tooltip parents to it instead of the panel.
fn popup_tooltip<'a>(
    window: &Window,
    content: impl Into<Element<'a, Message>>,
    tooltip: impl Into<Cow<'static, str>>,
) -> Element<'a, Message> {
    window
        .core
        .applet
        .applet_tooltip::<Message>(content, tooltip, false, Message::Surface, window.popup)
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
        // Labels come from the FTL now; pin the English copy so a botched
        // catalogue edit fails here rather than in the panel.
        assert_eq!(
            shuffle_interval_labels(),
            ["30 minutes", "1 hour", "6 hours", "Daily"]
        );
        assert_eq!(shuffle_interval_labels().len(), SHUFFLE_INTERVAL_SECS.len());
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
    }

    #[test]
    fn retention_mapping_roundtrips() {
        // The plan's exact choices, in dropdown order (0 = forever).
        assert_eq!(RETENTION_DAYS, [3, 8, 30, 0]);
        assert_eq!(
            retention_labels(),
            ["3 days", "8 days", "30 days", "Forever"]
        );
        assert_eq!(retention_labels().len(), RETENTION_DAYS.len());
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
    }

    #[test]
    fn disabled_icons_are_visibly_dimmed() {
        // An enabled glyph must stay untouched, a disabled one must be dim
        // enough to read as disabled at a glance — the theme's own disabled
        // styling is invisible for `Button::Icon` (see DISABLED_ICON_OPACITY).
        assert_eq!(icon_opacity(true), 1.0);
        let disabled = icon_opacity(false);
        assert!(
            (0.2..=0.6).contains(&disabled),
            "disabled opacity {disabled} is not a clear visual difference"
        );
        assert!(disabled < icon_opacity(true));
    }

    #[test]
    fn status_line_reflects_the_fetch_lifecycle() {
        let mut window = Window::default();

        // Fresh cold start: nothing on disk, nothing fetched yet.
        assert_eq!(status_line(&window), "No images yet — fetching…");

        // Pipeline running.
        window.refresh_pending = true;
        assert_eq!(status_line(&window), "Checking for new images…");
        window.refresh_pending = false;

        // Fetch failed → the plan's exact error footer for network trouble…
        window.last_error = Some(RefreshError::Network("boom".to_owned()));
        assert_eq!(status_line(&window), "Bing unreachable — retrying in 1 h");
        // …while a local I/O failure is not blamed on Bing.
        window.last_error = Some(RefreshError::Disk("disk full".to_owned()));
        assert_eq!(status_line(&window), "Disk error — retrying in 1 h");

        // Success clears the error and records the time (relative wording
        // itself is covered by `format_updated`'s tests; only the prefix is
        // asserted here so the test cannot flake across a local midnight
        // between the two `now()` reads).
        window.last_error = None;
        window.last_updated = Some(chrono::Utc::now());
        assert!(status_line(&window).starts_with("Updated"));
    }

    #[test]
    fn status_line_restored_catalogue_without_fetch_is_up_to_date() {
        let mut window = Window::default();
        window.catalogue.images.push(entry("20260807", "Foo_ROW1"));
        assert_eq!(status_line(&window), "Up to date");
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

    #[test]
    fn format_updated_year_boundary_counts_calendar_days() {
        // Dec 31 → Jan 1 crosses the year: still "yesterday".
        assert_eq!(
            format_updated(at(2026, 12, 31, 23, 50), at(2027, 1, 1, 0, 10)),
            "Updated yesterday at 23:50"
        );
    }
}
