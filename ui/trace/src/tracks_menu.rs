use crate::groups::{PINNED_CLASS, STACKED_GROUPS, group_label};
use crate::model::TraceModel;
use crate::theme::Palette;
use crate::track_label::track_label;
use crate::track_order::TrackSort;
use crate::{
    CloseTracksMenu, FilterTracks, GUTTER_LABEL_SIZE, TICK_BAND_HEIGHT, TRACKS_MENU_FOLD_ABOVE,
    TRACKS_MENU_KEY_CONTEXT, TRACKS_MENU_MAX_LIST, TRACKS_MENU_WIDTH, TracePanel,
};
use bviz_trace_profile::{LaneClass, Profile};
use bviz_ui_components::{
    Hint, ListScroll, SCROLLBAR_THICKNESS, TextInput, TextTooltip, ToggleGroup, ToggleSegment,
    TruncatingLabel, list_scrollbar,
};
use gpui::{
    AnyElement, App, Context, Entity, Focusable, IntoElement, MouseButton, MouseDownEvent, Pixels,
    Render, SharedString, Subscription, WeakEntity, Window, deferred, div, prelude::*, px,
};

/// Side of the menu's caret icons.
const MENU_ICON: Pixels = px(12.);

/// The lane classes the menu lists a group of, in order: the pinned class,
/// then the stacked groups in the chart's order.
const LISTED_GROUPS: [LaneClass; STACKED_GROUPS.len() + 1] = {
    let mut listed = [PINNED_CLASS; STACKED_GROUPS.len() + 1];
    let mut i = 0;
    while i < STACKED_GROUPS.len() {
        listed[i + 1] = STACKED_GROUPS[i];
        i += 1;
    }
    listed
};

/// The tracks-visibility menu as a self-contained popover: a trigger in the
/// gutter corner where the ruler meets the track-label column, and, when open, a
/// filter field over a checklist of track names grouped by lane class. Owns its
/// open, fold, filter, scroll, and drag state as plain fields. Reads the visible
/// tracks from [`TraceModel`] and writes visibility
/// there; reflows the timeline through the owning [`TracePanel`].
pub(crate) struct TracksMenu {
    /// Whether the menu is open.
    open: bool,
    /// The reader's fold choice per lane-class group, indexed by [`LISTED_GROUPS`],
    /// `None` where they have made none and the list's own length decides.
    /// Independent of the chart's collapse so the menu can manage a group's
    /// visibility while its lanes are collapsed in the chart.
    folded_groups: [Option<bool>; LISTED_GROUPS.len()],
    /// The filter field; its text narrows the listed tracks.
    filter: Entity<TextInput>,
    /// Re-renders the menu as the filter text changes; `TextInput` notifies on
    /// every edit.
    _filter_observer: Subscription,
    /// Scroll position of the track list.
    scroll: ListScroll,
    /// Read for the visible tracks, written to toggle them.
    model: Entity<TraceModel>,
    /// The owning view, read for the gutter width that positions the popover,
    /// and updated to reflow the timeline after a visibility change.
    shell: WeakEntity<TracePanel>,
}

impl TracksMenu {
    pub(crate) fn new(
        model: Entity<TraceModel>,
        shell: WeakEntity<TracePanel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx, "Filter tracks…", 0));
        let filter_observer = cx.observe(&filter, |_, _, cx| cx.notify());
        Self {
            open: false,
            folded_groups: [None; LISTED_GROUPS.len()],
            filter,
            _filter_observer: filter_observer,
            scroll: ListScroll::new(),
            model,
            shell,
        }
    }

    /// Opens the menu with the cursor in its filter.
    pub(crate) fn open_filtering(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.filter.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Closes the menu, handing focus the filter held back to the timeline.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        if self.filter.read(cx).focus_handle(cx).is_focused(window)
            && let Some(shell) = self.shell.upgrade()
        {
            shell.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Resets the view-local fold state and list scroll for a freshly loaded
    /// profile. The document-derived visibility is seeded by the model instead.
    pub(crate) fn reset_for_new_profile(&mut self) {
        self.folded_groups = [None; LISTED_GROUPS.len()];
        self.scroll.to_top();
    }

    /// Rebuilds the timeline's scene over the lanes as they stand, for a change
    /// to what the chart stacks or the order it stacks them in.
    fn reflow_timeline(&self, cx: &mut Context<Self>) {
        if let Some(shell) = self.shell.upgrade() {
            shell.update(cx, |tv, cx| tv.invalidate_timeline(cx));
        }
    }

    /// Whether a group listing `listed` tracks is folded: the reader's choice,
    /// and until they make one, whether the list is long enough to bury the
    /// groups below it.
    fn folded(&self, group: usize, listed: usize) -> bool {
        self.folded_groups[group].unwrap_or(listed > TRACKS_MENU_FOLD_ABOVE)
    }

    /// Folds or unfolds a lane-class group's track list.
    fn toggle_group_folded(&mut self, group: usize, listed: usize) {
        self.folded_groups[group] = Some(!self.folded(group, listed));
    }

    /// The open menu's deferred body: a filter field over the filtered track
    /// checklist. `None` when the menu is closed.
    fn popover(
        &self,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        if !self.open {
            return None;
        }
        let profile = self.model.read(cx).profile()?.clone();
        let rows = self.menu_rows(&profile, palette, cx);
        let menu = self.chrome(rows, palette, cx);
        Some(deferred(
            div()
                .key_context(TRACKS_MENU_KEY_CONTEXT)
                .on_action(
                    cx.listener(|this, _: &CloseTracksMenu, window, cx| this.close(window, cx)),
                )
                .child(menu),
        ))
    }

    /// Tracks under their lane-class group headers, each group's tracks in the
    /// standing [`TrackSort`] — the same grouping and order the chart lays out.
    fn menu_rows(
        &self,
        profile: &Profile,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let filter = self.filter.read(cx).content().to_lowercase();
        let threads = profile.threads();
        let shared_names = self.model.read(cx).shared_track_names().to_vec();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (group, &class) in LISTED_GROUPS.iter().enumerate() {
            let mut tracks: Vec<usize> = (0..threads.len())
                .filter(|&t| threads[t].lane_class == class)
                .filter(|&t| {
                    filter.is_empty()
                        || profile
                            .str(threads[t].name)
                            .to_lowercase()
                            .contains(&filter)
                })
                .collect();
            if tracks.is_empty() {
                continue;
            }
            self.model
                .read(cx)
                .track_sort()
                .order(profile, threads, &mut tracks);
            let folded = self.folded(group, tracks.len());
            rows.push(
                self.group_row(group, class, &tracks, folded, palette, cx)
                    .into_any_element(),
            );
            if folded {
                continue;
            }
            for i in tracks {
                let label = track_label(profile, &shared_names, i);
                let visible = self
                    .model
                    .read(cx)
                    .visible_tracks()
                    .get(i)
                    .copied()
                    .unwrap_or(true);
                rows.push(
                    self.track_row(i, &label, visible, palette, cx)
                        .into_any_element(),
                );
            }
        }
        rows
    }

    /// A group header: a caret and how many of the tracks listed under it are
    /// visible, folding the group's track list on click. Its trailing control
    /// toggles the whole group's visibility without also folding.
    fn group_row(
        &self,
        group: usize,
        class: LaneClass,
        tracks: &[usize],
        folded: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let n_visible = {
            let model = self.model.read(cx);
            tracks
                .iter()
                .filter(|&&t| model.visible_tracks().get(t).copied().unwrap_or(true))
                .count()
        };
        let group_all_visible = n_visible == tracks.len();
        let listed = tracks.len();
        div()
            .id(SharedString::from(format!("track-group-{group}")))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .mt_1()
            .px_2()
            .py_1()
            .cursor_pointer()
            .text_color(palette.label)
            .hover(move |s| s.bg(palette.hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_group_folded(group, listed);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        bviz_ui_assets::icon(if folded { "caret-right" } else { "caret-down" })
                            .size(MENU_ICON)
                            .text_color(palette.label),
                    )
                    .child(format!(
                        "{}  {}/{} visible",
                        group_label(class),
                        n_visible,
                        tracks.len()
                    )),
            )
            .child(
                div()
                    .id(SharedString::from(format!("track-group-vis-{group}")))
                    .px_1()
                    .rounded_sm()
                    .hover(move |s| s.bg(palette.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.model
                            .update(cx, |m, _| m.set_group_visible(class, !group_all_visible));
                        this.reflow_timeline(cx);
                        cx.notify();
                    }))
                    .child(if group_all_visible { "hide" } else { "show" }),
            )
    }

    /// The control choosing the order the chart's lanes and this list stand a
    /// group's tracks in, the standing one filled in.
    fn sort_row(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let standing = self.model.read(cx).track_sort();
        let choice = ToggleGroup::one_of(
            "tracks-sort",
            "order",
            [TrackSort::Name, TrackSort::Weight],
            standing,
            |sort| {
                let (label, hint) = match sort {
                    TrackSort::Name => ("name", "Sort tracks by name"),
                    TrackSort::Weight => ("busiest", "Busiest tracks first"),
                };
                ToggleSegment::new(label).tooltip(hint)
            },
            {
                let menu = cx.entity();
                move |sort, _, cx| menu.update(cx, |this, cx| this.choose_sort(sort, cx))
            },
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .mx_1()
            .px_2()
            .py_1()
            .child(div().text_color(palette.muted_label).child("order"))
            .child(choice)
    }

    /// Stands every group's tracks in `sort`, reflowing the timeline over the
    /// restacked lanes.
    fn choose_sort(&mut self, sort: TrackSort, cx: &mut Context<Self>) {
        if !self.model.update(cx, |m, _| m.set_track_sort(sort)) {
            return;
        }
        self.reflow_timeline(cx);
        cx.notify();
    }

    /// A track row: a visibility checkbox and the track's label, toggling the
    /// track on click.
    fn track_row(
        &self,
        i: usize,
        label: &str,
        visible: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let check = div()
            .size(px(13.))
            .flex_none()
            .rounded_sm()
            .border_1()
            .border_color(palette.label)
            .when(visible, |c| {
                c.bg(palette.accent).border_color(palette.accent)
            });
        div()
            .id(SharedString::from(format!("track-row-{i}")))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .pl_4()
            .pr_2()
            .py_1()
            .cursor_pointer()
            .rounded_sm()
            .hover(move |s| s.bg(palette.hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.update(cx, |m, _| m.toggle_track(i));
                this.reflow_timeline(cx);
                cx.notify();
            }))
            .child(check)
            .child(TruncatingLabel::new("label", label.to_string()).flex_1())
    }

    /// The menu container over the built rows: the filter field, the order
    /// control, a show/hide-all toggle, and the scrollable track list with its
    /// scroll bar. Clicks inside
    /// must not reach the backdrop and close it; a press on the scroll bar grabs
    /// its thumb.
    fn chrome(
        &self,
        rows: Vec<AnyElement>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        div()
            .occlude()
            .absolute()
            .left_0()
            .top_0()
            .w(px(TRACKS_MENU_WIDTH))
            .flex()
            .flex_col()
            .bg(palette.panel)
            .border_1()
            .border_color(palette.border)
            .rounded_md()
            .shadow_lg()
            .text_size(px(GUTTER_LABEL_SIZE))
            .text_color(palette.label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    this.scroll.press(e.position);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(div().p_1().child(self.filter.clone()))
            .child(self.sort_row(palette, cx))
            .child({
                let (nvis, ntot) = self.model.read(cx).visible_count();
                let all_visible = ntot > 0 && nvis == ntot;
                let hint = match all_visible {
                    true => "Hide all tracks",
                    false => "Show all tracks",
                };
                div()
                    .id("tracks-toggle-all")
                    .flex()
                    .flex_row()
                    .items_center()
                    .mx_1()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(palette.border)
                    .hover(move |s| s.bg(palette.hover))
                    .tooltip(TextTooltip::builder(hint.into(), palette.colors.tooltip()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model
                            .update(cx, |m, _| m.set_all_visible(!all_visible));
                        this.reflow_timeline(cx);
                        cx.notify();
                    }))
                    .child(if all_visible { "hide all" } else { "show all" })
            })
            .child(self.menu_list(rows, palette, cx))
    }

    /// The scrollable track list with a scroll bar over its right edge.
    fn menu_list(
        &self,
        rows: Vec<AnyElement>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let list = div()
            .id("tracks-menu-list")
            .flex()
            .flex_col()
            .w_full()
            .p_1()
            .pr(px(SCROLLBAR_THICKNESS))
            .max_h(px(TRACKS_MENU_MAX_LIST))
            .overflow_y_scroll()
            .track_scroll(self.scroll.handle())
            .children(rows);
        let bar = list_scrollbar(
            cx.entity(),
            |menu: &Self| &menu.scroll,
            |menu: &mut Self| &mut menu.scroll,
            palette.scroll,
        );
        div()
            .relative()
            .w_full()
            .flex()
            .min_h_0()
            .child(list)
            .child(bar)
    }

    /// A backdrop behind the menu over the viewport below the trigger's band,
    /// closing the menu on an outside click.
    fn backdrop(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        deferred(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(TICK_BAND_HEIGHT))
                .bottom_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.close(window, cx);
                        cx.stop_propagation();
                    }),
                ),
        )
    }
}

impl Render for TracksMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let Some(shell) = self.shell.upgrade() else {
            return div().into_any_element();
        };
        let (gutter_width, focus) = {
            let s = shell.read(cx);
            (s.gutter_width, s.focus_handle(cx))
        };
        let menu = cx.weak_entity();
        let hint = Hint::default()
            .text("Tracks shown")
            .keys_for(&FilterTracks, &focus);
        let (nvis, ntot) = self.model.read(cx).visible_count();
        let trigger = div()
            .id("tracks-menu")
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_2()
            .gap_1()
            .cursor_pointer()
            .text_size(px(GUTTER_LABEL_SIZE))
            .text_color(palette.label)
            .bg(palette.header_band)
            .hover(move |s| s.bg(palette.hover))
            .map(|trigger| {
                let open = |menu: &Self| menu.open;
                hint.on_picker(trigger, palette.colors.tooltip(), menu, open)
            })
            // Keep the press off the chart handlers underneath.
            .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| {
                cx.stop_propagation()
            })
            .on_click(cx.listener(|this, _, window, cx| {
                if this.open {
                    this.close(window, cx);
                } else {
                    this.open_filtering(window, cx);
                }
            }))
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(format!("{nvis}/{ntot} tracks")),
            )
            .child(
                bviz_ui_assets::icon("caret-down")
                    .size(MENU_ICON)
                    .text_color(palette.label),
            );
        let popover = self.popover(palette, cx);
        let backdrop = popover.is_some().then(|| self.backdrop(cx));
        // An anchor at the corner holds the trigger and, below it, the popover.
        // The popover is an absolute child of this anchor (not a flex sibling of
        // the trigger row), so it opens directly under the trigger and does not
        // shift the trigger's own layout.
        let anchor = div()
            .absolute()
            .left_0()
            .top_0()
            .w(px(gutter_width))
            .h(px(TICK_BAND_HEIGHT))
            .child(trigger)
            .children(popover);
        div()
            .absolute()
            .inset_0()
            .children(backdrop)
            .child(anchor)
            .into_any_element()
    }
}
