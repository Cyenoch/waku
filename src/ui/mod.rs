use gpui::{
    AnyElement, App, Div, ElementId, Hsla, Img, InteractiveElement, Interactivity, ParentElement,
    PathBuilder, Pixels, RenderOnce, ScrollHandle, SharedString, Stateful, StyleRefinement, Styled,
    Svg, Window, canvas, div, img, point, prelude::*, px, rgb, svg,
};

pub mod menu;
pub mod motion;
pub mod scrollbar;
pub mod text_field;
pub mod tooltip;

use crate::model::{ActivityKind, ProviderId, SessionStatus};
use crate::theme::Theme;

/// A monochrome icon from the embedded set, tinted via text color.
pub fn icon(path: &'static str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(path)
        .w(px(size))
        .h(px(size))
        .flex_none()
        .text_color(color)
}

/// A polychrome file icon rendered as an image so the SVG's authored colors
/// are preserved. GPUI's `svg()` element intentionally renders an alpha mask
/// tinted with one text color.
pub fn file_icon(path: &'static str, size: f32) -> Img {
    img(path).w(px(size)).h(px(size)).flex_none()
}

/// A compact ghost icon button: the only button shape outside the composer's
/// bespoke send control.
pub fn icon_button(id: impl Into<ElementId>, path: &'static str, theme: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(22.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .hover(|element| element.bg(theme.overlay))
        .active(|element| element.bg(theme.overlay_strong))
        .child(icon(path, 13.0, theme.text_tertiary))
}

/// Keeps a wheel gesture inside a scrollable nested in another scrollable
/// (activity output inside the transcript list, command output inside the
/// background-work page), matching AppKit: while the viewport under the
/// pointer has overflow of its own, the ancestor must not scroll it away.
/// Call from an `on_scroll_wheel` listener. The viewport's own scroll
/// handler registers after user listeners, so it has already consumed the
/// delta when this stops the bubble; a viewport whose content fits keeps
/// chaining so short blocks do not dead-zone the page. Stopping propagation
/// also skips wheel listeners pushed earlier on the same element, so fold
/// any sibling wheel logic into the listener that calls this.
pub fn contain_scroll(handle: &ScrollHandle, cx: &mut App) {
    if handle.max_offset().y > px(0.5) {
        cx.stop_propagation();
    }
}

const PROVIDER_SERIES_RGB: [u32; 8] = [
    0x00D9_7757,
    0x0010_A37F,
    0x000E_A5E9,
    0x00A8_55F7,
    0x00F5_9E0B,
    0x00EF_4444,
    0x0014_B8A6,
    0x0063_66F1,
];

pub fn provider_series_rgb(provider: &ProviderId) -> u32 {
    let slot = match provider.as_str().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => 0,
        "openai-responses" | "openai" => 1,
        "xai" => 2,
        "opencode-zen" => 3,
        "opencode-go" => 4,
        "xai-oauth" => 5,
        "openai-chat" => 6,
        "openai-codex" => 7,
        id => {
            let mut hash: u32 = 2_166_136_261;
            for byte in id.bytes() {
                hash ^= u32::from(byte);
                hash = hash.wrapping_mul(16_777_619);
            }
            return PROVIDER_SERIES_RGB[(hash as usize) % PROVIDER_SERIES_RGB.len()];
        }
    };
    PROVIDER_SERIES_RGB[slot]
}

/// Brand hue for each provider series on the Usage page.
pub fn provider_color(_theme: &Theme, provider: &ProviderId) -> Hsla {
    rgb(provider_series_rgb(provider)).into()
}

/// Recognizable provider marks, matching the model picker vocabulary. HTTP
/// endpoints are user-configured, so every provider shares the generic mark.
pub fn provider_icon(_provider: &ProviderId) -> &'static str {
    "icons/bot.svg"
}

pub fn status_color(theme: &Theme, status: SessionStatus) -> Hsla {
    match status {
        SessionStatus::Idle => theme.text_ghost,
        SessionStatus::Connecting | SessionStatus::Working => theme.accent,
        SessionStatus::Waiting => theme.warning,
        SessionStatus::Failed => theme.danger,
    }
}

pub fn activity_icon(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Reasoning => "icons/sparkle.svg",
        ActivityKind::Command => "icons/terminal.svg",
        ActivityKind::FileChange => "icons/pencil.svg",
        ActivityKind::FileRead => "icons/file.svg",
        ActivityKind::FileSearch => "icons/search.svg",
        ActivityKind::FileList => "icons/folder.svg",
        ActivityKind::Search => "icons/search.svg",
        ActivityKind::Plan => "icons/list.svg",
        ActivityKind::Tool => "icons/wrench.svg",
    }
}

pub fn activity_noun(kind: ActivityKind) -> (String, String) {
    match kind {
        ActivityKind::Reasoning => (tr!("activity.thought"), tr!("activity.thoughts")),
        ActivityKind::Command => (tr!("activity.command"), tr!("activity.commands")),
        ActivityKind::FileChange => (tr!("activity.file_edit"), tr!("activity.file_edits")),
        ActivityKind::FileRead => (tr!("activity.file_read"), tr!("activity.file_reads")),
        ActivityKind::FileSearch => (tr!("activity.file_search"), tr!("activity.file_searches")),
        ActivityKind::FileList => (tr!("activity.file_list"), tr!("activity.file_lists")),
        ActivityKind::Search => (tr!("activity.search"), tr!("activity.searches")),
        ActivityKind::Plan => (tr!("activity.plan_step"), tr!("activity.plan_steps")),
        ActivityKind::Tool => (tr!("activity.tool_call"), tr!("activity.tool_calls")),
    }
}

/// A compact chip used as a dropdown-menu trigger. `selected` is driven by the
/// menu's open state and renders as a soft fill.
#[derive(IntoElement)]
pub struct MenuChip {
    base: Stateful<Div>,
    icon: Option<(&'static str, Hsla)>,
    label: SharedString,
    caret: bool,
    outlined: bool,
    selected: bool,
    disabled: bool,
    height: Option<Pixels>,
    background: Option<Hsla>,
}

impl MenuChip {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            base: div().id(id),
            icon: None,
            label: SharedString::default(),
            caret: true,
            outlined: false,
            selected: false,
            disabled: false,
            height: None,
            background: None,
        }
    }

    /// Override the chip's fixed height, for rows whose controls share a
    /// different one.
    pub fn height(mut self, height: Pixels) -> Self {
        self.height = Some(height);
        self
    }

    /// Fill behind an outlined chip. The default matches raised cards; a
    /// chip sitting directly on another surface passes that surface here so
    /// it doesn't read as a filled pill.
    pub fn background(mut self, background: Hsla) -> Self {
        self.background = Some(background);
        self
    }

    pub fn icon(mut self, path: &'static str, color: Hsla) -> Self {
        self.icon = Some((path, color));
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = label.into();
        self
    }

    pub fn outlined(mut self) -> Self {
        self.outlined = true;
        self
    }

    pub fn caret(mut self, caret: bool) -> Self {
        self.caret = caret;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Soft fill marking the chip as the open menu's trigger.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl Styled for MenuChip {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for MenuChip {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl ParentElement for MenuChip {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.base.extend(elements);
    }
}

impl RenderOnce for MenuChip {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        self.base
            .h(self
                .height
                .unwrap_or(if self.outlined { px(30.0) } else { px(24.0) }))
            .px(if self.outlined { px(10.0) } else { px(7.0) })
            .rounded(if self.outlined { px(7.0) } else { px(6.0) })
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(px(11.5))
            .line_height(px(14.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .when(self.outlined, |element| {
                element
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(self.background.unwrap_or(theme.raised))
            })
            .when(self.selected, |element| element.bg(theme.overlay))
            .when(!self.disabled, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .when(self.disabled, |element| element.opacity(0.7))
            .when_some(self.icon, |element, (path, color)| {
                element.child(icon(path, 10.5, color))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text_secondary)
                    .child(self.label),
            )
            .when(self.caret, |element| {
                element.child(icon("icons/chevron-down.svg", 9.0, theme.text_ghost))
            })
    }
}

/// An inline, link-like dropdown trigger used for the project name in the
/// empty-state headline.
#[derive(IntoElement)]
pub struct ProjectNameSelector {
    base: Stateful<Div>,
    label: SharedString,
    selected: bool,
}

impl ProjectNameSelector {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            base: div().id(id),
            label: label.into(),
            selected: false,
        }
    }

    /// Emphasised underline while its menu is open.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl Styled for ProjectNameSelector {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for ProjectNameSelector {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl ParentElement for ProjectNameSelector {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.base.extend(elements);
    }
}

impl RenderOnce for ProjectNameSelector {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::current(cx);
        let underline_color = if self.selected {
            theme.text_secondary
        } else {
            theme.text_tertiary
        };

        self.base
            .relative()
            .flex_none()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(self.label)
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let y = bounds.origin.y + bounds.size.height - px(0.5);
                        let mut builder =
                            PathBuilder::stroke(px(1.0)).dash_array(&[px(1.0), px(2.0)]);
                        builder.move_to(point(bounds.origin.x, y));
                        builder.line_to(point(bounds.origin.x + bounds.size.width, y));
                        if let Ok(line) = builder.build() {
                            window.paint_path(line, underline_color);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_referenced_icon_is_embedded() {
        use crate::assets::Assets;
        use crate::model::{ActivityKind, ProviderId};
        use gpui::AssetSource;

        let mut paths = vec![
            "icons/panel-left.svg",
            "icons/plus.svg",
            "icons/arrow-left.svg",
            "icons/arrow-right.svg",
            "icons/arrow-up.svg",
            "icons/stop.svg",
            "icons/check.svg",
            "icons/copy.svg",
            "icons/rewind.svg",
            "icons/fork.svg",
            "icons/git-branch.svg",
            "icons/chart-column.svg",
            "icons/chevron-down.svg",
            "icons/chevron-right.svg",
            "icons/chevron-up.svg",
            "icons/chevrons-up-down.svg",
            "icons/folder.svg",
            "icons/folder-new.svg",
            "icons/laptop.svg",
            "icons/file-diff.svg",
            "icons/globe.svg",
            "icons/alert.svg",
            "icons/lock.svg",
            "icons/lock-open.svg",
            "icons/star.svg",
            "icons/star-filled.svg",
            "icons/sparkle.svg",
            "icons/zap.svg",
            "icons/panel-right.svg",
            "icons/x.svg",
            "icons/bot.svg",
            "icons/rotate-cw.svg",
            "icons/package.svg",
            "icons/trash.svg",
        ];
        for provider in [
            ProviderId::OPENAI_RESPONSES,
            ProviderId::OPENAI_CHAT,
            ProviderId::ANTHROPIC,
        ] {
            paths.push(provider_icon(&ProviderId::new(provider)));
        }
        for kind in [
            ActivityKind::Reasoning,
            ActivityKind::Command,
            ActivityKind::FileChange,
            ActivityKind::FileRead,
            ActivityKind::FileSearch,
            ActivityKind::FileList,
            ActivityKind::Search,
            ActivityKind::Plan,
            ActivityKind::Tool,
        ] {
            paths.push(activity_icon(kind));
        }
        for path in paths {
            assert!(
                Assets.load(path).unwrap().is_some(),
                "missing embedded icon: {path}"
            );
        }
    }

    #[test]
    fn builtin_provider_series_colors_are_stable_and_distinct() {
        let ids = [
            ProviderId::new("anthropic"),
            ProviderId::new("openai-responses"),
            ProviderId::new("openai-chat"),
            ProviderId::new("openai-codex"),
            ProviderId::new("xai"),
            ProviderId::new("xai-oauth"),
            ProviderId::new("opencode-zen"),
            ProviderId::new("opencode-go"),
        ];
        let colors: Vec<u32> = ids.iter().map(provider_series_rgb).collect();
        assert_eq!(
            provider_series_rgb(&ProviderId::new("Anthropic")),
            provider_series_rgb(&ProviderId::new("anthropic"))
        );
        let unique = colors
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ids.len());
        assert!(
            colors
                .iter()
                .all(|color| PROVIDER_SERIES_RGB.contains(color))
        );
    }

    #[test]
    fn custom_provider_series_color_stays_inside_the_palette() {
        let color = provider_series_rgb(&ProviderId::new("my-gateway"));
        assert!(PROVIDER_SERIES_RGB.contains(&color));
    }
}
