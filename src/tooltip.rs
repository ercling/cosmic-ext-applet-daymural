// Hover tooltips, as real wayland popups with a *distinguishable* surface.
//
// Why this module exists at all: `Core::applet_tooltip` (libcosmic
// `src/applet/mod.rs`) hardcodes its popup content as
// `layer_container(text).layer(Layer::Background)`, i.e. `Container::Background`
// — background `base`, no border. That is the very same fill
// `Core::popup_container` paints behind our popup (`background(..).base` plus a
// 1 px divider border), so a tooltip opened over the popup was an invisible box:
// the label sat on the applet surface with nothing to separate the two.
// libcosmic's *own* non-wayland `widget::tooltip` does not look like that — it
// puts the label on a raised neutral chip (`Container::Tooltip`).
//
// So the widget below is `applet_tooltip`'s popup plumbing (same positioner,
// same 100 ms delay, same single reused surface id) with a chip of our own —
// see [`surface`] and [`chip_fill`]. Keep the positioner in sync with upstream
// if the pinned rev moves; it is copied deliberately, since the popup content
// (hence its style) is not injectable.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use cosmic::applet::cosmic_panel_config::PanelAnchor;
use cosmic::cctk::sctk::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor, Gravity,
};
use cosmic::iced::runtime::platform_specific::wayland::popup::{SctkPopupSettings, SctkPositioner};
use cosmic::iced::{Limits, Point, Rectangle, Size, window};
use cosmic::{Element, widget};

use crate::app::Message;

/// The one tooltip surface. Only one tooltip is ever visible at a time (a
/// pointer has one position), so — as upstream does — every call site shares
/// this window id and hands it back in the `on_leave` message that destroys
/// the popup.
static WINDOW_ID: LazyLock<window::Id> = LazyLock::new(window::Id::unique);

/// The one tooltip surface id (see [`WINDOW_ID`]).
///
/// `app.rs` needs it to keep its popup ledger: a `PopupClosed` naming this id
/// is the tooltip's, anything else parented to our popup is a dropdown menu
/// (whose id is minted inside the widget and cannot be observed from here).
pub fn window_id() -> window::Id {
    *WINDOW_ID
}

/// Autosize id of the tooltip surface's root element: per-surface bookkeeping,
/// so a single shared constant is right (the dropdown surfaces carry their own).
static SURFACE_ID: LazyLock<widget::Id> = LazyLock::new(|| widget::Id::new("tooltip-surface"));

/// libcosmic's tooltip delay.
const DELAY: Duration = Duration::from_millis(100);

/// Wrap `content` in a hover tooltip reading `label`.
///
/// `parent_id` is the surface the tooltip belongs to: our popup, for the
/// controls that live inside it. Parenting it to the panel (`None`) instead
/// makes the tooltip appear detached from the control it describes — that is
/// upstream's arrangement for a *panel button*, which this applet no longer has
/// a tooltip on (see `app::Window::view`).
///
/// `suppressed` gates the popup the same way upstream's `has_popup` flag does
/// (`Core::applet_tooltip` passes `(!has_popup).then_some(..)` as the widget's
/// `settings`), but for a different reason: upstream hides a *panel button's*
/// tooltip while its popup is open, whereas here it keeps a tooltip from arming
/// while a dropdown menu is mapped. Both would be children of the same popup,
/// i.e. siblings on one xdg-shell stack, and only the topmost of a stack may be
/// destroyed — see the popup ledger in `app.rs`. Note it withholds the
/// *settings closure*, not the widget: the wrapped button stays in the tree with
/// its state intact and simply can never create a popup.
pub fn tooltip<'a>(
    core: &cosmic::Core,
    content: impl Into<Element<'a, Message>>,
    label: impl Into<Cow<'static, str>>,
    parent_id: Option<window::Id>,
    suppressed: bool,
) -> Element<'a, Message> {
    let window_id = *WINDOW_ID;
    let (popup_anchor, gravity) = away_from_panel(core.applet.anchor);
    let label = label.into();

    widget::wayland::tooltip::widget::Tooltip::<Message, Message>::new(
        content,
        (!suppressed).then_some(move |bounds: Rectangle| SctkPopupSettings {
            parent: parent_id.unwrap_or(window::Id::RESERVED),
            id: window_id,
            grab: false,
            // Off-screen input zone: the tooltip must not eat pointer events,
            // or hovering it would count as leaving the control it describes.
            input_zone: Some(vec![Rectangle::new(
                Point::new(-1000., -1000.),
                Size::default(),
            )]),
            positioner: SctkPositioner {
                size: None,
                size_limits: Limits::NONE.min_width(1.).min_height(1.),
                anchor_rect: Rectangle {
                    x: bounds.x.round() as i32,
                    y: bounds.y.round() as i32,
                    width: bounds.width.round() as i32,
                    height: bounds.height.round() as i32,
                },
                anchor: popup_anchor,
                gravity,
                constraint_adjustment: 15,
                offset: (0, 0),
                reactive: true,
            },
            parent_size: None,
            close_with_children: true,
        }),
        move || {
            Element::from(widget::autosize::autosize(
                surface(label.clone()),
                SURFACE_ID.clone(),
            ))
        },
        // Both go to `TooltipSurface`, not a blind `Surface` forwarder: the
        // popup ledger in `app.rs` has to see everything this widget publishes
        // to hold the single-child invariant on our popup — while a dropdown
        // menu is mapped it drops all of it, because a tooltip and a menu are
        // siblings there and destroying a non-topmost sibling is a fatal
        // protocol error.
        Message::TooltipSurface(cosmic::surface::Action::DestroyPopup(window_id)),
        Message::TooltipSurface,
    )
    .delay(DELAY)
    .into()
}

/// Anchor + gravity that push the tooltip away from the panel edge, so it
/// never opens underneath the panel it is anchored to.
fn away_from_panel(anchor: PanelAnchor) -> (Anchor, Gravity) {
    match anchor {
        PanelAnchor::Left => (Anchor::Right, Gravity::Right),
        PanelAnchor::Right => (Anchor::Left, Gravity::Left),
        PanelAnchor::Top => (Anchor::Bottom, Gravity::Bottom),
        PanelAnchor::Bottom => (Anchor::Top, Gravity::Top),
    }
}

/// The tooltip surface's content: the label on its own raised chip.
///
/// Shaped like libcosmic's `Container::Tooltip` (the class `widget::tooltip`
/// gives every non-applet tooltip in COSMIC: a flat neutral fill on `radius_l`
/// corners) with three deliberate differences:
///
/// * the fill is two palette steps off the background layer *in the direction
///   the theme has room for* (see [`chip_fill`]), not that class's fixed
///   `neutral_2`. A stock tooltip floats over *app* content — a component
///   surface a step away from the background layer — while ours floats over the
///   applet popup, which **is** the background layer (`background(..).base`).
///   Against that, `neutral_2` lands at 1.29:1 in the light palette and
///   1.05:1 in the dark one, i.e. the bug being fixed.
/// * the 1 px divider border `Core::popup_container` draws around the popup,
///   so the chip keeps a crisp edge wherever it lands.
/// * an explicit text/icon colour. `Container::Tooltip` leaves those `None`,
///   i.e. inherited from the surface's renderer style — which for us is the
///   applet's `on_bg_color`. Pinning it to the background layer's `on` keeps
///   the label readable against the chip no matter what a theme does to the
///   applet style.
fn surface(label: Cow<'static, str>) -> Element<'static, cosmic::Action<Message>> {
    let space = cosmic::theme::spacing();
    widget::container(widget::text(label))
        .class(cosmic::theme::Container::custom(chip_style))
        .padding(space.space_xxs)
        .into()
}

/// Style of the tooltip chip — see [`surface`]. Split out so the property that
/// broke can be asserted directly (`the_chip_never_wears_the_popups_own_colour`).
fn chip_style(theme: &cosmic::Theme) -> widget::container::Style {
    let cosmic = theme.cosmic();
    let on = cosmic.background(theme.transparent).on;
    widget::container::Style {
        icon_color: Some(on.into()),
        text_color: Some(on.into()),
        background: Some(cosmic::iced::Background::Color(chip_fill(theme))),
        border: cosmic::iced::Border {
            radius: cosmic.corner_radii.radius_l.into(),
            width: 1.0,
            color: cosmic.background(theme.transparent).divider.into(),
        },
        shadow: cosmic::iced::Shadow::default(),
        snap: true,
    }
}

/// The chip's fill: two steps off the background layer along the neutral ramp,
/// in whichever direction the theme has room for.
///
/// The ramp runs dark → light (`neutral_0` black … `neutral_10` white) while
/// the background layer sits at opposite ends of it in the two modes: the
/// built-in light palette puts `background.base` at 0.843 *above* `neutral_2`
/// (0.745), the dark one at 0.106 *below* it (0.086). So a fixed palette index
/// cannot serve both — one of them always collapses onto the popup's own
/// colour. Stepping away instead lands the chip at ~1.9:1 against the popup in
/// both, with the label at ~7:1 on top of it.
fn chip_fill(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let palette = &theme.cosmic().palette;
    if theme.theme_type.is_dark() {
        palette.neutral_4.into()
    } else {
        palette.neutral_3.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::{Background, Color};

    /// Every theme the built-in palettes cover.
    fn themes() -> [cosmic::Theme; 4] {
        [
            cosmic::Theme::light(),
            cosmic::Theme::dark(),
            cosmic::Theme::light_hc(),
            cosmic::Theme::dark_hc(),
        ]
    }

    fn fill_of(style: &widget::container::Style) -> Color {
        match style.background {
            Some(Background::Color(color)) => color,
            other => panic!("the chip must have a flat fill, got {other:?}"),
        }
    }

    /// WCAG relative luminance of an sRGB colour.
    fn luminance(color: Color) -> f32 {
        let channel = |c: f32| {
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// The regression this module exists for: upstream's `applet_tooltip`
    /// painted the tooltip surface in `background(..).base`, the exact fill
    /// `Core::popup_container` puts behind the popup — so a tooltip opening
    /// over the popup showed a label on no visible chip at all.
    ///
    /// Asserts a *visible* step rather than mere inequality: a chip one hair
    /// off the surface under it is the same bug with extra steps. The floor is
    /// 1.5:1 — the stock `Container::Tooltip` fill lands at 1.29:1 (light) and
    /// 1.05:1 (dark) here, so it would not pass, while [`chip_fill`] clears it
    /// with room to spare in both.
    #[test]
    fn the_chip_never_wears_the_popups_own_colour() {
        for theme in themes() {
            let popup = Color::from(theme.cosmic().background(theme.transparent).base);
            let chip = fill_of(&chip_style(&theme));
            let ratio = contrast(chip, popup);
            assert!(
                ratio >= 1.5,
                "tooltip chip is only {ratio:.2}:1 against the popup background in {:?}",
                theme.theme_type
            );
        }
    }

    /// The label is pinned to the background layer's `on` colour rather than
    /// inherited from the surface's renderer style; it has to stay readable on
    /// the chip in every theme.
    #[test]
    fn the_label_is_readable_on_the_chip() {
        for theme in themes() {
            let style = chip_style(&theme);
            let label = style.text_color.expect("the label colour must be pinned");
            let ratio = contrast(label, fill_of(&style));
            assert!(
                ratio >= 4.5,
                "label/chip contrast is {ratio:.2}:1 in {:?}",
                theme.theme_type
            );
        }
    }
}
