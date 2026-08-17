//! Context-window usage for the configured endpoint runtime.
//!
//! Account-specific quota fetchers belonged to the removed CLI integrations.
//! The desktop now renders only the usage reported by the daemon for the
//! selected session.

use super::*;

/// Context occupancy for the selected session, from daemon-reported usage.
impl Waku {
    pub(super) fn usage_meter_available(&self) -> bool {
        self.selected_session()
            .is_some_and(|session| session.context_usage.is_some())
    }

    pub(super) fn toggle_usage_panel_action(
        &mut self,
        _: &crate::ToggleUsagePanel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_page = Some(SettingsPage::Usage);
        cx.notify();
    }

    pub(super) fn render_usage_meter(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::current(cx);
        let usage = self.selected_session()?.context_usage.as_ref()?;
        let window = usage.window.filter(|window| *window > 0);
        let percent = window.map(|window| (usage.tokens as f32 / window as f32).clamp(0.0, 1.0));
        let label = match (percent, window) {
            (Some(percent), Some(window)) => format!(
                "{}% · {} / {}",
                (percent * 100.0).round() as u64,
                usage.tokens,
                window
            ),
            _ => format!("{} tokens", usage.tokens),
        };
        Some(
            div()
                .id("context-usage")
                .px(px(7.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .text_size(px(10.5))
                .text_color(theme.text_tertiary)
                .child(label)
                .into_any_element(),
        )
    }
}
