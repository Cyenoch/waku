use gpui::actions;

use super::composer::next_picker_highlight;
use super::*;

actions!(waku_settings, [ClearSearch]);

const SETTINGS_CONTENT_MAX_WIDTH: f32 = 760.0;

/// The Usage page is a dashboard, not a form; it mirrors T3 Code's wide
/// two-column layout and needs the extra room for the chart.
const SETTINGS_USAGE_MAX_WIDTH: f32 = 1024.0;

/// Key context the settings sidebar declares around its search field.
const SETTINGS_SIDEBAR_CONTEXT: &str = "SettingsSidebar";

/// The search field while focused inside the sidebar. The field holds real
/// focus the whole time — the sidebar's selection is only drawn — so `up` and
/// `down` have to be claimed from under it, and only a binding can do that:
/// they arrive as actions, which consume the keystroke before the field sees
/// it.
const SETTINGS_SEARCH_CONTEXT: &str = "SettingsSidebar > ComposerInput";

/// The sidebar's rows in display order, each with the keyword haystack the
/// search field filters against.
const SETTINGS_PAGES: [(SettingsPage, &str, &str, &str); 6] = [
    (
        SettingsPage::General,
        "settings.general",
        "icons/settings.svg",
        "settings.general_keywords",
    ),
    (
        SettingsPage::Appearance,
        "settings.appearance",
        "icons/appearance.svg",
        "settings.appearance_keywords",
    ),
    (
        SettingsPage::Providers,
        "settings.providers",
        "icons/bot.svg",
        "settings.providers_keywords",
    ),
    (
        SettingsPage::Skills,
        "settings.skills",
        "icons/package.svg",
        "settings.skills_keywords",
    ),
    (
        SettingsPage::Usage,
        "settings.usage",
        "icons/chart-column.svg",
        "settings.usage_keywords",
    ),
    (
        SettingsPage::Daemon,
        "settings.daemon",
        "icons/server.svg",
        "settings.daemon_keywords",
    ),
];

/// Bind the search field's list-navigation keys. Called once at startup.
pub fn init(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("down", SelectNextEntry, Some(SETTINGS_SEARCH_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(SETTINGS_SEARCH_CONTEXT)),
        // Two-stage escape: the first press clears the query, and on an empty
        // field the handler propagates, so the keystroke falls through to
        // `CancelTurn`, which closes settings.
        KeyBinding::new("escape", ClearSearch, Some(SETTINGS_SEARCH_CONTEXT)),
    ]);
}

/// The sidebar rows the query leaves visible, in display order. `query` must
/// already be trimmed and lowercased; when it is empty every page matches.
pub(super) fn visible_settings_pages(
    query: &str,
) -> impl Iterator<Item = (SettingsPage, String, &'static str)> + '_ {
    SETTINGS_PAGES
        .into_iter()
        .filter(|(page, ..)| page.is_visible_in_navigation())
        .filter_map(move |(page, label_key, icon, keywords_key)| {
            let label = crate::i18n::translate(label_key);
            let keywords = crate::i18n::translate(keywords_key).to_lowercase();
            (query.is_empty() || keywords.contains(query)).then_some((page, label, icon))
        })
}

impl Waku {
    pub(super) fn render_settings(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);

        div()
            .key_context("Waku")
            .track_focus(&self.settings_focus)
            .on_action(|_: &CloseWindow, window, _| crate::platform::hide_window(window))
            .on_action(cx.listener(Self::new_session_action))
            .on_action(cx.listener(Self::new_project_action))
            .on_action(cx.listener(Self::open_settings_action))
            .on_action(cx.listener(Self::toggle_sidebar_action))
            .on_action(cx.listener(Self::toggle_right_panel_action))
            .on_action(cx.listener(Self::toggle_command_palette_action))
            .on_action(cx.listener(Self::toggle_fps_counter_action))
            .on_action(cx.listener(Self::navigate_back_action))
            .on_action(cx.listener(Self::navigate_forward_action))
            .on_action(cx.listener(Self::focus_composer_action))
            .on_action(cx.listener(Self::cancel_turn_action))
            .capture_any_mouse_down(cx.listener(Self::navigation_mouse_down))
            .size_full()
            .flex()
            .bg(theme.canvas)
            .text_color(theme.text)
            .font_family(".SystemUIFont")
            .child(self.render_settings_sidebar(window, cx))
            .child(self.render_settings_content(window, cx))
            .into_any_element()
    }

    fn render_settings_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let current_page = self.settings_page.unwrap_or(SettingsPage::General);
        let query = self.settings_search_query(cx);
        let mut navigation = div().flex().flex_col().gap(px(3.0));

        for (page, label, icon_path) in visible_settings_pages(&query) {
            let selected = current_page == page;
            navigation = navigation.child(
                div()
                    .id(SharedString::from(format!(
                        "settings-tab-{}",
                        label.to_lowercase()
                    )))
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .h(px(36.0))
                    .px(px(11.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .cursor_default()
                    .text_size(px(13.0))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_secondary
                    })
                    .when(selected, |element| {
                        element.bg(theme.sidebar_item_background)
                    })
                    .hover(|element| element.bg(theme.sidebar_item_background))
                    .active(|element| element.bg(theme.sidebar_item_background))
                    .child(icon(
                        icon_path,
                        15.0,
                        if selected {
                            theme.text_secondary
                        } else {
                            theme.text_tertiary
                        },
                    ))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_settings_page(page, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.open_settings_page(page, cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }

        div()
            .key_context(SETTINGS_SIDEBAR_CONTEXT)
            .on_action(cx.listener(|this, _: &SelectNextEntry, _, cx| {
                this.cycle_settings_page("down", cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousEntry, _, cx| {
                this.cycle_settings_page("up", cx);
            }))
            .on_action(cx.listener(|this, _: &ClearSearch, _, cx| {
                if this.settings_search.read(cx).content().is_empty() {
                    cx.propagate();
                    return;
                }
                // `clear` emits `Edited`, and the app's subscription turns
                // that into the notify that re-expands the filtered list.
                this.settings_search.update(cx, |input, cx| input.clear(cx));
            }))
            .w(px(DEFAULT_SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.sidebar)
            .child(self.render_settings_sidebar_titlebar(window, cx))
            .child(
                div().px(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .tab_index(0)
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .h(px(34.0))
                        .px(px(9.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(9.0))
                        .cursor_default()
                        .text_size(px(13.0))
                        .text_color(theme.text_secondary)
                        .hover(|element| element.bg(theme.overlay))
                        .active(|element| element.bg(theme.overlay_strong))
                        .child(icon("icons/arrow-left.svg", 15.0, theme.text_tertiary))
                        .child(tr!("settings.back"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.settings_page = None;
                            let focus_handle = this.composer_focus(cx);
                            window.focus(&focus_handle, cx);
                            cx.notify();
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if !event.keystroke.modifiers.modified()
                                && matches!(event.keystroke.key.as_str(), "enter" | "space")
                            {
                                this.settings_page = None;
                                let focus_handle = this.composer_focus(cx);
                                window.focus(&focus_handle, cx);
                                cx.notify();
                                cx.stop_propagation();
                            }
                        })),
                ),
            )
            .child(
                div().px(px(12.0)).pt(px(8.0)).child(
                    TextField::new("settings-search-field", self.settings_search.clone())
                        .icon("icons/search.svg", 13.0),
                ),
            )
            .child(div().h(px(18.0)))
            .child(div().px(px(12.0)).child(navigation))
    }

    /// The search field's content, normalized the way the page filter expects.
    fn settings_search_query(&self, cx: &App) -> String {
        self.settings_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase()
    }

    /// Step the selected page through the rows the search leaves visible,
    /// wrapping at both ends. The field keeps focus so typing keeps narrowing
    /// the list; the landing page renders immediately, so there is no separate
    /// confirm step. A selection filtered out by the query re-enters the list
    /// from whichever end matches the key.
    fn cycle_settings_page(&mut self, key: &str, cx: &mut Context<Self>) {
        let query = self.settings_search_query(cx);
        let pages = visible_settings_pages(&query)
            .map(|(page, ..)| page)
            .collect::<Vec<_>>();
        let current_page = self.settings_page.unwrap_or(SettingsPage::General);
        let current = pages.iter().position(|page| *page == current_page);
        let Some(next) = next_picker_highlight(current, pages.len(), key) else {
            return;
        };
        self.open_settings_page(pages[next], cx);
    }

    fn render_settings_sidebar_titlebar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let left_window_controls = self.render_client_window_controls(
            super::window_chrome::WindowControlSide::Left,
            window,
            cx,
        );
        let height = if cfg!(target_os = "macos") || left_window_controls.is_some() {
            48.0
        } else {
            12.0
        };

        div()
            .id("settings-sidebar-titlebar")
            .h(px(height))
            .flex_none()
            .flex()
            .items_center()
            .children(left_window_controls)
            .child(
                self.window_drag_region(
                    div()
                        .id("settings-sidebar-traffic-light-drag-region")
                        .w(px(TRAFFIC_LIGHT_CLEARANCE))
                        .h_full()
                        .flex_none(),
                    cx,
                ),
            )
            .child(
                self.render_settings_drag_region("settings-sidebar-titlebar-drag-region", cx)
                    .h(px(height))
                    .flex_1(),
            )
    }

    fn render_settings_content(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let page = self.settings_page.unwrap_or(SettingsPage::General);
        let right_window_controls = self.render_client_window_controls(
            super::window_chrome::WindowControlSide::Right,
            window,
            cx,
        );
        // The Skills page is a mail-style split that owns the whole content
        // column — no page title, no titlebar strip, no width cap, no card.
        // Window dragging stays with the sidebar's own titlebar region.
        if page == SettingsPage::Skills {
            return div()
                .flex_1()
                .h_full()
                .min_w_0()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.sidebar_border)
                .bg(theme.surface)
                .children(right_window_controls.map(|controls| {
                    self.render_settings_drag_region("settings-skills-titlebar", cx)
                        .flex()
                        .items_center()
                        .justify_end()
                        .child(controls)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_skills_settings(cx)),
                );
        }
        // The Monthly and Projects list views own their own scrolling, so
        // their pages fill the viewport instead of riding the shared scroll
        // container.
        let fills_viewport = page == SettingsPage::Usage
            && matches!(
                self.usage_view,
                UsageViewMode::Monthly | UsageViewMode::Projects
            );
        // The titlebar strip is transparent; once content slides under it, a
        // hairline marks the boundary so the clip edge reads as a header
        // rather than a glitch.
        let content_scrolled = !fills_viewport && self.settings_scroll.offset().y < px(-1.0);

        let inner = div()
            .w_full()
            .max_w(px(match page {
                SettingsPage::Usage => SETTINGS_USAGE_MAX_WIDTH,
                _ => SETTINGS_CONTENT_MAX_WIDTH,
            }))
            .mx_auto()
            .when(fills_viewport, |element| {
                element.h_full().min_h_0().flex().flex_col()
            })
            .child(
                div()
                    .pt(px(2.0))
                    .flex_none()
                    .text_size(px(18.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(match page {
                        SettingsPage::General => tr!("settings.general"),
                        SettingsPage::Providers => tr!("settings.providers"),
                        SettingsPage::Skills => tr!("settings.skills"),
                        SettingsPage::Usage => tr!("settings.usage"),
                        SettingsPage::Daemon => tr!("settings.daemon"),
                        SettingsPage::Appearance => tr!("settings.appearance"),
                    }),
            )
            .child(match page {
                SettingsPage::General => self.render_general_settings(cx),
                SettingsPage::Providers => self.render_providers_settings(cx),
                SettingsPage::Skills => self.render_skills_settings(cx),
                SettingsPage::Usage => self.render_usage_settings(cx),
                SettingsPage::Daemon => self.render_daemon_settings(cx),
                SettingsPage::Appearance => self.render_appearance_settings(cx),
            });

        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.sidebar_border)
            .bg(theme.surface)
            .child(
                self.render_settings_drag_region("settings-content-titlebar", cx)
                    .flex()
                    .items_center()
                    .justify_end()
                    .children(right_window_controls)
                    .when(content_scrolled, |element| {
                        element.border_b_1().border_color(theme.border)
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div()
                            .id("settings-content-scroll")
                            .size_full()
                            .when(!fills_viewport, |element| {
                                element
                                    .overflow_y_scroll()
                                    .track_scroll(&self.settings_scroll)
                                    .pb(px(48.0))
                            })
                            .when(fills_viewport, |element| {
                                element.min_h_0().flex().flex_col()
                            })
                            .px(px(32.0))
                            .child(inner),
                    )
                    .when(!fills_viewport, |element| {
                        element.child(scrollbar::vertical(
                            &self.settings_scroll,
                            &self.settings_scrollbar,
                        ))
                    }),
            )
    }

    fn render_general_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let updater_available = cx
            .try_global::<crate::updater::UpdaterState>()
            .is_some_and(|updater| updater.0.is_some());
        let analytics_enabled = self.state.analytics_enabled;
        let analytics_toggle = div()
            .id("anonymous-analytics-toggle")
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .w(px(36.0))
            .h(px(20.0))
            .p(px(2.0))
            .flex_none()
            .rounded_full()
            .cursor_default()
            .bg(if analytics_enabled {
                theme.inverse
            } else {
                theme.inset
            })
            .border_1()
            .border_color(if analytics_enabled {
                theme.inverse
            } else {
                theme.border_strong
            })
            .flex()
            .items_center()
            .when(analytics_enabled, |element| element.justify_end())
            .child(
                div()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded_full()
                    .bg(if analytics_enabled {
                        theme.on_inverse
                    } else {
                        theme.text_tertiary
                    }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_analytics_enabled(!analytics_enabled, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.set_analytics_enabled(!analytics_enabled, cx);
                    cx.stop_propagation();
                }
            }));
        div()
            .child(
                div()
                    .mt(px(15.0))
                    .w_full()
                    .px(px(20.0))
                    .py(px(14.0))
                    .rounded(px(13.0))
                    .bg(theme.raised)
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("settings.local_by_default")),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .text_color(theme.text_secondary)
                            .child(tr!("settings.local_by_default_description")),
                    ),
            )
            .child(
                div()
                    .mt(px(15.0))
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .rounded(px(13.0))
                    .bg(theme.raised)
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("settings.share_anonymous_usage_data")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(px(12.5))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("settings.share_anonymous_usage_data_description")),
                            ),
                    )
                    .child(analytics_toggle),
            )
            .when(updater_available, |column| {
                let enabled = self.automatic_updates_enabled;
                let toggle = div()
                    .id("automatic-updates-toggle")
                    .tab_index(0)
                    .focus_visible(|style| style.border_color(theme.accent))
                    .w(px(36.0))
                    .h(px(20.0))
                    .p(px(2.0))
                    .flex_none()
                    .rounded_full()
                    .cursor_default()
                    .bg(if enabled { theme.inverse } else { theme.inset })
                    .border_1()
                    .border_color(if enabled {
                        theme.inverse
                    } else {
                        theme.border_strong
                    })
                    .flex()
                    .items_center()
                    .when(enabled, |element| element.justify_end())
                    .child(div().w(px(14.0)).h(px(14.0)).rounded_full().bg(if enabled {
                        theme.on_inverse
                    } else {
                        theme.text_tertiary
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_automatic_updates_enabled(!enabled, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.set_automatic_updates_enabled(!enabled, cx);
                            cx.stop_propagation();
                        }
                    }));
                column.child(
                    div()
                        .mt(px(15.0))
                        .w_full()
                        .min_h(px(60.0))
                        .px(px(20.0))
                        .py(px(12.0))
                        .rounded(px(13.0))
                        .bg(theme.raised)
                        .flex()
                        .items_center()
                        .gap(px(24.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(13.5))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(tr!("settings.automatic_updates")),
                                )
                                .child(
                                    div()
                                        .mt(px(5.0))
                                        .text_size(px(12.5))
                                        .line_height(px(18.0))
                                        .text_color(theme.text_secondary)
                                        .child(tr!("settings.automatic_updates_description")),
                                ),
                        )
                        .child(toggle),
                )
            })
            .into_any_element()
    }

    fn set_analytics_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.state.analytics_enabled = enabled;
        self.analytics.set_enabled(enabled);
        self.save();
        cx.notify();
    }

    fn set_automatic_updates_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.automatic_updates_enabled = enabled;
        if let Some(updater) = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|updater| updater.0.as_ref())
        {
            updater.set_automatically_checks_for_updates(enabled);
        }
        cx.notify();
    }

    fn render_daemon_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        if self.daemon.is_remote() {
            return div()
                .mt(px(15.0))
                .w_full()
                .px(px(20.0))
                .py(px(16.0))
                .rounded(px(13.0))
                .bg(theme.raised)
                .child(
                    div()
                        .text_size(px(13.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr!("daemon.external_title")),
                )
                .child(
                    div()
                        .mt(px(5.0))
                        .text_size(px(12.5))
                        .line_height(px(18.0))
                        .text_color(theme.text_secondary)
                        .child(tr!("daemon.external_description")),
                )
                .into_any_element();
        }

        let enabled = self.state.daemon_exposure.enabled;
        let pending = self.daemon_reconfigure_pending;
        let fields_dirty = self.daemon_exposure_fields_dirty(cx);
        let port = self.state.daemon_exposure.port;
        let websocket_url = format!("ws://{}:{port}", self.daemon_hostname);
        let token = self.state.daemon_exposure.token.clone();

        let exposure_toggle = div()
            .id("daemon-exposure-toggle")
            .tab_index(0)
            .focus_visible(|style| style.border_color(theme.accent))
            .w(px(36.0))
            .h(px(20.0))
            .p(px(2.0))
            .flex_none()
            .rounded_full()
            .cursor_default()
            .opacity(if pending { 0.55 } else { 1.0 })
            .bg(if enabled { theme.inverse } else { theme.inset })
            .border_1()
            .border_color(if enabled {
                theme.inverse
            } else {
                theme.border_strong
            })
            .flex()
            .items_center()
            .when(enabled, |element| element.justify_end())
            .child(div().w(px(14.0)).h(px(14.0)).rounded_full().bg(if enabled {
                theme.on_inverse
            } else {
                theme.text_tertiary
            }))
            .when(!pending, |element| {
                element
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_daemon_exposure_enabled(!enabled, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.set_daemon_exposure_enabled(!enabled, cx);
                            cx.stop_propagation();
                        }
                    }))
            });

        let apply_disabled = pending || !fields_dirty;
        let apply_button = div()
            .id("apply-daemon-settings")
            .tab_index(0)
            .h(px(29.0))
            .px(px(11.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_size(px(10.5))
            .text_color(theme.text_secondary)
            .opacity(if apply_disabled { 0.55 } else { 1.0 })
            .focus_visible(|style| style.border_color(theme.accent))
            .when(!apply_disabled, |element| {
                element
                    .hover(|element| element.bg(theme.overlay))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.apply_daemon_exposure_fields(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.apply_daemon_exposure_fields(cx);
                            cx.stop_propagation();
                        }
                    }))
            })
            .child(if pending {
                tr!("daemon.restarting")
            } else {
                tr!("daemon.apply")
            });

        let copy_url_feedback_id = "daemon-url";
        let url_copied = self.control_was_copied(copy_url_feedback_id);
        let copy_url = websocket_url.clone();
        let copy_url_button = div()
            .id("copy-daemon-url")
            .tab_index(0)
            .h(px(27.0))
            .px(px(9.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .gap(px(5.0))
            .cursor_default()
            .text_size(px(10.5))
            .text_color(theme.text_secondary)
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .child(icon(
                if url_copied {
                    "icons/check.svg"
                } else {
                    "icons/copy.svg"
                },
                11.0,
                theme.text_tertiary,
            ))
            .child(if url_copied {
                tr!("common.copied")
            } else {
                tr!("common.copy")
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()));
                this.show_control_copied(copy_url_feedback_id, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(websocket_url.clone()));
                    this.show_control_copied(copy_url_feedback_id, cx);
                    cx.stop_propagation();
                }
            }));

        let copy_token_feedback_id = "daemon-token";
        let token_copied = self.control_was_copied(copy_token_feedback_id);
        let click_token = token.clone();
        let key_token = token.clone();
        let token_revealed = self.daemon_token_revealed;
        let reveal_token_button = div()
            .id("reveal-daemon-token")
            .tab_index(0)
            .size(px(27.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .text_color(theme.text_secondary)
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon(
                if token_revealed {
                    "icons/eye-off.svg"
                } else {
                    "icons/eye.svg"
                },
                12.0,
                theme.text_tertiary,
            ))
            .tooltip(Tooltip::text(if token_revealed {
                tr!("daemon.hide_token")
            } else {
                tr!("daemon.reveal_token")
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.daemon_token_revealed = !this.daemon_token_revealed;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.daemon_token_revealed = !this.daemon_token_revealed;
                    cx.stop_propagation();
                    cx.notify();
                }
            }));
        let copy_token_button = div()
            .id("copy-daemon-token")
            .tab_index(0)
            .h(px(27.0))
            .px(px(9.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .gap(px(5.0))
            .cursor_default()
            .text_size(px(10.5))
            .text_color(theme.text_secondary)
            .focus_visible(|style| style.border_color(theme.accent))
            .hover(|element| element.bg(theme.overlay))
            .child(icon(
                if token_copied {
                    "icons/check.svg"
                } else {
                    "icons/copy.svg"
                },
                11.0,
                theme.text_tertiary,
            ))
            .child(if token_copied {
                tr!("common.copied")
            } else {
                tr!("common.copy")
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(click_token.clone()));
                this.show_control_copied(copy_token_feedback_id, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(key_token.clone()));
                    this.show_control_copied(copy_token_feedback_id, cx);
                    cx.stop_propagation();
                }
            }));

        let regenerate_button = div()
            .id("regenerate-daemon-token")
            .tab_index(0)
            .h(px(27.0))
            .px(px(9.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .cursor_default()
            .text_size(px(10.5))
            .text_color(theme.text_secondary)
            .opacity(if pending { 0.55 } else { 1.0 })
            .focus_visible(|style| style.border_color(theme.accent))
            .when(!pending, |element| {
                element
                    .hover(|element| element.bg(theme.overlay))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.regenerate_daemon_token(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.regenerate_daemon_token(cx);
                            cx.stop_propagation();
                        }
                    }))
            })
            .child(tr!("daemon.regenerate_token"));

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .min_h(px(66.0))
                    .px(px(20.0))
                    .py(px(13.0))
                    .rounded(px(13.0))
                    .bg(theme.raised)
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .child(
                                        div()
                                            .text_size(px(13.5))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(tr!("daemon.expose_title")),
                                    )
                                    .child(
                                        div()
                                            .px(px(6.0))
                                            .py(px(2.0))
                                            .rounded_full()
                                            .text_size(px(9.5))
                                            .text_color(if enabled {
                                                theme.success
                                            } else {
                                                theme.text_tertiary
                                            })
                                            .bg(theme.overlay)
                                            .child(if pending {
                                                tr!("daemon.status_restarting")
                                            } else if enabled {
                                                tr!("daemon.status_exposed")
                                            } else {
                                                tr!("daemon.status_local")
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .min_w_0()
                                    .whitespace_normal()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("daemon.expose_description")),
                            ),
                    )
                    .child(exposure_toggle),
            )
            .when(enabled, |column| {
                column.child(
                    div()
                        .px(px(20.0))
                        .py(px(15.0))
                        .rounded(px(13.0))
                        .bg(theme.raised)
                        .child(
                            div()
                                .text_size(px(13.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(tr!("daemon.connection_title")),
                        )
                        .child(
                            div()
                                .mt(px(4.0))
                                .min_w_0()
                                .whitespace_normal()
                                .text_size(px(11.5))
                                .line_height(px(16.0))
                                .text_color(theme.text_secondary)
                                .child(tr!("daemon.connection_description")),
                        )
                        .child(
                            div()
                                .mt(px(14.0))
                                .flex()
                                .items_start()
                                .gap(px(24.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child(tr!("daemon.port")),
                                        )
                                        .child(
                                            div()
                                                .mt(px(3.0))
                                                .whitespace_normal()
                                                .text_size(px(10.0))
                                                .line_height(px(14.0))
                                                .text_color(theme.text_tertiary)
                                                .child(tr!("daemon.port_description")),
                                        ),
                                )
                                .child(
                                    div().flex_1().min_w_0().flex().justify_end().child(
                                        TextField::new(
                                            "daemon-port-field",
                                            self.daemon_port_input.clone(),
                                        )
                                        .w(px(150.0)),
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(14.0))
                                .flex()
                                .items_start()
                                .gap(px(24.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child(tr!("daemon.allowed_origins")),
                                        )
                                        .child(
                                            div()
                                                .mt(px(3.0))
                                                .whitespace_normal()
                                                .text_size(px(10.0))
                                                .line_height(px(14.0))
                                                .text_color(theme.text_tertiary)
                                                .child(tr!("daemon.allowed_origins_description")),
                                        ),
                                )
                                .child(
                                    div().flex_1().min_w_0().flex().justify_end().child(
                                        TextField::new(
                                            "daemon-origins-field",
                                            self.daemon_origins_input.clone(),
                                        )
                                        .w_full()
                                        .max_w(px(360.0)),
                                    ),
                                ),
                        )
                        .child(div().mt(px(13.0)).flex().justify_end().child(apply_button)),
                )
            })
            .when(enabled, |column| {
                column.child(
                    div()
                        .px(px(20.0))
                        .py(px(15.0))
                        .rounded(px(13.0))
                        .bg(theme.raised)
                        .child(
                            div()
                                .text_size(px(13.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(tr!("daemon.credentials_title")),
                        )
                        .child(
                            div()
                                .mt(px(4.0))
                                .min_w_0()
                                .whitespace_normal()
                                .text_size(px(11.5))
                                .line_height(px(16.0))
                                .text_color(theme.text_secondary)
                                .child(tr!("daemon.credentials_description")),
                        )
                        .child(
                            div()
                                .mt(px(13.0))
                                .py(px(8.0))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(
                                    div()
                                        .w(px(80.0))
                                        .flex_none()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_tertiary)
                                        .child(tr!("daemon.websocket_url")),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(".SystemUIFontMonospaced")
                                        .text_size(px(11.0))
                                        .text_color(theme.text)
                                        .child(SharedString::from(format!(
                                            "ws://{}:{port}",
                                            self.daemon_hostname
                                        ))),
                                )
                                .child(copy_url_button),
                        )
                        .child(
                            div()
                                .py(px(8.0))
                                .border_t_1()
                                .border_color(theme.border)
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(
                                    div()
                                        .w(px(80.0))
                                        .flex_none()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_tertiary)
                                        .child(tr!("daemon.token")),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(".SystemUIFontMonospaced")
                                        .text_size(px(11.0))
                                        .text_color(theme.text)
                                        .child(SharedString::from(if token_revealed {
                                            token.clone()
                                        } else {
                                            "••••••••••••••••••••••••••••••••".to_owned()
                                        })),
                                )
                                .child(reveal_token_button)
                                .child(copy_token_button)
                                .child(regenerate_button),
                        )
                        .child(
                            div()
                                .mt(px(7.0))
                                .px(px(10.0))
                                .py(px(8.0))
                                .rounded(px(8.0))
                                .bg(theme.inset)
                                .w_full()
                                .min_w_0()
                                .flex()
                                .gap(px(8.0))
                                .child(icon("icons/alert.svg", 13.0, theme.warning))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .whitespace_normal()
                                        .text_size(px(10.5))
                                        .line_height(px(15.0))
                                        .text_color(theme.text_secondary)
                                        .child(tr!("daemon.security_warning")),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    fn daemon_exposure_from_fields(
        &self,
        cx: &App,
    ) -> Result<waku_client::DaemonExposureSettings, String> {
        let port = self
            .daemon_port_input
            .read(cx)
            .content()
            .trim()
            .parse::<u16>()
            .map_err(|_| tr!("daemon.invalid_port"))?;
        if port == 0 {
            return Err(tr!("daemon.invalid_port"));
        }
        let origins = self.daemon_origins_input.read(cx).content().to_owned();
        let mut settings = self.state.daemon_exposure.clone();
        settings.port = port;
        settings
            .with_allowed_origins_text(&origins)
            .and_then(waku_client::DaemonExposureSettings::validate)
            .map_err(|error| error.to_string())
    }

    fn daemon_exposure_fields_dirty(&self, cx: &App) -> bool {
        self.daemon_exposure_from_fields(cx)
            .map(|settings| {
                settings.port != self.state.daemon_exposure.port
                    || settings.allowed_origins != self.state.daemon_exposure.allowed_origins
            })
            .unwrap_or(true)
    }

    fn set_daemon_exposure_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if !enabled {
            self.daemon_token_revealed = false;
        }
        let settings = if enabled {
            match self.daemon_exposure_from_fields(cx) {
                Ok(mut settings) => {
                    settings.enabled = true;
                    settings
                }
                Err(error) => {
                    self.show_toast(tr!("daemon.invalid_settings", error = error));
                    return;
                }
            }
        } else {
            let mut settings = self.state.daemon_exposure.clone();
            settings.enabled = false;
            settings
        };
        self.apply_daemon_exposure(settings, cx);
    }

    pub(super) fn apply_daemon_exposure_fields(&mut self, cx: &mut Context<Self>) {
        let settings = match self.daemon_exposure_from_fields(cx) {
            Ok(settings) => settings,
            Err(error) => {
                self.show_toast(tr!("daemon.invalid_settings", error = error));
                return;
            }
        };
        self.apply_daemon_exposure(settings, cx);
    }

    fn regenerate_daemon_token(&mut self, cx: &mut Context<Self>) {
        let mut settings = match self.daemon_exposure_from_fields(cx) {
            Ok(settings) => settings,
            Err(error) => {
                self.show_toast(tr!("daemon.invalid_settings", error = error));
                return;
            }
        };
        settings.token = waku_client::DaemonExposureSettings::new_token();
        self.daemon_token_revealed = false;
        self.apply_daemon_exposure(settings, cx);
    }

    fn apply_daemon_exposure(
        &mut self,
        settings: waku_client::DaemonExposureSettings,
        cx: &mut Context<Self>,
    ) {
        if self.daemon_reconfigure_pending || settings == self.state.daemon_exposure {
            return;
        }
        if self.daemon.is_remote() {
            self.show_toast(tr!("daemon.external_description"));
            return;
        }
        if self
            .state
            .sessions
            .iter()
            .any(|session| !matches!(session.status, SessionStatus::Idle | SessionStatus::Failed))
        {
            self.show_toast(tr!("daemon.stop_active_tasks"));
            return;
        }

        let needs_restart = self.state.daemon_exposure.enabled || settings.enabled;
        if !needs_restart {
            self.state.daemon_exposure = settings;
            self.save();
            cx.notify();
            return;
        }

        self.daemon_reconfigure_pending = true;
        let daemon = self.daemon.clone();
        let applied = settings.clone();
        let restart = cx
            .background_executor()
            .spawn(async move { daemon.reconfigure(settings) });
        cx.spawn(async move |this, cx| {
            let result = restart.await;
            let _ = this.update(cx, |this, cx| {
                this.daemon_reconfigure_pending = false;
                match result {
                    Ok(()) => {
                        this.state.daemon_exposure = applied.clone();
                        this.runtimes.clear();
                        this.daemon_port_input.update(cx, |input, cx| {
                            input.set_content(applied.port.to_string(), cx)
                        });
                        this.daemon_origins_input.update(cx, |input, cx| {
                            input.set_content(applied.allowed_origins_text(), cx)
                        });
                        this.save();
                        this.show_success_toast(tr!("daemon.settings_applied"));
                    }
                    Err(error) => {
                        this.show_toast(tr!("daemon.restart_failed", error = error.to_string()))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_appearance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let selected_theme = self.state.theme;
        let selected_language = self.state.language;
        let weak = cx.entity().downgrade();
        let theme_handle = self.menu_handle("theme-selector", cx);
        let theme_selector = dropdown_menu(
            MenuChip::new("theme-selector")
                .label(selected_theme.label())
                .outlined()
                .selected(theme_handle.is_open())
                .w(px(116.0))
                .justify_between(),
            "theme-selector-menu",
            &theme_handle,
            MenuAlign::BelowRight,
            move |_| {
                ThemePreference::ALL
                    .into_iter()
                    .map(|preference| {
                        let weak = weak.clone();
                        MenuItem::new(preference.label(), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_theme_preference(preference, window, cx);
                            });
                        })
                        .selected(preference == selected_theme)
                    })
                    .collect()
            },
        );

        let weak = cx.entity().downgrade();
        let language_handle = self.menu_handle("language-selector", cx);
        let language_selector = dropdown_menu(
            MenuChip::new("language-selector")
                .label(selected_language.label())
                .outlined()
                .selected(language_handle.is_open())
                .w(px(116.0))
                .justify_between(),
            "language-selector-menu",
            &language_handle,
            MenuAlign::BelowRight,
            move |_| {
                crate::i18n::AppLanguage::ALL
                    .into_iter()
                    .map(|language| {
                        let weak = weak.clone();
                        MenuItem::new(language.label(), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.set_language(language, window, cx);
                            });
                        })
                        .selected(language == selected_language)
                    })
                    .collect()
            },
        );

        div()
            .mt(px(15.0))
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(13.0))
            .overflow_hidden()
            .bg(theme.raised)
            .child(
                div()
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("settings.theme")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(px(12.5))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("settings.theme_description")),
                            ),
                    )
                    .child(theme_selector),
            )
            .child(div().mx(px(20.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .w_full()
                    .min_h(px(60.0))
                    .px(px(20.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(24.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(tr!("language.title")),
                            )
                            .child(
                                div()
                                    .mt(px(5.0))
                                    .text_size(px(12.5))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("language.description")),
                            ),
                    )
                    .child(language_selector),
            )
            .into_any_element()
    }

    fn render_providers_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let providers = &self.state.external_providers;
        let editing = self.expanded_provider_settings.clone();
        let builtin_cards = self.render_builtin_provider_cards(cx);
        let mut rows = div().mt(px(12.0)).flex().flex_col().gap(px(8.0));
        if providers.is_empty() {
            rows = rows.child(
                div()
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(theme.raised)
                    .text_color(theme.text_secondary)
                    .child(tr!("providers.configure_first")),
            );
        }
        for provider in providers {
            let id = provider.id.clone();
            let id_for_edit = id.clone();
            let id_for_edit_keyboard = id.clone();
            let id_for_delete = id.clone();
            let is_editing = editing.as_ref() == Some(&id);
            let label = format!("{} · {}", provider.name, provider.default_model);
            rows = rows.child(
                div()
                    .p(px(14.0))
                    .rounded(px(10.0))
                    .bg(theme.raised)
                    .border_1()
                    .border_color(if is_editing {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(icon("icons/bot.svg", 16.0, theme.text_secondary))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_color(theme.text).child(label)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_tertiary)
                                    .child(SharedString::from(format!(
                                        "{} · {}",
                                        provider.id, provider.api_format
                                    ))),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("edit-provider-{}", id)))
                                    .tab_index(0)
                                    .focus_visible(|style| {
                                        style.border_1().border_color(theme.accent)
                                    })
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .text_color(theme.text_secondary)
                                    .hover(|element| element.bg(theme.overlay))
                                    .child(tr!("common.edit"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_provider_edit(Some(id_for_edit.clone()), cx);
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.begin_provider_edit(
                                                    Some(id_for_edit_keyboard.clone()),
                                                    cx,
                                                );
                                                cx.stop_propagation();
                                            }
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "delete-provider-{}",
                                        id_for_delete
                                    )))
                                    .tab_index(0)
                                    .focus_visible(|style| {
                                        style.border_1().border_color(theme.accent)
                                    })
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .text_color(theme.danger)
                                    .hover(|element| element.bg(theme.overlay))
                                    .child(tr!("common.delete"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.delete_provider(&id_for_delete, cx);
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.delete_provider(&id, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(px(10.5))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(provider.base_url.clone())),
                    ),
            );
        }
        let add = div()
            .id("add-provider")
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .px(px(11.0))
            .py(px(7.0))
            .rounded(px(7.0))
            .bg(theme.accent)
            .text_color(theme.text)
            .hover(|element| element.opacity(0.9))
            .child(tr!("providers.add"))
            .on_click(cx.listener(|this, _, _, cx| this.begin_provider_edit(None, cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.begin_provider_edit(None, cx);
                    cx.stop_propagation();
                }
            }));
        let form = editing.map(|provider_id| self.render_provider_form(provider_id, theme, cx));
        div()
            .w_full()
            .px(px(20.0))
            .py(px(16.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(20.0))
                    .child(
                        div().flex_1().min_w_0().child(
                            div()
                                .text_size(px(15.0))
                                .font_weight(FontWeight::MEDIUM)
                                .child(tr!("providers.title"))
                                .child(
                                    div()
                                        .mt(px(5.0))
                                        .text_size(px(12.0))
                                        .line_height(px(18.0))
                                        .text_color(theme.text_secondary)
                                        .child(tr!("providers.config_description")),
                                ),
                        ),
                    )
                    .child(add),
            )
            .child(builtin_cards)
            .child(rows)
            .children(form)
            .into_any_element()
    }

    fn render_builtin_provider_cards(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let mut cards = div().mt(px(12.0)).flex().flex_col().gap(px(8.0));
        for preset in waku_client::ProviderPreset::ALL {
            let provider = preset.provider_id();
            let auth = self.auth_statuses.get(&provider);
            let status = auth
                .map(|value| format!("{:?}", value.method))
                .unwrap_or_else(|| tr!("providers.not_authenticated").to_string());
            let methods = preset_login_methods(preset.id());
            let mut logins = div().flex().items_center().gap(px(6.0));
            for method in methods {
                let login_provider = provider.clone();
                let key_provider = provider.clone();
                let method = *method;
                logins = logins.child(
                    div()
                        .id(SharedString::from(format!(
                            "login-provider-{}-{}",
                            preset.id(),
                            method as u8
                        )))
                        .tab_index(0)
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(6.0))
                        .text_color(theme.text_secondary)
                        .hover(|element| element.bg(theme.overlay))
                        .child(login_method_label(method))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.start_provider_login(login_provider.clone(), method, cx)
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if !event.keystroke.modifiers.modified()
                                && matches!(event.keystroke.key.as_str(), "enter" | "space")
                            {
                                this.start_provider_login(key_provider.clone(), method, cx);
                                cx.stop_propagation();
                            }
                        })),
                );
            }
            let logout_provider = provider.clone();
            let logout_key_provider = provider.clone();
            let logout = div()
                .id(SharedString::from(format!(
                    "logout-provider-{}",
                    preset.id()
                )))
                .tab_index(0)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .px(px(8.0))
                .py(px(5.0))
                .rounded(px(6.0))
                .text_color(theme.danger)
                .hover(|element| element.bg(theme.overlay))
                .child(tr!("providers.logout"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.logout_provider(logout_provider.clone(), cx)
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        this.logout_provider(logout_key_provider.clone(), cx);
                        cx.stop_propagation();
                    }
                }));
            let mut card = div()
                .p(px(14.0))
                .rounded(px(10.0))
                .bg(theme.raised)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(icon("icons/bot.svg", 16.0, theme.text_secondary))
                        .child(
                            div()
                                .flex_1()
                                .child(div().text_color(theme.text).child(preset.display_name())),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_secondary)
                                .child(status),
                        )
                        .child(logins)
                        .child(logout),
                );
            if let Some(phase) = self
                .auth_phases
                .iter()
                .find(|phase| phase.provider() == Some(&provider))
            {
                if let waku_client::AuthPhase::AwaitingApiKey { .. } = phase {
                    let api_provider = provider.clone();
                    let api_key_provider = provider.clone();
                    card = card
                        .child(
                            TextField::new("provider-api-key", self.auth_api_key_input.clone())
                                .w_full(),
                        )
                        .child(
                            div()
                                .id("complete-provider-api-key")
                                .tab_index(0)
                                .focus_visible(|style| style.border_1().border_color(theme.accent))
                                .px(px(8.0))
                                .py(px(5.0))
                                .child(tr!("common.continue"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.complete_api_key_login(api_provider.clone(), cx)
                                }))
                                .on_key_down(cx.listener(
                                    move |this, event: &KeyDownEvent, _, cx| {
                                        if !event.keystroke.modifiers.modified()
                                            && matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            )
                                        {
                                            this.complete_api_key_login(
                                                api_key_provider.clone(),
                                                cx,
                                            );
                                            cx.stop_propagation();
                                        }
                                    },
                                )),
                        );
                }
                if let Some(login_id) = phase.login_id().filter(|_| {
                    matches!(
                        phase,
                        waku_client::AuthPhase::AwaitingBrowser { .. }
                            | waku_client::AuthPhase::AwaitingDevice { .. }
                            | waku_client::AuthPhase::AwaitingApiKey { .. }
                    )
                }) {
                    card = card.child(
                        div()
                            .id("cancel-provider-login")
                            .tab_index(0)
                            .focus_visible(|style| style.border_1().border_color(theme.accent))
                            .px(px(8.0))
                            .py(px(5.0))
                            .child(tr!("common.cancel"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_provider_login(login_id, cx)
                            }))
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                if !event.keystroke.modifiers.modified()
                                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                {
                                    this.cancel_provider_login(login_id, cx);
                                    cx.stop_propagation();
                                }
                            })),
                    );
                }
                card = card.child(
                    div()
                        .mt(px(5.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_secondary)
                        .child(auth_phase_summary(phase)),
                );
            }
            cards = cards.child(card);
        }
        cards
    }

    fn provider_field(
        &self,
        id: &str,
        input: Entity<ComposerInput>,
        label: String,
        theme: Theme,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .child(label),
            )
            .child(TextField::new(SharedString::from(id.to_owned()), input).w_full())
    }

    fn render_provider_form(
        &self,
        provider_id: ProviderId,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let existing = self
            .state
            .external_providers
            .iter()
            .find(|provider| provider.id == provider_id);
        let api_formats = waku_client::ApiFormat::ALL;
        let format = self.provider_api_format;
        let format_button = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("providers.api_format")),
            )
            .child(
                div()
                    .id("provider-api-format")
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .px(px(9.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .bg(theme.overlay)
                    .text_color(theme.text)
                    .child(format.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let index = api_formats
                            .iter()
                            .position(|candidate| *candidate == this.provider_api_format)
                            .unwrap_or(0);
                        this.provider_api_format = api_formats[(index + 1) % api_formats.len()];
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            let index = api_formats
                                .iter()
                                .position(|candidate| *candidate == this.provider_api_format)
                                .unwrap_or(0);
                            this.provider_api_format = api_formats[(index + 1) % api_formats.len()];
                            cx.notify();
                            cx.stop_propagation();
                        }
                    })),
            );
        let save_id = provider_id.clone();
        let save_key_id = provider_id.clone();
        let save = div()
            .id("save-provider")
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(theme.accent)
            .text_color(theme.text)
            .child(tr!("common.save"))
            .on_click(cx.listener(move |this, _, _, cx| this.save_provider(&save_id, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.save_provider(&save_key_id, cx);
                    cx.stop_propagation();
                }
            }));
        let cancel = div()
            .id("cancel-provider")
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .text_color(theme.text_secondary)
            .hover(|element| element.bg(theme.overlay))
            .child(tr!("common.cancel"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.expanded_provider_settings = None;
                this.clear_provider_form(cx);
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.expanded_provider_settings = None;
                    this.clear_provider_form(cx);
                    cx.notify();
                    cx.stop_propagation();
                }
            }));
        let models_hint = existing.map(|provider| provider.models.len()).unwrap_or(0);
        div()
            .mt(px(10.0))
            .p(px(14.0))
            .rounded(px(10.0))
            .bg(theme.surface)
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("providers.edit_title")),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .flex_wrap()
                    .gap(px(10.0))
                    .child(if existing.is_some() {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("providers.id")),
                            )
                            .child(
                                div()
                                    .px(px(9.0))
                                    .py(px(7.0))
                                    .rounded(px(6.0))
                                    .bg(theme.inset)
                                    .text_color(theme.text_secondary)
                                    .child(SharedString::from(provider_id.to_string())),
                            )
                    } else {
                        self.provider_field(
                            "provider-id",
                            self.provider_id_input.clone(),
                            tr!("providers.id"),
                            theme,
                        )
                    })
                    .child(self.provider_field(
                        "provider-name",
                        self.provider_name_input.clone(),
                        tr!("providers.name"),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-base-url",
                        self.provider_base_url_input.clone(),
                        tr!("providers.base_url"),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-api-key-env",
                        self.provider_api_key_env_input.clone(),
                        tr!("providers.api_key_env"),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-models",
                        self.provider_model_input.clone(),
                        format!("{} ({models_hint})", tr!("providers.models")),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-default-model",
                        self.provider_default_model_input.clone(),
                        tr!("providers.default_model"),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-limits-context",
                        self.provider_context_window_input.clone(),
                        tr!("providers.context_window"),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-limits-output",
                        self.provider_max_output_tokens_input.clone(),
                        tr!("providers.max_output_tokens"),
                        theme,
                    ))
                    .child(self.provider_field(
                        "provider-headers",
                        self.provider_headers_input.clone(),
                        tr!("providers.headers"),
                        theme,
                    )),
            )
            .child(div().mt(px(10.0)).child(format_button))
            .child(
                div()
                    .mt(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(save)
                    .child(cancel),
            )
    }

    fn begin_provider_edit(&mut self, provider: Option<ProviderId>, cx: &mut Context<Self>) {
        let provider = provider
            .unwrap_or_else(|| ProviderId::new(format!("provider-{}", Uuid::new_v4().simple())));
        let existing = self
            .state
            .external_providers
            .iter()
            .find(|candidate| candidate.id == provider)
            .cloned();
        self.expanded_provider_settings = Some(provider.clone());
        self.provider_api_format = existing
            .as_ref()
            .map(|provider| provider.api_format)
            .unwrap_or_default();
        self.provider_id_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| provider.id.to_string())
                    .unwrap_or_else(|| provider.to_string()),
                cx,
            )
        });
        self.provider_name_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| provider.name.clone())
                    .unwrap_or_default(),
                cx,
            )
        });
        self.provider_base_url_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| provider.base_url.clone())
                    .unwrap_or_default(),
                cx,
            )
        });
        self.provider_api_key_env_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .and_then(|provider| provider.api_key_env.clone())
                    .unwrap_or_default(),
                cx,
            )
        });
        self.provider_headers_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| {
                        provider
                            .headers
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
                cx,
            );
        });
        self.provider_model_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| {
                        (if provider.models.is_empty() {
                            vec![provider.default_model.clone()]
                        } else {
                            provider.models.clone()
                        })
                        .join(", ")
                    })
                    .unwrap_or_default(),
                cx,
            )
        });
        self.provider_default_model_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| provider.default_model.clone())
                    .unwrap_or_default(),
                cx,
            )
        });
        self.provider_context_window_input.update(cx, |input, cx| {
            input.set_content(
                existing
                    .as_ref()
                    .map(|provider| provider.context_window.to_string())
                    .unwrap_or_else(|| "128000".to_owned()),
                cx,
            )
        });
        self.provider_max_output_tokens_input
            .update(cx, |input, cx| {
                input.set_content(
                    existing
                        .as_ref()
                        .map(|provider| provider.max_output_tokens.to_string())
                        .unwrap_or_else(|| "16384".to_owned()),
                    cx,
                )
            });
        cx.notify();
    }

    fn clear_provider_form(&mut self, cx: &mut Context<Self>) {
        for input in [
            &self.provider_id_input,
            &self.provider_name_input,
            &self.provider_base_url_input,
            &self.provider_api_key_env_input,
            &self.provider_headers_input,
            &self.provider_model_input,
            &self.provider_default_model_input,
            &self.provider_context_window_input,
            &self.provider_max_output_tokens_input,
        ] {
            input.update(cx, |input, cx| input.clear(cx));
        }
    }

    fn delete_provider(&mut self, id: &ProviderId, cx: &mut Context<Self>) {
        if self.state.last_provider == *id
            || self
                .state
                .sessions
                .iter()
                .any(|session| &session.provider == id)
            || self
                .state
                .favorite_models
                .iter()
                .any(|favorite| &favorite.provider == id)
        {
            self.show_toast(tr!("providers.in_use"));
            cx.notify();
            return;
        }
        self.state
            .external_providers
            .retain(|provider| &provider.id != id);
        if self.expanded_provider_settings.as_ref() == Some(id) {
            self.expanded_provider_settings = None;
        }
        self.save();
        cx.notify();
    }

    fn save_provider(&mut self, editing_id: &ProviderId, cx: &mut Context<Self>) {
        let id = ProviderId::new(self.provider_id_input.read(cx).content().trim());
        let name = self
            .provider_name_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let base_url = self
            .provider_base_url_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let api_key_env_input = self
            .provider_api_key_env_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let model_input = self.provider_model_input.read(cx).content().to_owned();
        let default_input = self
            .provider_default_model_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let context_input = self
            .provider_context_window_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let output_input = self
            .provider_max_output_tokens_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let header_input = self.provider_headers_input.read(cx).content().to_owned();
        if let Err(error) = parse_provider_draft_policy(ProviderDraftPolicy {
            id: id.as_str(),
            name: &name,
            base_url: &base_url,
            api_key_env: Some(&api_key_env_input),
            models_text: &model_input,
            default_model: &default_input,
            context_window: &context_input,
            max_output_tokens: &output_input,
            headers_text: &header_input,
        }) {
            self.show_toast(error);
            cx.notify();
            return;
        }
        let models = self
            .provider_model_input
            .read(cx)
            .content()
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut seen_models = std::collections::HashSet::new();
        if models
            .iter()
            .any(|model| !seen_models.insert(model.to_ascii_lowercase()))
        {
            self.show_toast(tr!("providers.duplicate_models"));
            cx.notify();
            return;
        }
        let default_model = self
            .provider_default_model_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let context_window = match self
            .provider_context_window_input
            .read(cx)
            .content()
            .trim()
            .parse::<u64>()
        {
            Ok(value) => value,
            Err(_) => {
                self.show_toast(tr!("providers.invalid_context_window").to_owned());
                cx.notify();
                return;
            }
        };
        let max_output_tokens = match self
            .provider_max_output_tokens_input
            .read(cx)
            .content()
            .trim()
            .parse::<u64>()
        {
            Ok(value) => value,
            Err(_) => {
                self.show_toast(tr!("providers.invalid_max_output_tokens").to_owned());
                cx.notify();
                return;
            }
        };
        if id.validate().is_err()
            || (editing_id.is_valid() && id != *editing_id)
            || name.is_empty()
            || base_url.is_empty()
            || models.is_empty()
            || !models.iter().any(|model| model == &default_model)
        {
            self.show_toast(tr!("providers.invalid_configuration"));
            cx.notify();
            return;
        }
        let api_key_env = self
            .provider_api_key_env_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let mut headers = Vec::new();
        let mut header_names = std::collections::HashSet::new();
        for line in self
            .provider_headers_input
            .read(cx)
            .content()
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let Some((header_name, header_value)) = line.split_once(':') else {
                self.show_toast(tr!("providers.invalid_header_format").to_owned());
                cx.notify();
                return;
            };
            let header_name = header_name.trim().to_owned();
            let header_value = header_value.trim().to_owned();
            if header_name.is_empty()
                || header_value.is_empty()
                || !header_names.insert(header_name.to_ascii_lowercase())
                || matches!(
                    header_name.to_ascii_lowercase().as_str(),
                    "authorization"
                        | "x-api-key"
                        | "host"
                        | "content-length"
                        | "content-type"
                        | "anthropic-version"
                )
            {
                self.show_toast(tr!("providers.invalid_headers").to_owned());
                cx.notify();
                return;
            }
            headers.push((header_name, header_value));
        }
        let provider = ExternalProvider {
            id: id.clone(),
            name,
            base_url,
            api_format: self.provider_api_format,
            api_key_env: (!api_key_env.is_empty()).then_some(api_key_env),
            headers,
            models,
            default_model,
            context_window,
            max_output_tokens,
        };
        if let Err(error) = provider.validate() {
            self.show_toast(error);
            cx.notify();
            return;
        }
        if self
            .state
            .external_providers
            .iter()
            .any(|candidate| candidate.id == id && &candidate.id != editing_id)
        {
            self.show_toast(tr!("providers.duplicate_id"));
            cx.notify();
            return;
        }
        if let Some(existing) = self
            .state
            .external_providers
            .iter_mut()
            .find(|candidate| &candidate.id == editing_id)
        {
            *existing = provider;
        } else {
            self.state.external_providers.push(provider);
        }
        self.expanded_provider_settings = Some(id.clone());
        if self.state.last_provider == ProviderId::new("")
            || self.state.last_provider == *editing_id
        {
            self.state.last_provider = id;
        }
        self.save();
        cx.notify();
    }
    fn render_settings_drag_region(
        &self,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(48.0))
            .flex_none()
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    crate::platform::titlebar_double_click(window);
                }
            })
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.header_drag_armed = false;
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.header_drag_armed = false;
                }),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.header_drag_armed {
                    this.header_drag_armed = false;
                    crate::platform::start_window_move(window);
                }
            }))
    }
    fn set_theme_preference(
        &mut self,
        preference: ThemePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.theme == preference {
            return;
        }
        self.state.theme = preference;
        crate::theme::apply_theme_preference(preference, window, cx);
        self.save();
        cx.notify();
    }
    fn set_language(
        &mut self,
        language: crate::i18n::AppLanguage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.language == language {
            return;
        }

        self.state.language = language;
        crate::i18n::set_language(language);

        self.composer.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.do_anything"), cx)
        });
        self.model_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.search_models"), cx)
        });
        self.branch_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.search_branches"), cx)
        });
        self.branch_create_input.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.new_branch_name"), cx)
        });
        self.settings_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("settings.search"), cx)
        });
        self.skills_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("skills.search"), cx)
        });
        self.usage_project_filter.update(cx, |input, cx| {
            input.set_placeholder(tr!("input.filter_projects"), cx)
        });
        self.refresh_command_palette_localized_text(cx);
        self.refresh_file_search_localized_text(cx);
        for browser in self.right_panel_browsers.values() {
            browser.update(cx, |browser, cx| browser.refresh_localized_text(cx));
        }
        for terminal in self.right_panel_terminals.values() {
            terminal.update(cx, |terminal, cx| terminal.refresh_localized_text(cx));
        }
        self.invalidate_composer_sources(cx);

        let updater_available = cx
            .try_global::<crate::updater::UpdaterState>()
            .and_then(|updater| updater.0.as_ref())
            .is_some();
        crate::set_app_menus(cx, updater_available);
        self.save();
        window.refresh();
        cx.notify();
    }

    pub(super) fn refresh_provider_auth_statuses(&mut self, cx: &mut Context<Self>) {
        self.auth_generation = self.auth_generation.wrapping_add(1);
        let generation = self.auth_generation;
        let client = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.get_auth_status(None) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.auth_generation {
                    return;
                }
                this.auth_pending.clear();
                match result {
                    Ok(waku_client::ResponsePayload::AuthStatus { statuses, phases }) => {
                        this.auth_statuses = statuses
                            .into_iter()
                            .map(|status| (status.provider.clone(), status))
                            .collect();
                        this.auth_phases = phases;
                        this.auth_error.clear();
                    }
                    Ok(_) => {
                        this.auth_error.insert(
                            ProviderId::new("all"),
                            "invalid auth status response".into(),
                        );
                    }
                    Err(error) => {
                        this.auth_error
                            .insert(ProviderId::new("all"), error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn refresh_provider_catalogs(&mut self, force: bool, cx: &mut Context<Self>) {
        self.model_catalog_generation = self.model_catalog_generation.wrapping_add(1);
        let generation = self.model_catalog_generation;
        let providers = self
            .state
            .external_providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect::<Vec<_>>();
        let client = self.daemon.client();
        self.model_catalog_pending.extend(providers.iter().cloned());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut catalogs = Vec::new();
                    for provider in providers {
                        let response = if force {
                            client.refresh_models(provider.clone())?
                        } else {
                            client.list_models(provider.clone())?
                        };
                        if let waku_client::ResponsePayload::Models { catalog } = response {
                            catalogs.push(catalog);
                        }
                    }
                    Ok::<_, anyhow::Error>(catalogs)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.model_catalog_generation {
                    return;
                }
                this.model_catalog_pending.clear();
                match result {
                    Ok(catalogs) => {
                        for catalog in catalogs {
                            this.model_catalogs
                                .insert(catalog.provider.clone(), catalog);
                        }
                        this.model_catalog_error.clear();
                    }
                    Err(error) => {
                        this.model_catalog_error
                            .insert(ProviderId::new("all"), error.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn start_provider_login(
        &mut self,
        provider: ProviderId,
        method: waku_client::LoginMethod,
        cx: &mut Context<Self>,
    ) {
        self.auth_generation = self.auth_generation.wrapping_add(1);
        let generation = self.auth_generation;
        self.auth_pending.insert(provider.clone());
        self.clear_auth_api_key(cx);
        let pending_provider = provider.clone();
        let client = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.start_login(provider, method) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.auth_generation {
                    return;
                }
                match result {
                    Ok(waku_client::ResponsePayload::Login { phase }) => {
                        match &phase {
                            waku_client::AuthPhase::AwaitingBrowser { url, .. }
                            | waku_client::AuthPhase::AwaitingDevice {
                                verification_url: url,
                                ..
                            } => cx.open_url(url),
                            _ => {}
                        }
                        this.auth_phases.push(phase);
                    }
                    Ok(_) => {
                        this.auth_error
                            .insert(ProviderId::new("all"), "invalid login response".into());
                    }
                    Err(error) => {
                        this.auth_error
                            .insert(ProviderId::new("all"), error.to_string());
                    }
                };
                this.auth_pending.remove(&pending_provider);
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_provider_login(&mut self, login_id: uuid::Uuid, cx: &mut Context<Self>) {
        self.clear_auth_api_key(cx);
        let client = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.cancel_login(login_id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.auth_error
                        .insert(ProviderId::new("all"), error.to_string());
                }
                this.refresh_provider_auth_statuses(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn complete_api_key_login(&mut self, provider: ProviderId, cx: &mut Context<Self>) {
        let key = self.auth_api_key_input.read(cx).content().trim().to_owned();
        if key.is_empty() {
            return;
        }
        let Some(login_id) = self.auth_phases.iter().find_map(|phase| {
            let waku_client::AuthPhase::AwaitingApiKey {
                login_id,
                provider: phase_provider,
                ..
            } = phase
            else {
                return None;
            };
            (phase_provider == &provider).then_some(*login_id)
        }) else {
            return;
        };
        self.clear_auth_api_key(cx);
        let client = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    client.complete_api_key_login(
                        login_id,
                        provider,
                        waku_client::SecretString::new(key),
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.auth_error
                        .insert(ProviderId::new("all"), error.to_string());
                }
                this.refresh_provider_auth_statuses(cx);
                this.refresh_provider_catalogs(true, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn clear_auth_api_key(&mut self, cx: &mut Context<Self>) {
        self.auth_api_key_input
            .update(cx, |input, cx| input.clear(cx));
    }

    fn logout_provider(&mut self, provider: ProviderId, cx: &mut Context<Self>) {
        self.clear_auth_api_key(cx);
        let client = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.logout(provider) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if result.is_ok() {
                    this.refresh_provider_auth_statuses(cx);
                    this.refresh_provider_catalogs(true, cx);
                } else if let Err(error) = result {
                    this.auth_error
                        .insert(ProviderId::new("all"), error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }
}
fn preset_login_methods(preset_id: &str) -> &'static [waku_client::LoginMethod] {
    match preset_id {
        waku_client::ProviderId::OPENAI_CODEX => &[
            waku_client::LoginMethod::OauthBrowser,
            waku_client::LoginMethod::OauthDevice,
        ],
        waku_client::ProviderId::XAI_OAUTH => &[waku_client::LoginMethod::OauthDevice],
        _ => &[waku_client::LoginMethod::ApiKey],
    }
}

fn login_method_label(method: waku_client::LoginMethod) -> String {
    match method {
        waku_client::LoginMethod::ApiKey => tr!("providers.connect").to_string(),
        waku_client::LoginMethod::OauthBrowser => tr!("providers.sign_in").to_string(),
        waku_client::LoginMethod::OauthDevice => tr!("providers.use_device_code").to_string(),
    }
}

fn auth_phase_summary(phase: &waku_client::AuthPhase) -> String {
    match phase {
        waku_client::AuthPhase::AwaitingBrowser { url, .. } => format!("Opening browser: {url}"),
        waku_client::AuthPhase::AwaitingDevice {
            user_code,
            verification_url,
            ..
        } => format!("Code {user_code} at {verification_url}"),
        waku_client::AuthPhase::AwaitingApiKey { instructions, .. } => instructions.clone(),
        waku_client::AuthPhase::Completed { .. } => tr!("providers.connected").to_string(),
        waku_client::AuthPhase::Failed { message, .. } => message.clone(),
        waku_client::AuthPhase::Idle => String::new(),
    }
}
struct ProviderDraftPolicy<'a> {
    id: &'a str,
    name: &'a str,
    base_url: &'a str,
    api_key_env: Option<&'a str>,
    models_text: &'a str,
    default_model: &'a str,
    context_window: &'a str,
    max_output_tokens: &'a str,
    headers_text: &'a str,
}

fn parse_provider_draft_policy(
    policy: ProviderDraftPolicy<'_>,
) -> Result<ExternalProvider, String> {
    let ProviderDraftPolicy {
        id,
        name,
        base_url,
        api_key_env,
        models_text,
        default_model,
        context_window,
        max_output_tokens,
        headers_text,
    } = policy;

    let models = models_text
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    if models
        .iter()
        .any(|model| !seen.insert(model.to_ascii_lowercase()))
    {
        return Err(tr!("providers.duplicate_models").to_owned());
    }
    let context_window = context_window
        .parse::<u64>()
        .map_err(|_| tr!("providers.invalid_context_window").to_owned())?;
    let max_output_tokens = max_output_tokens
        .parse::<u64>()
        .map_err(|_| tr!("providers.invalid_max_output_tokens").to_owned())?;
    let mut headers = Vec::new();
    let mut names = std::collections::HashSet::new();
    for line in headers_text.lines().filter(|line| !line.trim().is_empty()) {
        let Some((key, value)) = line.split_once(':') else {
            return Err(tr!("providers.invalid_header_format").to_owned());
        };
        let key = key.trim().to_owned();
        let value = value.trim().to_owned();
        let lower = key.to_ascii_lowercase();
        if key.is_empty()
            || value.is_empty()
            || !names.insert(lower.clone())
            || matches!(
                lower.as_str(),
                "authorization"
                    | "x-api-key"
                    | "host"
                    | "content-length"
                    | "content-type"
                    | "anthropic-version"
            )
        {
            return Err(tr!("providers.invalid_headers").to_owned());
        }
        headers.push((key, value));
    }
    let provider = ExternalProvider {
        id: ProviderId::new(id),
        name: name.into(),
        base_url: base_url.into(),
        api_format: Default::default(),
        api_key_env: api_key_env.map(str::to_owned),
        headers,
        models,
        default_model: default_model.into(),
        context_window,
        max_output_tokens,
    };
    provider.validate().map_err(|error| error.to_owned())?;
    Ok(provider)
}

#[cfg(test)]
fn auth_phase_url(phase: &waku_client::AuthPhase) -> Option<&str> {
    match phase {
        waku_client::AuthPhase::AwaitingBrowser { url, .. }
        | waku_client::AuthPhase::AwaitingDevice {
            verification_url: url,
            ..
        } => Some(url),
        _ => None,
    }
}

#[cfg(test)]
fn accepts_generation(current: u64, response: u64) -> bool {
    current == response
}

#[cfg(test)]
mod auth_behavior_tests {
    use super::{accepts_generation, auth_phase_url, preset_login_methods};
    use uuid::Uuid;
    use waku_client::{LoginMethod, ProviderId};

    #[test]
    fn browser_and_device_phases_provide_system_urls() {
        let browser = waku_client::AuthPhase::AwaitingBrowser {
            login_id: Uuid::nil(),
            provider: ProviderId::new("openai-codex"),
            url: "https://browser".into(),
        };
        let device = waku_client::AuthPhase::AwaitingDevice {
            login_id: Uuid::nil(),
            provider: ProviderId::new("openai-codex"),
            user_code: "CODE".into(),
            verification_url: "https://device".into(),
            instructions: "verify".into(),
        };
        assert_eq!(auth_phase_url(&browser), Some("https://browser"));
        assert_eq!(auth_phase_url(&device), Some("https://device"));
    }

    #[test]
    fn stale_auth_and_catalog_generations_are_discarded() {
        assert!(accepts_generation(7, 7));
        assert!(!accepts_generation(8, 7));
    }

    #[test]
    fn secret_material_is_redacted_from_debug_and_display() {
        let secret = waku_client::SecretString::new("not-persisted");
        assert!(!format!("{secret:?}").contains("not-persisted"));
        assert!(!secret.to_string().contains("not-persisted"));
    }

    #[test]
    fn codex_exposes_explicit_browser_and_device_login() {
        assert_eq!(
            preset_login_methods(waku_client::ProviderId::OPENAI_CODEX),
            [LoginMethod::OauthBrowser, LoginMethod::OauthDevice]
        );
    }

    #[test]
    fn auth_phases_are_scoped_to_the_login_owner() {
        let phase = waku_client::AuthPhase::AwaitingDevice {
            login_id: Uuid::nil(),
            provider: ProviderId::new("openai-codex"),
            user_code: "CODE".into(),
            verification_url: "https://device".into(),
            instructions: "verify".into(),
        };
        assert_eq!(
            phase.provider().map(ProviderId::as_str),
            Some("openai-codex")
        );
        assert_ne!(phase.provider().map(ProviderId::as_str), Some("xai-oauth"));
    }

    #[test]
    fn failed_login_always_carries_login_id_and_provider() {
        let phase = waku_client::AuthPhase::Failed {
            login_id: Uuid::nil(),
            provider: ProviderId::new("openai-codex"),
            message: "loopback unavailable".into(),
        };
        assert_eq!(phase.login_id(), Some(Uuid::nil()));
        assert_eq!(
            phase.provider().map(ProviderId::as_str),
            Some("openai-codex")
        );
    }
}
