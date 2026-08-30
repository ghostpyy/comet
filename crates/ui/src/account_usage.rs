//! Sidebar account-usage pill: which provider login the open chat spends
//! against, and how much of its plan is left.
//!
//! Sits directly under the user menu at the foot of the sidebar. Collapsed it
//! is two lines — brand mark, provider name, the tightest window's percentage,
//! and a meter. Clicking opens a card with the full settings-grade detail for
//! that account plus a scrollable list of every other login.
//!
//! Reads [`AppState::agent_accounts`], the same snapshot Settings → Agents
//! renders, so opening settings and watching the pill cost one provider probe
//! between them. Refresh policy lives in the shell (it owns window
//! activation); this module only draws what the snapshot says.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, Hsla, SharedString, Window, div, prelude::*, px,
};

use chrono::{DateTime, Utc};
use zeron_proto::{AgentAccount, AgentUsageWindow, HarnessId};

use crate::popover;
use crate::settings::accounts::{format_reset, usage_color, usage_level};
use crate::state::AppState;
use crate::theme::Theme;

/// Raised when the card's footer row is clicked. The shell owns settings
/// navigation, so the pill asks rather than reaching for it.
pub struct OpenAccountSettings;

/// Card width. Wider than a narrow sidebar on purpose — the meters and reset
/// labels need the room, and `snap_to_window_with_margin` keeps it on screen.
const CARD_WIDTH: f32 = 268.0;

/// The window a plan is closest to exhausting — the number worth one line of
/// sidebar. Ties keep the earlier window, which is the snapshot's own order
/// (Session before Week), so a fresh account showing 0%/0% reads "Session".
/// Pure.
pub fn tightest_window(windows: &[AgentUsageWindow]) -> Option<&AgentUsageWindow> {
    windows.iter().reduce(|worst, w| {
        if w.used_fraction > worst.used_fraction {
            w
        } else {
            worst
        }
    })
}

/// Display name for an account: the email it signed in with, else whatever
/// the provider gave us. Pure.
pub fn account_label(account: &AgentAccount) -> SharedString {
    account
        .email
        .clone()
        .or_else(|| account.display_name.clone())
        .unwrap_or_else(|| "Unknown account".into())
        .into()
}

/// Percent as shown next to a meter — rounded, with the same 0-vs-tiny floor
/// the meters use. Pure.
fn percent_label(fraction: f32) -> SharedString {
    format!("{}%", (fraction.clamp(0.0, 1.0) * 100.0).round() as u32).into()
}

/// A meter bar. `height` and `width` differ between the pill (thin, full
/// width) and the card rows, so the caller sizes it.
fn meter(fraction: f32, height: f32, theme: &Theme) -> AnyElement {
    let fraction = fraction.clamp(0.0, 1.0);
    let level = usage_level(fraction);
    let fill = usage_color(level, theme).opacity(0.85);
    div()
        .flex_1()
        .h(px(height))
        .rounded_full()
        .overflow_hidden()
        .bg(crate::theme::ink(0.07))
        .when(fraction > 0.0, |el| {
            el.child(
                div()
                    // Same 1.5% floor as the settings meters: tiny non-zero
                    // usage must still paint something.
                    .w(gpui::relative(fraction.max(0.015)))
                    .h_full()
                    .rounded_full()
                    .bg(fill),
            )
        })
        .into_any_element()
}

fn brand_mark(harness: HarnessId, size: f32, theme: &Theme) -> AnyElement {
    let (mark, tint) = crate::pickers::harness_brand_icon(harness);
    crate::icons::icon(mark)
        .size(px(size))
        .text_color(tint.unwrap_or(theme.text_muted))
        .into_any_element()
}

/// The sidebar pill. Owns only its popup state; the snapshot lives on
/// [`AppState`].
pub struct AccountUsagePill {
    state: Entity<AppState>,
    card: popover::Popup<()>,
    _observe: gpui::Subscription,
}

impl EventEmitter<OpenAccountSettings> for AccountUsagePill {}

impl AccountUsagePill {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            card: popover::Popup::default(),
            _observe: observe,
        }
    }

    fn close_card(&mut self, cx: &mut Context<Self>) {
        if self.card.begin_close() {
            popover::reap_popup(cx, |pill: &mut Self| &mut pill.card);
            cx.notify();
        }
    }

    /// The harness the open chat runs on. No chat selected means no claim
    /// about which login is being spent, so the pill hides rather than
    /// guessing from a remembered default.
    fn open_chat_harness(&self, cx: &Context<Self>) -> Option<HarnessId> {
        self.state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
            .map(|config| config.harness)
    }

    /// One row inside the card's "other accounts" list.
    fn render_other_row(
        &self,
        account: &AgentAccount,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let tightest = tightest_window(&account.usage_windows);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(brand_mark(account.harness, 12.0, theme)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.5))
                                    .text_color(theme.text_muted)
                                    .child(account_label(account)),
                            )
                            .when_some(tightest, |el, window| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(usage_color(
                                            usage_level(window.used_fraction),
                                            theme,
                                        ))
                                        .child(percent_label(window.used_fraction)),
                                )
                            }),
                    )
                    .map(|el| match tightest {
                        Some(window) => el.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .child(meter(window.used_fraction, 3.0, theme))
                                .when_some(format_reset(window.resets_at, now), |el, reset| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(theme.text_faint)
                                            .child(SharedString::from(reset)),
                                    )
                                }),
                        ),
                        None => el.child(
                            div()
                                .text_size(crate::typography::ui_rems(10.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from("Usage unavailable")),
                        ),
                    }),
            )
            .into_any_element()
    }

    /// The expanded card: the open chat's account in full, then every other
    /// login in a scroll region.
    fn render_card(
        &mut self,
        account: &AgentAccount,
        width: f32,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;

        let others: Vec<AnyElement> = {
            let state = self.state.read(cx);
            let snapshot = state.agent_accounts.ready();
            let rows: Vec<&AgentAccount> = snapshot
                .map(|s| {
                    s.accounts
                        .iter()
                        .filter(|a| a.id != account.id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut out = Vec::with_capacity(rows.len());
            for other in rows {
                out.push(self.render_other_row(other, theme, now));
            }
            out
        };
        let has_others = !others.is_empty();

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(brand_mark(account.harness, 15.0, theme)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(crate::pickers::harness_label(
                        account.harness,
                    ))),
            )
            .when(account.active, |el| {
                el.child(widgets::badge_active(theme, "Active"))
            });

        let identity = div()
            .mt(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .child(account_label(account)),
            )
            .when_some(account.plan_label.clone(), |el, plan| {
                el.child(widgets::badge(theme, plan))
            });

        // Every window, not just the tightest — the card is where the full
        // picture belongs.
        let meters: Vec<AnyElement> = account
            .usage_windows
            .iter()
            .map(|window| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(
                        div()
                            .w(px(46.0))
                            .flex_none()
                            .truncate()
                            .child(SharedString::from(window.label.clone())),
                    )
                    .child(meter(window.used_fraction, 4.0, theme))
                    .child(
                        div()
                            .w(px(34.0))
                            .flex_none()
                            .text_right()
                            .text_color(usage_color(usage_level(window.used_fraction), theme))
                            .child(percent_label(window.used_fraction)),
                    )
                    .when_some(format_reset(window.resets_at, now), |el, reset| {
                        el.child(
                            div()
                                .flex_none()
                                .truncate()
                                .text_color(theme.text_faint)
                                .child(SharedString::from(reset)),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        let body = div()
            .px(px(10.0))
            .pt(px(9.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .child(header)
            .child(identity)
            .map(|el| {
                if meters.is_empty() {
                    el.child(
                        div()
                            .mt(px(8.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from("Usage unavailable")),
                    )
                } else {
                    el.child(
                        div()
                            .mt(px(8.0))
                            .flex()
                            .flex_col()
                            .gap(px(5.0))
                            .children(meters),
                    )
                }
            });

        let card = popover::popover_card(theme)
            .w(px(width))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_card(cx)))
            .flex()
            .flex_col()
            .child(body)
            .when(has_others, |el| {
                el.child(
                    div()
                        .border_t_1()
                        .border_color(theme.border)
                        .pt(px(6.0))
                        .px(px(2.0))
                        .child(
                            div()
                                .px(px(8.0))
                                .pb(px(4.0))
                                .text_size(crate::typography::ui_rems(10.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from("Other accounts")),
                        )
                        .child(
                            // Capped so a long login list scrolls inside the
                            // card instead of growing past the window.
                            div()
                                .id("account-usage-others")
                                .max_h(px(190.0))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .children(others),
                        ),
                )
            })
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .p(px(2.0))
                    .child(
                        popover::menu_row(theme, false, "account-usage-manage")
                            .id("account-usage-manage")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_card(cx);
                                cx.emit(OpenAccountSettings);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::SETTINGS_MINIMALISTIC)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Manage accounts")),
                    ),
            );

        popover::anchored_menu_above(
            "account-usage-card",
            card.into_any_element(),
            self.card.closing_since(),
        )
    }
}

impl Render for AccountUsagePill {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let Some(harness) = self.open_chat_harness(cx) else {
            return div().into_any_element();
        };
        // Cloned once per render: the card's listeners need `&mut Context`,
        // which rules out holding a borrow of the shared state across them.
        // One account, not the whole snapshot.
        let Some(account) = self.state.read(cx).active_account_for(harness).cloned() else {
            return div().into_any_element();
        };

        let now = Utc::now();
        let open = self.card.is_open();
        let tightest = tightest_window(&account.usage_windows);
        let accent: Hsla = tightest
            .map(|w| usage_color(usage_level(w.used_fraction), &theme))
            .unwrap_or(theme.text_faint);

        let mut pill = div()
            .id("account-usage-pill")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .bg(if open {
                theme.glass_hover()
            } else {
                crate::motion::hover_blend(
                    "account-usage-pill",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(crate::motion::hover_listener("account-usage-pill"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.card.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.card.take_press_was_open() {
                    this.close_card(cx);
                } else {
                    this.card.open(());
                }
                cx.notify();
            }))
            .child(
                // 28px column aligns the text with the user name above it.
                div()
                    .flex_none()
                    .size(px(28.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(brand_mark(account.harness, 16.0, &theme)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(crate::pickers::harness_label(
                                        harness,
                                    ))),
                            )
                            .when_some(tightest, |el, window| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(12.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(accent)
                                        .child(percent_label(window.used_fraction)),
                                )
                            }),
                    )
                    .map(|el| match tightest {
                        Some(window) => el.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .child(meter(window.used_fraction, 4.0, &theme))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(
                                            match format_reset(window.resets_at, now) {
                                                Some(reset) => {
                                                    format!("{} · {reset}", window.label)
                                                }
                                                None => window.label.clone(),
                                            },
                                        )),
                                ),
                        ),
                        // No windows: the login is known but the provider did
                        // not report a plan. Say so instead of drawing an
                        // empty meter that reads as 0% used.
                        None => el.child(
                            div()
                                .text_size(crate::typography::ui_rems(10.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from("Usage unavailable")),
                        ),
                    }),
            );

        if self.card.get().is_some() {
            let card = self.render_card(&account, CARD_WIDTH, &theme, now, cx);
            pill = pill.child(card);
        }
        pill.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(label: &str, used: f32) -> AgentUsageWindow {
        AgentUsageWindow {
            label: label.into(),
            used_fraction: used,
            resets_at: None,
        }
    }

    #[test]
    fn tightest_window_picks_the_highest_fraction() {
        let windows = vec![window("Session", 0.73), window("Week", 0.82)];
        assert_eq!(tightest_window(&windows).unwrap().label, "Week");
    }

    #[test]
    fn tightest_window_keeps_the_earlier_window_on_a_tie() {
        let windows = vec![window("Session", 0.0), window("Week", 0.0)];
        assert_eq!(tightest_window(&windows).unwrap().label, "Session");
    }

    #[test]
    fn tightest_window_is_none_without_windows() {
        assert!(tightest_window(&[]).is_none());
    }

    #[test]
    fn percent_label_rounds() {
        assert_eq!(percent_label(0.825).as_ref(), "83%");
        assert_eq!(percent_label(0.0).as_ref(), "0%");
        // Fractions past 1.0 arrive when a provider reports an overage.
        assert_eq!(percent_label(1.4).as_ref(), "100%");
    }
}
