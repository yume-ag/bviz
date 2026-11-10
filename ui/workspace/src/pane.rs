use crate::drop::Band;
use crate::gestures::{CHROME_GESTURES, ChromeIntent, OPEN_PANEL_MENU};
use crate::panel::keys_focus;
use crate::workspace::{ClosePanel, ShowKeysAndGestures, ToggleMaximize};
use crate::{AnyPanel, Caching, Documents, PanelEvent};
use bviz_ui_components::gesture::{Pointer, scroll_pixels};
use bviz_ui_components::{
    Button, KeysHelp, KeysHelpEvent, TextTooltip, compact_band_end, overlay_canvas,
};
use bviz_ui_theme::ThemeColors;
use gpui::{
    Anchor, AnyElement, AnyView, App, Bounds, ClickEvent, Div, ElementId, Entity, EntityId,
    EventEmitter, FocusHandle, Hsla, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, Render, ScrollHandle, ScrollWheelEvent, StyleRefinement, Subscription, Window,
    anchored, deferred, div, linear_color_stop, linear_gradient, point, prelude::*, px, size,
};
use std::cmp::Ordering;
use std::iter;

/// Height of the tab strip, giving a tab's icon, label and close control a line
/// to sit on with room around them.
const TAB_STRIP_HEIGHT: Pixels = px(30.);

/// Height of the handle over a pane holding one panel: a line for the panel's
/// name and nothing besides, so that naming a pane costs the panel as little of
/// the window as it can.
const HANDLE_HEIGHT: Pixels = px(24.);

/// How wide a tab may grow before its label is cut, so one long title cannot
/// push its neighbours off the strip.
const MAX_TAB_WIDTH: Pixels = px(200.);

/// How narrow a tab shrinks before the strip scrolls instead: room for its
/// icon, its close control and the number of its document, with a few letters
/// of its title ahead of the cut.
const MIN_TAB_WIDTH: Pixels = px(120.);

/// How far into the strip the fade over an end it has scrolled tabs past
/// reaches.
const OVERFLOW_FADE_WIDTH: Pixels = px(24.);

/// How near an end of its scroll a strip is taken to stand at it: gpui rounds
/// the furthest a strip scrolls, and leaves where a reveal scrolls it to
/// unrounded.
const SCROLL_END_SLACK: Pixels = px(1.);

const TAB_ICON_SIZE: Pixels = px(14.);

const HANDLE_ICON_SIZE: Pixels = px(12.);

const RESTORE_ICON: &str = "arrows-in";

const MENU_ICON: &str = "dots-three-vertical";

const KEYS_HELP_ICON: &str = "question";

/// How far the keys help stands in from the pane's corner and from the
/// window's edges.
const KEYS_HELP_MARGIN: Pixels = px(8.);

/// The shortest the keys help stands over a pane too short to hold it, room
/// for its filter and a few lines below it.
const KEYS_HELP_MIN_HEIGHT: Pixels = px(200.);

/// How a window frames a pane, as its workspace last laid the arrangement out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Framing {
    /// The window lays out no pane besides this one, so this one has the
    /// window's body to itself. `elsewhere` tells whether the session holds
    /// another window open, which is the only place left to carry a panel this
    /// pane holds alone.
    Lone { elsewhere: bool },
    /// The window lays other panes out beside this one.
    Beside,
    /// The window draws this pane in place of the whole arrangement, holding
    /// the panes it lays out besides this one back until it is restored.
    Maximized,
}

/// What a pane puts up over the panel it shows to name the panels it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Naming {
    /// A tab for each of the several panels the pane holds, to pick between.
    Strip,
    /// A line naming the one panel the pane holds, and nothing besides.
    Handle,
    /// The strip a drop joining the pane would make of its handle: a tab for
    /// the panel the pane holds, so that the boundaries a drag is aimed at are
    /// the boundaries of the row it would land in.
    Row,
}

impl Naming {
    /// How tall the band is drawn. A handle and the row put up in its place
    /// stand at one height, so that the region a drag over a pane holding one
    /// panel is aimed at is the same whichever of them the pane puts up, and
    /// the panel below keeps the room it had.
    fn height(self) -> Pixels {
        match self {
            Self::Strip => TAB_STRIP_HEIGHT,
            Self::Handle | Self::Row => HANDLE_HEIGHT,
        }
    }
}

/// The band a frame drew over the panels a pane holds, as that frame laid it
/// out.
struct DrawnBand {
    /// Which band it drew, so that one the pane has since put up in its place
    /// is told from it.
    naming: Naming,
    bounds: Bounds<Pixels>,
    /// The tabs as [`Band::tabs`] reads them; empty for a band that puts up
    /// no tabs of its own.
    tabs: Vec<Bounds<Pixels>>,
}

/// A panel of a pane, and the subscription carrying that panel's events to the
/// pane. Taking the panel out of the pane retires the subscription with it.
struct Tab {
    panel: AnyPanel,
    _events: Subscription,
}

/// Panels sharing one place in a window, of which the pane shows one and lists
/// the rest in a tab strip above it. A pane holds at least one panel, `active`
/// indexes the one it shows, and closing the last panel closes the pane itself
/// ([`PaneEvent::Closed`]) rather than leaving an empty one in the window.
pub struct Pane {
    tabs: Vec<Tab>,
    active: usize,
    framing: Framing,
    /// Whether this is the window's active pane, whose panel on show the
    /// window's panel keys act on.
    window_active: bool,
    /// Whether the click the pane last took was on the control that gives the
    /// window's body back.
    restore_clicked: bool,
    /// Whether a drop where a drag is holding the panel it carries would join
    /// this pane's tabs, which takes the whole window's arrangement and the
    /// drag across it to tell, so the workspace tells it.
    joined_by_drag: bool,
    /// The band the last frame drew over the panels the pane holds, which a
    /// gesture between frames reads to tell where a drop joining them lands.
    /// `None` until a frame has drawn one, and while the pane names its panels
    /// not at all.
    band: Option<DrawnBand>,
    /// How far the tabs are scrolled along the strip, and where the frame last
    /// laid them out.
    tab_scroll: ScrollHandle,
    /// The height the last frame laid the shown panel out at, which the keys
    /// help over it is kept within. `None` until a frame has drawn the pane.
    body_height: Option<Pixels>,
    keys_help: Option<OpenHelp>,
}

/// The keys help a pane puts up over the panel it shows, and the subscription
/// letting it go once it closes.
struct OpenHelp {
    help: Entity<KeysHelp>,
    _dismissed: Subscription,
}

/// What handing a panel over left of the pane that held it. The panel is
/// reached only through this, so a caller taking a pane's last panel is handed
/// the pane's own fate along with it.
#[must_use]
pub(crate) enum Handover {
    /// The pane holds other panels and shows one of them.
    Kept(AnyPanel),
    /// That was the pane's only panel, so the pane has nothing of its own left
    /// to show: it goes on holding the panel until its workspace takes it out of
    /// the arrangement, which the workspace does as part of the move.
    Emptied(AnyPanel),
}

/// What a [`Pane`] tells its workspace about itself.
pub enum PaneEvent {
    /// The panel on show changed, or changed what it says about itself.
    Changed,
    /// The last panel closed, so the pane is done.
    Closed,
    /// The pane is asking for the whole of the window's body, or to give it
    /// back where it already has it.
    Maximize,
    /// A press landed on the tab naming that panel, at that point of the
    /// window. The press is a drag of the panel from there if it is carried far
    /// enough, which only the workspace can tell, the drag running across the
    /// whole window.
    Pressed { panel: EntityId, at: Point<Pixels> },
    /// The button came up over a tab, at that point of the window. The handlers
    /// carrying a drag across the window are installed by a paint, so a press
    /// and a release inside one frame never reach them; the release the tab
    /// itself takes ends the gesture in that case.
    Released { at: Point<Pixels> },
    /// The menu of rearrangements that panel offers was asked for, to stand at
    /// that point of the window. What the menu offers takes the whole window's
    /// arrangement to tell, which only the workspace knows.
    Menu { panel: EntityId, at: Point<Pixels> },
    /// That panel asked to be shown, which takes making this the active pane,
    /// and only the workspace can.
    Reveal(EntityId),
    /// The keys help of that panel was asked to open or close, which takes
    /// showing the panel and focusing it, and only the workspace can.
    KeysHelp(EntityId),
}

impl EventEmitter<PaneEvent> for Pane {}

impl Pane {
    pub(crate) fn new(panel: AnyPanel, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let events = panel.subscribe(window, cx);
        let mut pane = Self {
            tabs: vec![Tab {
                panel,
                _events: events,
            }],
            active: 0,
            framing: Framing::Lone { elsewhere: false },
            window_active: false,
            restore_clicked: false,
            joined_by_drag: false,
            band: None,
            tab_scroll: ScrollHandle::new(),
            body_height: None,
            keys_help: None,
        };
        pane.show_active(window, cx);
        pane
    }

    /// Adds `panel` to the pane and shows it.
    pub(crate) fn add(&mut self, panel: AnyPanel, window: &mut Window, cx: &mut Context<Self>) {
        self.insert(self.tabs.len(), panel, window, cx);
    }

    /// Adds `panel` at `index` among the tabs — at the end where the pane holds
    /// fewer — and shows it.
    pub(crate) fn insert(
        &mut self,
        index: usize,
        panel: AnyPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let events = panel.subscribe(window, cx);
        let showing = self.active_panel().clone();
        let index = index.min(self.tabs.len());
        self.tabs.insert(
            index,
            Tab {
                panel,
                _events: events,
            },
        );
        showing.set_active(false, window, cx);
        self.active = index;
        self.show_active(window, cx);
    }

    pub fn active_panel(&self) -> &AnyPanel {
        &self.tabs[self.active].panel
    }

    /// How many panels the pane holds, one of which it shows.
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub(crate) fn set_framing(&mut self, framing: Framing) {
        self.framing = framing;
    }

    pub(crate) fn set_window_active(&mut self, active: bool) {
        self.window_active = active;
    }

    /// Scrolls the tab on show into view in the frame drawn next.
    pub(crate) fn reveal_shown_tab(&self) {
        self.tab_scroll.scroll_to_item(self.active);
    }

    /// The focus this pane's control acting on `panel` looks up the keys it
    /// names in, as [`keys_focus`] gives it.
    fn keys_focus(&self, panel: EntityId, cx: &App) -> Option<FocusHandle> {
        let active = self.window_active.then(|| self.active_panel());
        keys_focus(panel, active, cx)
    }

    pub(crate) fn set_joined_by_drag(&mut self, joined: bool) {
        self.joined_by_drag = joined;
    }

    /// The band the pane puts up over its panels, as the last frame laid it
    /// out. Its tabs come with it only where that frame drew the band the pane
    /// puts up now, the boundaries within a band being known to the frame that
    /// draws it and no earlier one.
    pub(crate) fn band(&self) -> Option<Band> {
        let drawn = self.band.as_ref()?;
        Some(Band {
            bounds: drawn.bounds,
            tabs: match self.naming() == Some(drawn.naming) {
                true => drawn.tabs.clone(),
                false => Vec::new(),
            },
        })
    }

    fn naming(&self) -> Option<Naming> {
        naming(self.tabs.len(), self.framing, self.joined_by_drag)
    }

    /// The panels the pane holds, in the order it holds them.
    pub(crate) fn panels(&self) -> impl Iterator<Item = &AnyPanel> {
        self.tabs.iter().map(|tab| &tab.panel)
    }

    /// The panel the pane holds at `index` of its tabs.
    pub(crate) fn panel_at(&self, index: usize) -> Option<&AnyPanel> {
        Some(&self.tabs.get(index)?.panel)
    }

    /// Which tab holds `panel`, if this pane holds it at all.
    pub(crate) fn index_of(&self, panel: EntityId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panel.entity_id() == panel)
    }

    /// The tab the pane shows.
    pub(crate) fn showing(&self) -> usize {
        self.active
    }

    /// Shows the panel at `index` in place of the one on show.
    pub fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.active || index >= self.tabs.len() {
            return;
        }
        self.tabs[self.active].panel.set_active(false, window, cx);
        self.active = index;
        self.show_active(window, cx);
    }

    /// Shows the panel after the one on show, wrapping round to the first.
    pub fn activate_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.activate((self.active + 1) % self.tabs.len(), window, cx);
    }

    /// Shows the panel before the one on show, wrapping round to the last.
    pub fn activate_previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.tabs.len() - 1;
        self.activate(self.active.checked_sub(1).unwrap_or(last), window, cx);
    }

    /// Closes the panel at `index`, or the whole pane where that panel is the
    /// only one it holds.
    pub fn close(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            cx.emit(PaneEvent::Closed);
            return;
        }
        drop(self.detach(index, window, cx));
    }

    pub fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(self.active, window, cx);
    }

    /// Hands `panel` to a caller that re-homes it, whether or not the pane holds
    /// others. `None` where this pane does not hold it.
    pub(crate) fn remove(
        &mut self,
        panel: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Handover> {
        let index = self.index_of(panel)?;
        if let Some(taken) = self.detach(index, window, cx) {
            return Some(Handover::Kept(taken));
        }
        let only = self.active_panel().clone();
        only.set_active(false, window, cx);
        Some(Handover::Emptied(only))
    }

    /// Shows again the panel a [`Handover::Emptied`] handed over, for a
    /// caller that found nowhere to re-home it.
    pub(crate) fn take_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_active(window, cx);
    }

    pub(crate) fn on_panel_event(
        &mut self,
        panel: EntityId,
        event: &PanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.index_of(panel) else {
            return;
        };
        match event {
            PanelEvent::StatusChanged => {
                if index == self.active {
                    cx.emit(PaneEvent::Changed);
                }
                cx.notify();
            }
            PanelEvent::Close => self.close(index, window, cx),
            PanelEvent::Activate => cx.emit(PaneEvent::Reveal(panel)),
        }
    }

    /// Takes the panel at `index` out of the pane, moving on to another panel
    /// where it was the one on show. `None` where the pane does not hold such a
    /// panel, or holds only it.
    fn detach(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyPanel> {
        if index >= self.tabs.len() || self.tabs.len() == 1 {
            return None;
        }
        let taken = self.tabs.remove(index).panel;
        let showing = self.active;
        self.active = showing_after(showing, index, self.tabs.len());
        if index == showing {
            taken.set_active(false, window, cx);
            self.show_active(window, cx);
        } else {
            cx.notify();
        }
        Some(taken)
    }

    /// Brings the panel `active` indexes forward, its tab into view, and tells
    /// the workspace what the pane is showing.
    fn show_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = self.active_panel().clone();
        panel.set_active(true, window, cx);
        self.reveal_shown_tab();
        self.mark_shown(cx);
        cx.emit(PaneEvent::Changed);
    }

    /// Marks the panel on show for a repaint of its own, which a caller that
    /// brings it back into the frames the window draws owes it: a repaint that
    /// panel marked while no frame drew it — because its pane showed another,
    /// or because no frame drew its pane at all — was consumed by a frame that
    /// left it out, so a frame drawing it again records the scene it draws only
    /// if it is marked afresh.
    pub(crate) fn mark_shown(&self, cx: &mut Context<Self>) {
        self.active_panel().notify(cx);
        cx.notify();
    }

    pub(crate) fn shows_keys_help(&self) -> bool {
        self.keys_help.is_some()
    }

    /// Puts `help` up over the panel on show, in place of any it put up
    /// before. Once it closes holding focus, focus goes back to that panel.
    pub(crate) fn show_keys_help(
        &mut self,
        help: Entity<KeysHelp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dismissed = cx.subscribe_in(&help, window, |pane, _, event, window, cx| {
            let KeysHelpEvent::Dismissed { holding_focus } = *event;
            pane.keys_help = None;
            if holding_focus {
                window.focus(&pane.active_panel().focus_handle(cx), cx);
            }
            cx.notify();
        });
        self.keys_help = Some(OpenHelp {
            help,
            _dismissed: dismissed,
        });
        cx.notify();
    }

    /// The tallest the keys help may stand over the shown panel.
    pub(crate) fn keys_help_room(&self, window: &Window) -> Pixels {
        keys_help_room(self.body_height.unwrap_or(window.viewport_size().height))
    }

    pub(crate) fn close_keys_help(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = &self.keys_help {
            open.help.update(cx, |help, cx| help.close(window, cx));
        }
    }

    /// The keys help, standing in from the top right corner of the panel below
    /// and kept inside the window.
    fn keys_help_overlay(&self) -> Option<impl IntoElement + use<>> {
        let help = self.keys_help.as_ref()?.help.clone();
        Some(
            div()
                .absolute()
                .top(KEYS_HELP_MARGIN)
                .right(KEYS_HELP_MARGIN)
                .child(deferred(
                    anchored()
                        .anchor(Anchor::TopRight)
                        .snap_to_window_with_margin(KEYS_HELP_MARGIN)
                        .child(help),
                )),
        )
    }

    fn tab_strip(
        &self,
        naming: Naming,
        colors: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut tabs = Vec::with_capacity(self.tabs.len());
        for index in 0..self.tabs.len() {
            tabs.push(self.tab(index, colors, cx));
        }
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .w_full()
            .h(naming.height())
            .pr(compact_band_end())
            .overflow_hidden()
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.border)
            .text_sm()
            // The tabs scroll in a container of their own, so that what a drag
            // reads as the boundaries between them is the tabs and nothing else
            // the strip carries, and the controls after them stay in view.
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .child(
                        div()
                            .id(ElementId::NamedInteger(
                                "pane-tabs".into(),
                                cx.entity_id().as_u64(),
                            ))
                            .flex()
                            .flex_row()
                            .items_center()
                            .size_full()
                            .overflow_x_scroll()
                            .track_scroll(&self.tab_scroll)
                            .children(tabs),
                    )
                    .child(overflow_fades(self.tab_scroll.clone(), colors.surface)),
            )
            .children(self.restore_control(cx))
            .child(self.keys_help_control(cx))
            .child(self.menu_control(cx))
    }

    /// The control that gives the window's body back to every pane, put up by
    /// the pane drawn in place of them. No other pane puts one up: a control on
    /// every pane would crowd both the close controls of a tab strip and the
    /// single line a handle is.
    fn restore_control(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.framing != Framing::Maximized {
            return None;
        }
        let pane = cx.entity();
        let id = pane.entity_id().as_u64();
        Some(control(
            Button::new(
                ElementId::NamedInteger("pane-restore".into(), id),
                move |_, _, cx| {
                    pane.update(cx, |pane, cx| {
                        pane.restore_clicked = true;
                        cx.emit(PaneEvent::Maximize);
                    });
                },
            )
            .ghost()
            .icon(RESTORE_ICON)
            .compact()
            .keys_in(
                &ToggleMaximize,
                self.keys_focus(self.active_panel().entity_id(), cx)
                    .as_ref(),
            ),
        ))
    }

    /// The control that puts up the keys help of the panel on show.
    fn keys_help_control(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pane = cx.entity();
        let id = self.active_panel().entity_id();
        control(
            Button::new(
                ElementId::NamedInteger("pane-keys-help".into(), id.as_u64()),
                move |_, _, cx| pane.update(cx, |_, cx| cx.emit(PaneEvent::KeysHelp(id))),
            )
            .ghost()
            .icon(KEYS_HELP_ICON)
            .compact()
            .keys_in(&ShowKeysAndGestures, self.keys_focus(id, cx).as_ref()),
        )
    }

    /// The control that puts up the menu of rearrangements the panel on show
    /// offers, so the menu is there to be found without knowing the gesture
    /// that opens it too, which its hint names.
    fn menu_control(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pane = cx.entity();
        let panel = self.active_panel().entity_id();
        control(
            Button::new(
                ElementId::NamedInteger("pane-menu".into(), panel.as_u64()),
                move |event: &ClickEvent, _, cx| {
                    let at = event.position();
                    pane.update(cx, |_, cx| cx.emit(PaneEvent::Menu { panel, at }));
                },
            )
            .ghost()
            .icon(MENU_ICON)
            .compact()
            .gesture(OPEN_PANEL_MENU.row()),
        )
    }

    /// Acts on a click on the tab at `tab`, or on the pane's handle where that
    /// is `None`, as [`CHROME_GESTURES`] has it. A click on the control that
    /// gives the window's body back is neither: it leaves the pane restored,
    /// and the click the platform counts after it must not ask for the body
    /// straight back.
    fn clicked(
        &mut self,
        tab: Option<usize>,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restoring = std::mem::take(&mut self.restore_clicked);
        CHROME_GESTURES.dispatch(Pointer::click(event), |intent| match (intent, tab) {
            (ChromeIntent::Maximize, _) if !restoring => {
                cx.emit(PaneEvent::Maximize);
                true
            }
            (ChromeIntent::Activate, Some(index)) => {
                self.activate(index, window, cx);
                true
            }
            _ => false,
        });
    }

    /// The grip over a pane holding one panel, naming that panel as a tab strip
    /// names the several a pane picks between.
    fn handle(&self, colors: &ThemeColors, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.active_panel().clone();
        let ink = colors.text_muted;
        div()
            .id(ElementId::NamedInteger(
                "pane-handle".into(),
                panel.entity_id().as_u64(),
            ))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .w_full()
            .h(HANDLE_HEIGHT)
            .pl_2()
            .pr(compact_band_end())
            .overflow_hidden()
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.border)
            .text_sm()
            .text_color(ink)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, pressed(panel.entity_id(), cx))
            .on_mouse_down(MouseButton::Right, menu_asked(panel.entity_id(), cx))
            .on_mouse_up(MouseButton::Left, released(cx))
            .on_click(cx.listener(|pane, event: &ClickEvent, window, cx| {
                pane.clicked(None, event, window, cx);
            }))
            .tooltip(naming_tooltip(&panel, colors))
            .children(panel.icon(cx).map(|name| {
                bviz_ui_assets::icon(name)
                    .size(HANDLE_ICON_SIZE)
                    .text_color(ink)
            }))
            .child(title(&panel))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_1()
                    .justify_end()
                    .children(self.restore_control(cx))
                    .child(self.keys_help_control(cx))
                    .child(self.menu_control(cx)),
            )
    }

    fn tab(&self, index: usize, colors: &ThemeColors, cx: &mut Context<Self>) -> AnyElement {
        let panel = self.tabs[index].panel.clone();
        let showing = index == self.active;
        let ink = if showing {
            colors.text
        } else {
            colors.text_muted
        };
        let hover = colors.element_hover;
        let pane = cx.entity();
        div()
            .id(ElementId::NamedInteger(
                "pane-tab".into(),
                panel.entity_id().as_u64(),
            ))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h_full()
            .px_2()
            .min_w(MIN_TAB_WIDTH)
            .max_w(MAX_TAB_WIDTH)
            .border_r_1()
            .border_color(colors.border)
            .bg(if showing {
                colors.background
            } else {
                colors.surface
            })
            .text_color(ink)
            .cursor_pointer()
            .when(!showing, |tab| tab.hover(move |s| s.bg(hover)))
            .on_mouse_down(MouseButton::Left, pressed(panel.entity_id(), cx))
            .on_mouse_down(MouseButton::Right, menu_asked(panel.entity_id(), cx))
            .on_mouse_up(MouseButton::Left, released(cx))
            .on_scroll_wheel(scrolled(cx))
            .on_click(cx.listener(move |pane, event: &ClickEvent, window, cx| {
                pane.clicked(Some(index), event, window, cx);
            }))
            .tooltip(naming_tooltip(&panel, colors))
            .children(panel.icon(cx).map(|name| {
                bviz_ui_assets::icon(name)
                    .flex_shrink_0()
                    .size(TAB_ICON_SIZE)
                    .text_color(ink)
            }))
            .child(title(&panel))
            .child(
                div()
                    .flex_shrink_0()
                    // A press on the close control must not also reach the tab
                    // and bring forward the panel it is about to close.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(
                            ElementId::NamedInteger(
                                "pane-tab-close".into(),
                                panel.entity_id().as_u64(),
                            ),
                            move |_, window, cx| {
                                pane.update(cx, |pane, cx| pane.close(index, window, cx));
                            },
                        )
                        .ghost()
                        .icon("x")
                        .compact()
                        .keys_in(&ClosePanel, self.keys_focus(panel.entity_id(), cx).as_ref()),
                    ),
            )
            .into_any_element()
    }
}

/// A control of the strip or handle a pane puts up over the panel it shows. A
/// handle reads a press on it as the start of a drag of its panel, which a
/// press on a control it carries is not.
fn control(button: Button) -> Div {
    div()
        .flex_shrink_0()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(button)
}

/// Tells the pane a press landed on the tab naming `panel`, which the workspace
/// reads as the start of a drag of that panel.
fn pressed(
    panel: EntityId,
    cx: &mut Context<Pane>,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |_, event: &MouseDownEvent, _, cx| {
        CHROME_GESTURES.dispatch(Pointer::press(event), |intent| match intent {
            ChromeIntent::Move => {
                cx.emit(PaneEvent::Pressed {
                    panel,
                    at: event.position,
                });
                true
            }
            _ => false,
        });
    })
}

/// Tells the pane the button came up over a tab, which the workspace reads as
/// the end of the gesture that press began.
fn released(cx: &mut Context<Pane>) -> impl Fn(&MouseUpEvent, &mut Window, &mut App) + 'static {
    cx.listener(|_, event: &MouseUpEvent, _, cx| {
        cx.emit(PaneEvent::Released { at: event.position });
    })
}

/// Tells the pane the menu of `panel` was asked for where the press landed,
/// which the workspace puts up over the window.
fn menu_asked(
    panel: EntityId,
    cx: &mut Context<Pane>,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |_, event: &MouseDownEvent, _, cx| {
        cx.stop_propagation();
        CHROME_GESTURES.dispatch(Pointer::press(event), |intent| match intent {
            ChromeIntent::OpenMenu => {
                cx.emit(PaneEvent::Menu {
                    panel,
                    at: event.position,
                });
                true
            }
            _ => false,
        });
    })
}

/// Scrolls the tabs along the strip, as [`CHROME_GESTURES`] has it.
fn scrolled(cx: &mut Context<Pane>) -> impl Fn(&ScrollWheelEvent, &mut Window, &mut App) + 'static {
    cx.listener(|pane, event: &ScrollWheelEvent, _, cx| {
        CHROME_GESTURES.dispatch(Pointer::scroll(event), |intent| match intent {
            ChromeIntent::ScrollTabs => {
                // The strip's own scroll would move the tabs a second time.
                cx.stop_propagation();
                let scroll = &pane.tab_scroll;
                let mut offset = scroll.offset();
                let along = scrolled_along(offset.x, scroll_pixels(event), scroll.max_offset().x);
                if along != offset.x {
                    offset.x = along;
                    scroll.set_offset(offset);
                    cx.notify();
                }
                true
            }
            _ => false,
        });
    })
}

/// Where tabs scrolled to `offset`, at most `max` past their start, stand once
/// a wheel turns by `delta`. A strip scrolls along one axis only, so it takes
/// whichever axis the wheel turns furthest on — a mouse without a horizontal
/// wheel turns only the other.
fn scrolled_along(offset: Pixels, delta: Point<Pixels>, max: Pixels) -> Pixels {
    let along = if delta.x.abs() >= delta.y.abs() {
        delta.x
    } else {
        delta.y
    };
    (offset + along).clamp(-max, px(0.))
}

/// As much of each tab as the strip scrolled by `scroll` shows, so that a tab
/// scrolled out of sight sits at the end of the strip it left by.
fn shown_tabs(scroll: &ScrollHandle) -> Vec<Bounds<Pixels>> {
    let strip = scroll.bounds();
    let offset = scroll.offset().x;
    let count = scroll.children_count();
    let mut tabs = Vec::with_capacity(count);
    tabs.extend(
        (0..count)
            .map_while(|index| scroll.bounds_for_item(index))
            .map(|tab| shown_part(tab, offset, strip)),
    );
    tabs
}

/// What `strip` shows of `tab`, laid out unscrolled and drawn scrolled by
/// `offset`: its span across the strip, empty at the end it lies beyond.
fn shown_part(tab: Bounds<Pixels>, offset: Pixels, strip: Bounds<Pixels>) -> Bounds<Pixels> {
    let clamp = |x: Pixels| x.clamp(strip.left(), strip.right());
    let left = clamp(tab.left() + offset);
    let right = clamp(tab.right() + offset);
    Bounds::new(point(left, tab.top()), size(right - left, tab.size.height))
}

/// Fades the tabs out toward each end of the strip that `scroll` has scrolled
/// tabs past, into the strip's `surface`, so that a strip cut at a tab's edge
/// still shows there is more of it.
fn overflow_fades(scroll: ScrollHandle, surface: Hsla) -> impl IntoElement {
    overlay_canvas(move |bounds, window, _| {
        let offset = scroll.offset().x;
        let max = scroll.max_offset().x;
        let clear = surface.opacity(0.);
        let fade = |left: Pixels, from: Hsla, to: Hsla| {
            let band = Bounds::new(
                point(left, bounds.top()),
                size(OVERFLOW_FADE_WIDTH, bounds.size.height),
            );
            let toward_right = 90.;
            gpui::fill(
                band,
                linear_gradient(
                    toward_right,
                    linear_color_stop(from, 0.),
                    linear_color_stop(to, 1.),
                ),
            )
        };
        if offset < -SCROLL_END_SLACK {
            window.paint_quad(fade(bounds.left(), surface, clear));
        }
        if offset > -max + SCROLL_END_SLACK {
            window.paint_quad(fade(bounds.right() - OVERFLOW_FADE_WIDTH, clear, surface));
        }
    })
}

/// The tallest the keys help may stand over a panel `height` tall: inside it
/// where the panel has room, and otherwise as tall as reads.
fn keys_help_room(height: Pixels) -> Pixels {
    (height - KEYS_HELP_MARGIN * 2.).max(KEYS_HELP_MIN_HEIGHT)
}

/// Takes up the height the shown panel was laid out at, refitting the keys
/// help over it to a height that changed.
fn body_laid_out(
    cx: &Context<Pane>,
) -> impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static {
    let pane = cx.entity();
    move |body, window, cx| {
        let Some(&bounds) = body.first() else {
            return;
        };
        let height = bounds.size.height;
        let refit = pane.update(cx, |pane, _| {
            let changed = pane.body_height.replace(height) != Some(height);
            let help = pane.keys_help.as_ref().map(|open| open.help.clone());
            help.filter(|_| changed)
        });
        if let Some(help) = refit {
            window.on_next_frame(move |_, cx| {
                help.update(cx, |help, cx| {
                    help.set_max_height(keys_help_room(height), cx)
                });
            });
        }
    }
}

/// Records where a frame laid the `naming` band out, for a drag over the pane
/// to read. A band drawn in place of another, or at another width, draws
/// boundaries no frame has read yet, so drawing one asks for a frame that aims
/// the drag at them.
fn band_laid_out(
    naming: Naming,
    cx: &Context<Pane>,
) -> impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static {
    let pane = cx.entity();
    move |band, window, cx| {
        let Some(&bounds) = band.first() else {
            return;
        };
        let relaid = pane.update(cx, |pane, _| {
            let drawn = pane.band.as_ref();
            let replaced = drawn.map(|band| band.naming) != Some(naming);
            let resized = drawn.map(|band| band.bounds.size.width) != Some(bounds.size.width);
            // A reveal asked for before the strip was laid out at this width
            // was measured against the width it had before.
            if replaced || resized {
                pane.reveal_shown_tab();
            }
            let tabs = match naming {
                Naming::Strip | Naming::Row => shown_tabs(&pane.tab_scroll),
                Naming::Handle => Vec::new(),
            };
            pane.band = Some(DrawnBand {
                naming,
                bounds,
                tabs,
            });
            replaced || resized
        });
        if relaid {
            // A notify from within the frame being drawn schedules nothing, so
            // the frame reading the band and revealing the tab on show is asked
            // for from the one after this.
            let pane = pane.clone();
            window.on_next_frame(move |_, cx| pane.update(cx, |_, cx| cx.notify()));
        }
    }
}

/// The tooltip a tab or handle naming `panel` shows, which reads the name of
/// the panel's document only once it is put up.
fn naming_tooltip(
    panel: &AnyPanel,
    colors: &ThemeColors,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let panel = panel.clone();
    let colors = colors.tooltip();
    move |_, cx| {
        let joined = Documents::global(cx).read(cx).joined_name(panel.document());
        let tip = naming_tip(
            &panel.display_title(),
            joined.as_deref(),
            panel.description(),
        );
        TextTooltip::view(tip.into(), None, colors, cx)
    }
}

/// What the tooltip over a panel's tab or handle reads, a line each: the
/// panel's whole title, the document `joined` names, and what the panel's
/// kind shows.
fn naming_tip(title: &str, joined: Option<&str>, description: Option<&str>) -> String {
    let lines = iter::once(title).chain(joined).chain(description);
    lines.collect::<Vec<_>>().join("\n")
}

/// A panel's name as the pane holding it puts it up: cut short where there is
/// no room for it, keeping the number of the document it belongs to in view.
fn title(panel: &AnyPanel) -> Div {
    div()
        .flex()
        .flex_row()
        .min_w(px(0.))
        .child(div().min_w(px(0.)).truncate().child(panel.plain_title()))
        .children(
            panel
                .title_suffix()
                .map(|suffix| div().flex_shrink_0().whitespace_nowrap().child(suffix)),
        )
}

/// How a pane holding `tabs` panels, framed as `framing`, names them: a tab
/// strip where it holds several; a [`Naming::Row`] where `joined_by_drag` says
/// a drop is aimed at joining the one panel it holds; and a handle where that
/// panel could be carried somewhere else — to another pane of its window, or to
/// another of the session's windows. `None` where it names them not at all: the
/// one panel of a window's one pane with nowhere to go is the window itself,
/// nothing to pick between and nothing to carry it to, so anything drawn over
/// it would only take room from it.
fn naming(tabs: usize, framing: Framing, joined_by_drag: bool) -> Option<Naming> {
    if tabs > 1 {
        return Some(Naming::Strip);
    }
    if matches!(framing, Framing::Lone { elsewhere: false }) {
        return None;
    }
    Some(match joined_by_drag {
        true => Naming::Row,
        false => Naming::Handle,
    })
}

/// Which of the `remaining` tabs a pane shows once the tab at `removed` has left
/// it and the tab at `showing` was the one on show: the tab that moves up into
/// the gap, or the last tab where the gap is past the end; and otherwise the
/// same tab, one place nearer the front where the tab that left sat before it.
fn showing_after(showing: usize, removed: usize, remaining: usize) -> usize {
    match removed.cmp(&showing) {
        Ordering::Equal => removed.min(remaining - 1),
        Ordering::Less => showing - 1,
        Ordering::Greater => showing,
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.active_panel();
        let body = match panel.caching() {
            Caching::Whole => (panel.view())
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
            Caching::Layers => panel.view().into_any_element(),
        };
        let help = self.keys_help_overlay();
        let Some(naming) = self.naming() else {
            self.band = None;
            return div()
                .relative()
                .flex()
                .flex_col()
                .size_full()
                .child(body)
                .children(help)
                .on_children_prepainted(body_laid_out(cx))
                .into_any_element();
        };
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let named = match naming {
            Naming::Handle => self.handle(&colors, cx).into_any_element(),
            Naming::Strip | Naming::Row => self.tab_strip(naming, &colors, cx).into_any_element(),
        };
        // The band is measured through a container of its own, so that what a
        // drag is aimed at is the band the pane draws rather than anything laid
        // out within it.
        let above = div()
            .flex_shrink_0()
            .w_full()
            .child(named)
            .on_children_prepainted(band_laid_out(naming, cx));
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(above)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .child(body)
                    .children(help)
                    .on_children_prepainted(body_laid_out(cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_naming_tip_reads_the_whole_title_then_the_document_then_the_kind() {
        let title = "Flame Graph \u{b7} build.profile.gz (2)";
        assert_eq!(naming_tip(title, None, None), title);
        assert_eq!(
            naming_tip(
                title,
                Some("Invocation a5f7f3e6"),
                Some("Time by call stack")
            ),
            format!("{title}\nInvocation a5f7f3e6\nTime by call stack")
        );
        assert_eq!(
            naming_tip(title, Some("Invocation a5f7f3e6"), None),
            format!("{title}\nInvocation a5f7f3e6")
        );
        assert_eq!(
            naming_tip(title, None, Some("Time by call stack")),
            format!("{title}\nTime by call stack")
        );
    }

    /// Several panels are picked between by their tabs, wherever the window
    /// lays the pane holding them out and whatever a drag is aimed at.
    #[test]
    fn a_pane_holding_several_panels_names_them_however_it_is_framed() {
        for framing in [
            Framing::Lone { elsewhere: false },
            Framing::Lone { elsewhere: true },
            Framing::Beside,
            Framing::Maximized,
        ] {
            for joined in [false, true] {
                assert_eq!(
                    naming(2, framing, joined),
                    Some(Naming::Strip),
                    "{framing:?}"
                );
            }
        }
    }

    /// A drag aimed at joining the one panel a pane holds is answered by a
    /// [`Naming::Row`] in place of the handle naming it, wherever the window
    /// lays that pane out.
    #[test]
    fn a_pane_a_drop_would_join_puts_up_the_row_it_would_make() {
        assert_eq!(naming(1, Framing::Beside, true), Some(Naming::Row));
        assert_eq!(
            naming(1, Framing::Lone { elsewhere: true }, true),
            Some(Naming::Row)
        );
    }

    /// A pane holding one panel names it at one height however it names it, and
    /// the strip naming several stands taller than both.
    #[test]
    fn the_row_a_drop_would_make_is_as_tall_as_the_handle_it_replaces() {
        assert_eq!(Naming::Row.height(), Naming::Handle.height());
        assert_ne!(Naming::Strip.height(), Naming::Handle.height());
    }

    /// A lone panel is named by the handle that carries it wherever it could
    /// go: to another pane of its window, or to another of the session's
    /// windows.
    #[test]
    fn a_lone_panel_is_named_wherever_it_could_be_carried() {
        assert_eq!(naming(1, Framing::Beside, false), Some(Naming::Handle));
        assert_eq!(naming(1, Framing::Maximized, false), Some(Naming::Handle));
        assert_eq!(
            naming(1, Framing::Lone { elsewhere: true }, false),
            Some(Naming::Handle)
        );
    }

    /// The one panel of the one pane of the session's one window has nowhere to
    /// be carried to, so nothing is drawn over it — and no drag of a panel can
    /// be running in a window whose one panel is the one being carried.
    #[test]
    fn the_lone_panel_of_a_lone_window_is_named_by_nothing() {
        let alone = Framing::Lone { elsewhere: false };
        assert_eq!(naming(1, alone, false), None);
        assert_eq!(naming(1, alone, true), None);
    }

    /// The panel handed over is the one on show, so the tab left showing is the
    /// one that moves up into the gap it leaves.
    #[test]
    fn the_tab_after_the_one_given_up_takes_its_place() {
        assert_eq!(showing_after(0, 0, 2), 0);
        assert_eq!(showing_after(1, 1, 3), 1);
    }

    /// There is no tab after the last one, so the strip falls back to the tab
    /// that becomes the last.
    #[test]
    fn giving_up_the_last_tab_falls_back_to_the_one_before_it() {
        assert_eq!(showing_after(2, 2, 2), 1);
        assert_eq!(showing_after(1, 1, 1), 0);
    }

    /// A tab taken from after the one on show leaves it where it is.
    #[test]
    fn a_tab_after_the_one_on_show_leaves_it_alone() {
        assert_eq!(showing_after(1, 2, 3), 1);
    }

    /// A tab taken from before the one on show moves it one place nearer the
    /// front, so the pane goes on showing the same panel.
    #[test]
    fn a_tab_before_the_one_on_show_moves_it_nearer_the_front() {
        assert_eq!(showing_after(2, 0, 3), 1);
        assert_eq!(showing_after(1, 0, 2), 0);
    }

    /// A wheel turning along the strip scrolls it, one turning only across it
    /// scrolls it just as far, and a sweep turning both is read along the axis
    /// it turns furthest on.
    #[test]
    fn either_wheel_scrolls_the_strip_along() {
        let max = px(300.);
        assert_eq!(
            scrolled_along(px(-100.), point(px(-40.), px(0.)), max),
            px(-140.)
        );
        assert_eq!(
            scrolled_along(px(-100.), point(px(0.), px(-40.)), max),
            px(-140.)
        );
        assert_eq!(
            scrolled_along(px(-100.), point(px(-40.), px(25.)), max),
            px(-140.),
            "a sweep mostly along the strip"
        );
        assert_eq!(
            scrolled_along(px(-100.), point(px(5.), px(-40.)), max),
            px(-140.),
            "a sweep mostly across the strip"
        );
    }

    /// The strip stops at its first tab and at its last.
    #[test]
    fn the_strip_scrolls_no_further_than_its_tabs() {
        let max = px(300.);
        assert_eq!(
            scrolled_along(px(-10.), point(px(0.), px(40.)), max),
            px(0.)
        );
        assert_eq!(
            scrolled_along(px(-290.), point(px(0.), px(-40.)), max),
            px(-300.)
        );
        assert_eq!(
            scrolled_along(px(0.), point(px(0.), px(-40.)), px(0.)),
            px(0.),
            "tabs that fit do not scroll"
        );
    }

    /// A tab scrolled partly out of the strip is aimed at by the part of it in
    /// view, and one wholly out of it by the end of the strip it left by.
    #[test]
    fn a_tab_is_shown_as_far_as_the_strip_shows_it() {
        let strip = Bounds::new(point(px(100.), px(0.)), size(px(300.), px(30.)));
        let tab = |left: f32| Bounds::new(point(px(left), px(0.)), size(px(120.), px(30.)));
        let span = |bounds: Bounds<Pixels>| (bounds.left(), bounds.right());
        assert_eq!(
            span(shown_part(tab(220.), px(0.), strip)),
            (px(220.), px(340.))
        );
        assert_eq!(
            span(shown_part(tab(220.), px(-180.), strip)),
            (px(100.), px(160.))
        );
        assert_eq!(
            span(shown_part(tab(100.), px(-180.), strip)),
            (px(100.), px(100.))
        );
        assert_eq!(
            span(shown_part(tab(460.), px(0.), strip)),
            (px(400.), px(400.))
        );
    }
}
