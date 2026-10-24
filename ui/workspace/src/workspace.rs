use crate::carry::{Carrying, Reported, Taken};
use crate::documents_menu::{self, DocumentsMenu};
use crate::drop::{PaneGeometry, Preview};
use crate::gestures::{CHROME_GESTURES, ChromeIntent, chrome_gestures};
#[cfg(target_family = "wasm")]
use crate::opening::{FileState, OpeningFile};
use crate::opening::{Opening, Shown, Unopened};
use crate::pane::Handover;
use crate::panel::keys_focus;
use crate::panel_menu::{Deed, Standing};
#[cfg(not(target_family = "wasm"))]
use crate::path_prompt::{PathPrompt, PathPromptEvent};
use crate::regions::{Placement, placement};
use crate::{
    AnyPanel, AppActions, Arrangement, DividerId, Document, DocumentId, Documents, Framing,
    Joinable, Layout, Maximize, Member, Pane, PaneEvent, PathOpener, PendingOpen, Regions, Removal,
    Role, Shared, Side, Split, SplitSide, WindowsChanged, documents, drag_icon, drop, panel_menu,
};
use bviz_input::Named;
use bviz_ui_components::dialog;
use bviz_ui_components::gesture::Pointer;
use bviz_ui_components::{
    Button, Divider, DragEnd, GestureGroup, Host, KeyScope, KeyStyle, KeysHelp, Menu, MenuEvent,
    NOTE_TEXT_SIZE, action_label, declare_scopes, declared_scopes, keys_in_effect, note_frame,
    note_strip, overlay_canvas, spinner, text_input, track_drag,
};
use bviz_ui_playback::{
    Clock, KEY_CONTEXT as PLAYBACK_KEY_CONTEXT, PlayPause, PlaybackBar, SeekEnd, SeekStart,
    SpeedDown, SpeedUp, StepBack, StepForward,
};
use bviz_ui_shell::Chrome;
use bviz_ui_theme::ThemeColors;
use gpui::{
    AbsoluteLength, Action, AnyElement, AnyView, AnyWindowHandle, App, Axis, Bounds, Canvas, Div,
    ElementId, Entity, EntityId, EventEmitter, ExternalPaths, FocusHandle, Focusable, IconScale,
    IntoElement, KeyBinding, KeyContext, MouseDownEvent, Pixels, PlatformDragEvent, Point, Rems,
    Render, SharedString, Size, Stateful, Subscription, WeakEntity, Window, actions, deferred, div,
    point, prelude::*, px, relative,
};
use std::collections::{HashMap, VecDeque};
use std::iter;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

actions!(
    bviz_workspace,
    [
        /// Show next tab
        NextTab,
        /// Show previous tab
        PreviousTab,
        /// Close panel
        ClosePanel,
        /// Close document
        CloseDocument,
        /// Move panel to new window
        DetachPanel,
        /// Maximise or restore pane
        ToggleMaximize,
        /// Open or close Documents menu
        ToggleDocuments,
        /// Focus left pane
        FocusLeft,
        /// Focus right pane
        FocusRight,
        /// Focus pane above
        FocusUp,
        /// Focus pane below
        FocusDown,
        /// Move panel left
        MovePanelLeft,
        /// Move panel right
        MovePanelRight,
        /// Move panel up
        MovePanelUp,
        /// Move panel down
        MovePanelDown,
        /// Split panel left
        SplitLeft,
        /// Split panel right
        SplitRight,
        /// Split panel up
        SplitUp,
        /// Split panel down
        SplitDown,
        /// Even out split
        EvenSplit,
        /// Reset to opening arrangement
        ResetArrangement,
        /// Show keys and gestures
        ShowKeysAndGestures,
        /// Open files
        OpenFiles,
    ]
);

/// The key context the workspace's own bindings are scoped to, so they reach a
/// window this crate frames and no other.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("Workspace", "Panes & windows");

/// The hairline drawn between two sides of a split.
const DIVIDER_LINE: Pixels = px(1.);

/// The band the divider is grabbed by, wide enough to aim a press at where the
/// line alone would be a pixel wide. Every boundary between a split's sides
/// takes this much of the split's extent out of what those sides divide.
pub(crate) const DIVIDER_GRIP: Pixels = px(7.);

/// How far a press on a panel's tab has to be carried along either axis before
/// it is a drag of that panel rather than a click on it, so that a click from an
/// unsteady hand is still a click. Measured axis by axis and set past the
/// distance the platform allows between the two presses of a double click, so a
/// press the platform still counts towards one cannot also have been a drag.
const DRAG_THRESHOLD: Pixels = px(6.);

/// How far from the cursor the ghost of the dragged panel is drawn, clear of
/// the cursor itself.
const GHOST_OFFSET: Pixels = px(14.);

/// How tall the ghost is drawn: a line for the panel's name, as its tab gives
/// it one. Fixed, so that the room for it beside the cursor is known before it
/// is laid out.
const GHOST_HEIGHT: Pixels = px(26.);

/// How wide the ghost may grow before the name it carries is cut, so that one
/// long title cannot cover what the drag is aimed at.
const GHOST_MAX_WIDTH: Pixels = px(220.);

/// How much room the ghost gives the name it carries, either side of it.
const GHOST_PADDING: Rems = Rems(0.5);

/// How far the ghost's corners are rounded off.
const GHOST_RADIUS: Rems = Rems(0.375);

/// The type the dragged panel's name is set in.
const GHOST_TEXT: Rems = Rems(0.875);

/// How large the icon naming the dragged panel's kind is drawn in the ghost,
/// beside its name.
const GHOST_ICON_SIZE: Pixels = px(14.);

/// The edge around what a drag puts up — the ghost, and the mark over where it
/// would land — wide enough to read as a border in screen pixels rather than as
/// a fringe of what it encloses.
const DRAG_EDGE: Pixels = px(2.);

/// How much of the accent colour fills a marked destination: enough to read
/// against either theme, light enough to read what it covers through.
const MARK_FILL: f32 = 0.3;

/// The layer a panel's menu is drawn in, above the overlay a drag of a panel
/// puts over the window, so that the menu stands over everything the window
/// holds.
const MENU_LAYER: usize = 1;

/// What a file chooser's control choosing the files says it does.
#[cfg(not(target_family = "wasm"))]
const OPEN: &str = "Open";

/// How an empty window says that files dropped on it open.
const DROP_HINT: &str = "or drop them on this window";

/// How long a note told briefly stands: long enough to be read at a glance
/// away from whatever the user was doing.
const BRIEF_NOTE: Duration = Duration::from_secs(4);

/// Adds `note` to `notes` in place of one reading the same, so a note told
/// again stands once, from when it was last told.
fn add_note(notes: &mut Vec<Note>, note: Note) {
    if let Says::Text(text) = &note.says {
        notes.retain(|standing| !matches!(&standing.says, Says::Text(told) if told == text));
    }
    notes.push(note);
}

/// How wide an empty window shows the openings of files named to it.
const OPENINGS_WIDTH: Rems = Rems(32.);

/// The text size of the openings an empty window shows in its body.
const OPENINGS_TEXT_SIZE: Rems = Rems(0.875);

/// The most of a line of an opening its file's label takes, leaving the rest
/// to the file's state.
const LABEL_SHARE: f32 = 0.5;

/// What the control dismissing a note says it does.
const DISMISS_NOTE: &str = "Dismiss";

/// Registers the window's key scopes and bindings, binding [`OpenFiles`], and
/// [`CloseDocument`] with it, only where `opens_files` says the app opens files
/// at all: where none open, a closed document is gone for good.
pub(crate) fn register_keys(cx: &mut App, opens_files: bool) {
    declare_scopes(
        cx,
        [
            KEY_CONTEXT,
            PLAYBACK_KEY_CONTEXT,
            documents_menu::KEY_CONTEXT,
        ],
    );
    cx.bind_keys(bindings(opens_files, Host::CURRENT));
}

fn bindings(opens_files: bool, host: Host) -> Vec<KeyBinding> {
    let workspace = Some(KEY_CONTEXT.id());
    let unless_typing = text_input::unless_typing(KEY_CONTEXT);
    let unless_typing = Some(unless_typing.as_str());
    let playback = text_input::unless_typing(PLAYBACK_KEY_CONTEXT);
    let playback = Some(playback.as_str());
    let mut bindings = vec![
        KeyBinding::new("?", ShowKeysAndGestures, unless_typing),
        KeyBinding::new("f1", ShowKeysAndGestures, workspace),
        KeyBinding::new("ctrl-tab", NextTab, workspace),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, workspace),
    ];
    match host {
        Host::Desktop => bindings.push(KeyBinding::new("secondary-w", ClosePanel, workspace)),
        Host::Browser => bindings.extend([
            KeyBinding::new("secondary-k tab", NextTab, workspace),
            KeyBinding::new("secondary-k shift-tab", PreviousTab, workspace),
            KeyBinding::new("secondary-k w", ClosePanel, workspace),
        ]),
    }
    if host.opens_windows() {
        bindings.push(KeyBinding::new("secondary-shift-n", DetachPanel, workspace));
    }
    bindings.extend([
        KeyBinding::new("secondary-shift-d", ToggleDocuments, workspace),
        KeyBinding::new(
            "escape",
            documents_menu::CloseDocuments,
            Some(documents_menu::KEY_CONTEXT.id()),
        ),
        KeyBinding::new("secondary-k m", ToggleMaximize, workspace),
        KeyBinding::new("secondary-k =", EvenSplit, workspace),
        KeyBinding::new("secondary-k 0", ResetArrangement, workspace),
        KeyBinding::new("secondary-k left", FocusLeft, workspace),
        KeyBinding::new("secondary-k right", FocusRight, workspace),
        KeyBinding::new("secondary-k up", FocusUp, workspace),
        KeyBinding::new("secondary-k down", FocusDown, workspace),
        KeyBinding::new("secondary-k shift-left", MovePanelLeft, workspace),
        KeyBinding::new("secondary-k shift-right", MovePanelRight, workspace),
        KeyBinding::new("secondary-k shift-up", MovePanelUp, workspace),
        KeyBinding::new("secondary-k shift-down", MovePanelDown, workspace),
        KeyBinding::new("secondary-k alt-left", SplitLeft, workspace),
        KeyBinding::new("secondary-k alt-right", SplitRight, workspace),
        KeyBinding::new("secondary-k alt-up", SplitUp, workspace),
        KeyBinding::new("secondary-k alt-down", SplitDown, workspace),
        KeyBinding::new("space", PlayPause, playback),
        KeyBinding::new("left", StepBack, playback),
        KeyBinding::new("right", StepForward, playback),
        KeyBinding::new("home", SeekStart, playback),
        KeyBinding::new("end", SeekEnd, playback),
        KeyBinding::new("]", SpeedUp, playback),
        KeyBinding::new("[", SpeedDown, playback),
    ]);
    if opens_files {
        bindings.push(KeyBinding::new("secondary-o", OpenFiles, workspace));
        bindings.push(KeyBinding::new("secondary-alt-w", CloseDocument, workspace));
    }
    bindings
}

/// Where a panel is re-homed by [`Workspace::move_panel`]. A pane is named by
/// its own handle, which holds wherever the arrangement lays that pane out: the
/// move takes the panel out first, and the splits around the pane it leaves can
/// collapse, and the destination still names the pane it named.
///
/// The pane is a type parameter, as an arrangement's is, so that where a drag
/// lands can be worked out on plain values away from the window real panes need.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Destination<P = Entity<Pane>> {
    /// Among that pane's tabs, at `index` of them or at the end where it holds
    /// fewer.
    Tab { pane: P, index: usize },
    /// A pane of its own on that side of that pane.
    Beside { pane: P, side: Side },
    /// A pane of its own spanning that side of the window.
    WindowEdge(Side),
    /// A window of its own, framed by a workspace of the same application at
    /// this window's size.
    NewWindow,
}

/// A [`Destination`] within the window: any but a window of its own.
enum Landing {
    Tab { pane: Entity<Pane>, index: usize },
    Beside { pane: Entity<Pane>, side: Side },
    WindowEdge(Side),
}

impl Destination {
    /// Where in the window `self` lays a panel out, `None` for a window of its
    /// own.
    fn in_window(self) -> Option<Landing> {
        match self {
            Destination::Tab { pane, index } => Some(Landing::Tab { pane, index }),
            Destination::Beside { pane, side } => Some(Landing::Beside { pane, side }),
            Destination::WindowEdge(side) => Some(Landing::WindowEdge(side)),
            Destination::NewWindow => None,
        }
    }
}

/// The menu of rearrangements a panel offers, while it stands over the window.
struct PanelMenu {
    menu: Entity<Menu>,
    /// Lets the menu go once it has closed itself.
    _dismissed: Subscription,
}

/// A press on the tab naming a panel, and how far it has been carried since.
struct PanelDrag {
    /// The panel pressed. The pane holding it is looked up afresh as the
    /// gesture goes on, so a gesture whose panel is closed from elsewhere — or
    /// whose pane the arrangement collapses under it — ends rather than moving
    /// whatever is left where it stood.
    panel: EntityId,
    carried: Carried,
    /// Where the last frame drawn marked the panel landing, which is where the
    /// release ending the drag puts it, so that what the drag shows is what it
    /// does. `None` where that frame marked nothing, and until a frame has
    /// marked anything at all.
    to: Option<Destination>,
    /// Whether the window system has refused to take this gesture over, which
    /// it answers the same way for the rest of it — so it is asked once, and
    /// the drag goes on being drawn and ended here.
    refused: bool,
    /// Cancels the gesture on `escape` for as long as it runs. Panels bind
    /// `escape` in their own key contexts, which lie deeper in the focus path
    /// than this workspace's and take the keystroke first, so the cancel
    /// intercepts the keystroke ahead of any binding at all.
    _cancel: Subscription,
}

/// How far a press on a panel's tab has been carried.
#[derive(Clone, Copy)]
enum Carried {
    /// Not far enough to be a drag of the panel: where the press landed, which
    /// a move has to pass [`DRAG_THRESHOLD`] from. The press is still the click
    /// the tab under it makes of one.
    Pressed(Point<Pixels>),
    /// Far enough: where the cursor is.
    Dragging(Point<Pixels>),
}

impl Carried {
    /// Where the press is holding the panel it carries, which is nowhere at all
    /// until it is a drag of that panel.
    fn dragging(self) -> Option<Point<Pixels>> {
        match self {
            Self::Pressed(_) => None,
            Self::Dragging(at) => Some(at),
        }
    }
}

/// Which pane lies on one side of another, as far as the last frame says.
#[derive(Debug, PartialEq)]
pub(crate) enum Towards<P = Entity<Pane>> {
    /// The pane laid out nearest that way.
    Found(P),
    /// Nothing but the window's own edge is laid out beyond that edge of the
    /// pane the direction starts from.
    Edge,
    /// Where the panes stand is not known, a pane the arrangement lays out
    /// having never been laid out by a frame.
    Unknown,
}

/// What a workspace keeps for each pane it frames.
struct FramedPane {
    _events: Subscription,
    /// Where the last frame that drew the arrangement laid this pane out, which
    /// is `None` until one has. Read between frames, to tell which pane a
    /// direction points at.
    bounds: Option<Bounds<Pixels>>,
}

/// The panes a window frames and the gestures aimed across them. One pane is the
/// active one, which the panel actions act on and whose panel titles the window.
struct Frame {
    arrangement: Arrangement,
    active: Entity<Pane>,
    /// What is kept for every pane of `arrangement`, retired as its pane leaves.
    panes: HashMap<EntityId, FramedPane>,
    /// The divider being dragged, while one is.
    divider_drag: Option<DividerId>,
    /// The press on a panel's tab being carried, while one is.
    panel_drag: Option<PanelDrag>,
    /// The menu a panel was asked for, while it stands.
    panel_menu: Option<PanelMenu>,
}

impl Frame {
    /// `layout` with `active` as its active pane and no gesture under way.
    fn new(layout: Layout, active: Entity<Pane>) -> Self {
        Self {
            arrangement: Arrangement::new(layout),
            active,
            panes: HashMap::new(),
            divider_drag: None,
            panel_drag: None,
            panel_menu: None,
        }
    }

    /// The panel the active pane shows.
    fn active_panel(&self, cx: &App) -> AnyPanel {
        self.active.read(cx).active_panel().clone()
    }

    /// The pane holding `panel` and the tab it holds it at.
    fn holder(&self, panel: EntityId, cx: &App) -> Option<(Entity<Pane>, usize)> {
        self.arrangement
            .layout()
            .panes()
            .find_map(|pane| pane.read(cx).index_of(panel).map(|at| (pane.clone(), at)))
    }

    fn pane_bounds(&self, pane: &Entity<Pane>) -> Option<Bounds<Pixels>> {
        self.panes.get(&pane.entity_id())?.bounds
    }

    /// Puts every pane back where the arrangement lays it out.
    fn restore(&mut self, cx: &mut Context<Workspace>) {
        let revealed = self.arrangement.restore();
        mark_revealed(revealed, cx);
    }

    fn rearrange(&mut self, cx: &mut Context<Workspace>) -> &mut Layout {
        self.restore(cx);
        self.arrangement.rearrange()
    }

    /// Tells the panel every pane shows that it is on show no longer, for a
    /// caller about to frame the panels afresh: the panes built in their place
    /// tell the panels they go on to show, and one left behind another's tab
    /// would otherwise go on drawing what it draws over the cursor.
    fn hide_shown_panels(&self, window: &mut Window, cx: &mut App) {
        for pane in self.arrangement.layout().panes() {
            let showing = pane.read(cx).active_panel().clone();
            showing.set_active(false, window, cx);
        }
    }

    /// Takes up the events of `pane`, a pane of the arrangement.
    fn watch(&mut self, pane: &Entity<Pane>, window: &Window, cx: &mut Context<Workspace>) {
        let events = cx.subscribe_in(
            pane,
            window,
            |workspace, pane, event, window, cx| match event {
                PaneEvent::Changed => cx.notify(),
                PaneEvent::Closed => workspace.close_pane(pane, window, cx),
                PaneEvent::Maximize => workspace.toggle_maximize(pane, cx),
                PaneEvent::Pressed { panel, at } => workspace.press_panel(*panel, *at, cx),
                PaneEvent::Released { at } => workspace.drop_panel(*at, window, cx),
                PaneEvent::Menu { panel, at } => {
                    workspace.open_panel_menu(*panel, *at, window, cx);
                }
                PaneEvent::KeysHelp(panel) => workspace.toggle_keys_help(*panel, window, cx),
                PaneEvent::Reveal(panel) => {
                    // The gesture asking for it may have come from another
                    // window.
                    workspace.reveal(*panel, window, cx);
                    window.activate_window();
                }
            },
        );
        self.panes.insert(
            pane.entity_id(),
            FramedPane {
                _events: events,
                bounds: None,
            },
        );
    }

    /// A pane of the arrangement showing `panel` alone.
    fn pane_for(
        &mut self,
        panel: AnyPanel,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Pane> {
        let pane = cx.new(|cx| Pane::new(panel, window, cx));
        self.watch(&pane, window, cx);
        pane
    }

    /// Lays `panel` out where its [`placement`] puts it, and gives the pane it
    /// lands in.
    fn place(
        &mut self,
        panel: AnyPanel,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Pane> {
        let panes = self.arrangement.layout().panes().map(|pane| {
            let roles = pane.read(cx).panels().map(AnyPanel::role);
            (pane.clone(), roles)
        });
        match placement(panel.role(), panes) {
            Placement::Tab(pane) => {
                pane.update(cx, |pane, cx| pane.add(panel, window, cx));
                pane
            }
            Placement::Apart(apart) => {
                let landed = self.pane_for(panel, window, cx);
                apart.lay_out(self.rearrange(cx), landed.clone());
                landed
            }
        }
    }

    /// Where the last frame laid each pane of the arrangement out. A pane no
    /// frame has laid out is left out: nothing is known of where a drop over it
    /// would land, and only the panes the arrangement holds are asked at all,
    /// so no frame that has since let a pane go is read for one.
    fn pane_geometry(&self, cx: &App) -> Vec<PaneGeometry<Entity<Pane>>> {
        let layout = self.arrangement.layout();
        let mut geometry = Vec::with_capacity(layout.panes().count());
        geometry.extend(layout.panes().filter_map(|pane| {
            let held = pane.read(cx);
            Some(PaneGeometry {
                pane: pane.clone(),
                bounds: self.pane_bounds(pane)?,
                tab_count: held.tab_count(),
                band: held.band(),
            })
        }));
        geometry
    }

    /// The pane `side` of `from` a move or a focus in that direction picks.
    fn pane_in_direction(&self, from: &Entity<Pane>, side: Side) -> Towards {
        in_direction(self.arrangement.layout(), from, side, |pane| {
            self.pane_bounds(pane)
        })
    }

    /// Every panel the frame holds: the one its first pane shows, then the
    /// others pane by pane in the order the arrangement lays them out, each
    /// pane's in tab order.
    fn held_panels(&self, cx: &App) -> (AnyPanel, Vec<AnyPanel>) {
        let layout = self.arrangement.layout();
        let leading = layout.first_pane().read(cx).active_panel().clone();
        let held: usize = layout.panes().map(|pane| pane.read(cx).tab_count()).sum();
        let mut others = Vec::with_capacity(held - 1);
        for pane in layout.panes() {
            others.extend(
                pane.read(cx)
                    .panels()
                    .filter(|panel| panel.entity_id() != leading.entity_id())
                    .cloned(),
            );
        }
        (leading, others)
    }
}

/// Marks the panel of each pane a restore brought back on screen for the
/// repaint it is owed, and draws the window afresh to bring it back.
fn mark_revealed(revealed: Vec<Entity<Pane>>, cx: &mut Context<Workspace>) {
    if revealed.is_empty() {
        return;
    }
    for pane in revealed {
        pane.update(cx, |pane, cx| pane.mark_shown(cx));
    }
    cx.notify();
}

/// Something a window tells its user until dismissed.
struct Note {
    /// Names the note apart from every other the window has been told, so its
    /// dismissal reaches it whichever notes go before it.
    id: u64,
    says: Says,
}

enum Says {
    Text(SharedString),
    /// Files on their way to their views, which no user dismisses: the note
    /// stands until the opening settles.
    Opening(Opening),
    Unopened(Unopened),
}

impl Says {
    /// The heading and the files' lines of an opening, `None` for a note of
    /// text alone.
    fn listing(&self) -> Option<Listing<'_>> {
        match self {
            Self::Text(_) => None,
            Self::Opening(opening) => Some((opening.heading(), opening.lines())),
            Self::Unopened(unopened) => Some((unopened.heading(), unopened.lines())),
        }
    }
}

type Listing<'a> = (String, Vec<(&'a SharedString, Shown<'a>)>);

/// A dialog standing over the window, and what it answers into.
struct Asking {
    dialog: AnyView,
    /// Where the dialog's keys are read.
    answer: FocusHandle,
    _answered: Subscription,
}

/// What a window shows below its title bar.
enum Body {
    /// No panel at all: the window stands open for documents to be opened
    /// into it.
    Empty,
    Framed(Box<Frame>),
}

/// A window's root view: the application chrome, and the [`Body`] it holds.
pub struct Workspace {
    /// The application, named ahead of the active panel in the window title
    /// so a task list tells a bviz window from any other.
    app_name: SharedString,
    /// The title last handed to the window system, which a frame hands it
    /// again only once it reads otherwise.
    titled: String,
    /// Focus for the chrome, so the shell's window actions (quit, close, tab)
    /// dispatch here; the ancestor of every panel's own focus.
    window_focus: FocusHandle,
    /// The window this workspace is the root of, through which a panel of
    /// another window is taken out of the panes here.
    window: AnyWindowHandle,
    body: Body,
    /// The status bar's list of the session's documents, through which a view
    /// closed here or shown in another window is brought into this one.
    documents_menu: Entity<DocumentsMenu>,
    /// Where the last frame laid the arrangement out, which is the region of
    /// the window a drag reads its edges and its panes against. `None` until a
    /// frame has laid it out.
    body_bounds: Option<Bounds<Pixels>>,
    /// The drags the window system is carrying for this window: the one this
    /// window handed it, and the one it carries over this window.
    carrying: Carrying<EntityId, Destination>,
    /// The bar driving the clock of the active panel's document, while that
    /// document plays back.
    playback: Option<Entity<PlaybackBar>>,
    /// What the window has to tell its user, each shown until dismissed.
    notes: Vec<Note>,
    /// How many notes the window has been told, which numbers the next.
    notes_told: u64,
    /// The dialogs waiting on an answer, in the order they were asked; the
    /// first stands over the window, and the rest wait their turn behind it.
    dialogs: VecDeque<Asking>,
    /// Draws the window afresh as the session's windows open and close, which
    /// changes where a panel of this one can be carried to.
    _windows_changed: Subscription,
    /// Draws the window afresh as the registry changes, which renames a
    /// document whose name the window's title and playback bar may show.
    _documents_changed: Subscription,
}

impl Workspace {
    /// Builds a workspace framing `regions`.
    pub(crate) fn new(
        app_name: impl Into<SharedString>,
        regions: Regions<AnyPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut workspace = Self::empty(app_name, window, cx);
        workspace.reframe(regions, window, cx);
        workspace
    }

    /// Builds a workspace framing no panel.
    pub(crate) fn empty(
        app_name: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = cx.weak_entity();
        let documents_menu = cx.new(|cx| DocumentsMenu::new(this, window, cx));
        let documents = Documents::global(cx);
        let windows_changed = cx.subscribe(&documents, |_, _, _: &WindowsChanged, cx| cx.notify());
        let documents_changed = cx.observe(&documents, |_, _, cx| cx.notify());
        let workspace = Self {
            app_name: app_name.into(),
            titled: String::new(),
            window_focus: cx.focus_handle(),
            window: window.window_handle(),
            body: Body::Empty,
            documents_menu,
            body_bounds: None,
            carrying: Carrying::new(),
            playback: None,
            notes: Vec::new(),
            notes_told: 0,
            dialogs: VecDeque::new(),
            _windows_changed: windows_changed,
            _documents_changed: documents_changed,
        };
        window.accept_platform_drag(panel_mime());
        let carried = cx.weak_entity();
        window.on_platform_drag(move |event, window, cx| {
            carried
                .update(cx, |workspace, cx| {
                    workspace.platform_drag(event, window, cx);
                })
                .ok();
        });
        let framed = cx.entity();
        documents.update(cx, |documents, cx| {
            documents.watch_workspace(&framed, cx);
        });
        workspace.show_active(window, cx);
        workspace
    }

    /// Builds a workspace framing the views of `documents`, as
    /// [`Workspace::open_documents`] lays them out.
    pub(crate) fn with_documents(
        app_name: impl Into<SharedString>,
        documents: Vec<Document>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut workspace = Self::empty(app_name, window, cx);
        workspace.open_documents(documents, window, cx);
        workspace
    }

    /// Registers each of `documents` and shows its views here: a window framing
    /// no panel frames them by role as a window opening with them does, and
    /// one framing panes lays each panel out where its [`placement`] puts it.
    /// The pane showing the first document's first view becomes the active one.
    pub fn open_documents(
        &mut self,
        documents: Vec<Document>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Body::Framed(frame) = &mut self.body else {
            if let Some(regions) = Document::open(documents, cx) {
                self.reframe(regions, window, cx);
            }
            return;
        };
        let registry = Documents::global(cx);
        let mut leading = None;
        for document in documents {
            let (first, rest) = registry.update(cx, |registry, cx| registry.open(document, cx));
            leading.get_or_insert(first.entity_id());
            for panel in iter::once(first).chain(rest) {
                frame.place(panel, window, cx);
            }
        }
        if let Some(panel) = leading {
            self.reveal(panel, window, cx);
        }
    }

    /// The panes the window frames, `None` while it frames none.
    fn frame(&self) -> Option<&Frame> {
        match &self.body {
            Body::Framed(frame) => Some(frame),
            Body::Empty => None,
        }
    }

    fn frame_mut(&mut self) -> Option<&mut Frame> {
        match &mut self.body {
            Body::Framed(frame) => Some(frame),
            Body::Empty => None,
        }
    }

    /// Closes `document` in every window: each panel showing one of its views
    /// leaves the window holding it, and the registry then lets the document
    /// go, with the models its views were minted over.
    pub fn close_document(
        &mut self,
        document: DocumentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        documents::close(document, self, window, cx);
    }

    /// Takes every panel showing a view of `document` out of the pane holding
    /// it, letting it go.
    pub(crate) fn drop_panels_of(
        &mut self,
        document: DocumentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(frame) = self.frame() else {
            return;
        };
        let panels: Vec<EntityId> = frame
            .arrangement
            .layout()
            .panes()
            .flat_map(|pane| {
                let held = pane.read(cx).panels();
                held.filter(|panel| panel.document() == document)
                    .map(AnyPanel::entity_id)
            })
            .collect();
        for panel in panels {
            self.take_panel(panel, window, cx);
        }
    }

    /// Joins the files `open` makes to the open document `to` names, as
    /// [`Documents::join`] does, and lays their views out here as
    /// [`placement`] puts them. `Err` hands `open` back unrun where the
    /// document has closed.
    pub fn join_document<F>(
        &mut self,
        to: &Joinable,
        open: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), F>
    where
        F: FnOnce(&Shared, &mut Window, &mut App) -> (Member, Vec<Member>),
    {
        let joined = Documents::global(cx)
            .update(cx, |documents, cx| documents.join(to, open, window, cx))?;
        for panel in joined {
            self.add_panel(panel, window, cx);
        }
        Ok(())
    }

    /// Asks which files to open, through the platform's file chooser or, where
    /// the platform offers none, the window's own dialog, and hands them to the
    /// app's [`PathOpener`].
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !cx.has_global::<PathOpener>() {
            return;
        }
        let chosen = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(OPEN.into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let chosen = chosen.await;
            this.update_in(cx, |workspace, window, cx| match chosen {
                Ok(Ok(Some(paths))) => workspace.open_paths(paths, window, cx),
                Ok(Ok(None)) => {}
                Ok(Err(_)) | Err(_) => workspace.ask_for_paths(window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Asks which files to open through the browser's file chooser, and hands
    /// them, once read, to the app's [`PathOpener`].
    #[cfg(target_family = "wasm")]
    pub(crate) fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !cx.has_global::<PathOpener>() {
            return;
        }
        let chosen = bviz_ui_web::choose_files();
        cx.spawn_in(window, async move |this, cx| {
            let read = bviz_ui_web::read_files(chosen.await).await;
            this.update_in(cx, |workspace, window, cx| {
                workspace.open_read(read, window, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Opens the files dropped on the page into this window, as long as it
    /// stands.
    #[cfg(target_family = "wasm")]
    pub(crate) fn open_dropped_files(&self, window: &Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let async_cx = window.to_async(cx);
        bviz_ui_web::on_files_dropped(move |files| {
            let this = this.clone();
            async_cx
                .spawn(async move |cx| {
                    let read = bviz_ui_web::read_files(files).await;
                    this.update_in(cx, |workspace, window, cx| {
                        workspace.open_read(read, window, cx)
                    })
                    .ok();
                })
                .detach();
        });
    }

    /// Hands the files `read` holds to the app's [`PathOpener`], telling the
    /// user of each the browser handed over unread.
    #[cfg(target_family = "wasm")]
    fn open_read(
        &mut self,
        read: Vec<Result<Named, bviz_ui_web::Unread>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut files = Vec::with_capacity(read.len());
        let mut lines = Vec::with_capacity(read.len());
        for file in read {
            let (label, state) = match file {
                Ok(named) => {
                    let label = crate::named_label(&named);
                    files.push(named);
                    (label, FileState::Opening)
                }
                Err(unread) => (
                    crate::document_label(std::path::Path::new(&unread.name)),
                    FileState::Failed(unread.error.to_string().into()),
                ),
            };
            lines.push(OpeningFile { label, state });
        }
        let pending = self.show_opening(Opening::new(lines), cx);
        self.hand_to_opener(files, pending, window, cx);
    }

    /// Opens the window's own dialog asking for the paths of files to open.
    #[cfg(not(target_family = "wasm"))]
    fn ask_for_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.new(PathPrompt::new);
        self.ask(
            prompt,
            |workspace, event, window, cx| {
                if let PathPromptEvent::Chosen(paths) = event {
                    workspace.open_paths(paths.clone(), window, cx);
                }
            },
            window,
            cx,
        );
    }

    /// Stands `dialog` over the window, once every dialog asked before it is
    /// answered, and hands `answered` the first event it emits, which is its
    /// answer: the dialog leaves the window as it emits one.
    pub fn ask<V, E>(
        &mut self,
        dialog: Entity<V>,
        answered: impl Fn(&mut Self, &E, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        V: Render + Focusable + EventEmitter<E>,
        E: 'static,
    {
        let asked = dialog.entity_id();
        let answered = cx.subscribe_in(&dialog, window, move |workspace, _, event, window, cx| {
            workspace.dismiss(asked, window, cx);
            answered(workspace, event, window, cx);
        });
        let answer = dialog.focus_handle(cx);
        self.dialogs.push_back(Asking {
            dialog: dialog.into(),
            answer,
            _answered: answered,
        });
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    /// Takes the dialog `asked` names off the window.
    fn dismiss(&mut self, asked: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        self.dialogs
            .retain(|asking| asking.dialog.entity_id() != asked);
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    /// Hands the files at `paths` to the app's [`PathOpener`] to open into
    /// this window.
    fn open_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let files = paths.into_iter().map(Named::Path).collect();
        self.open_named(files, window, cx);
    }

    /// Hands `files` to the app's [`PathOpener`] to open into this window.
    fn open_named(&mut self, files: Vec<Named>, window: &mut Window, cx: &mut Context<Self>) {
        if files.is_empty() {
            return;
        }
        let pending = self.begin_opening(&files, cx);
        self.hand_to_opener(files, pending, window, cx);
    }

    /// Hands `files` to the app's [`PathOpener`] to open as `pending`, which
    /// settles at once where there are none to open.
    fn hand_to_opener(
        &mut self,
        files: Vec<Named>,
        pending: PendingOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match cx.try_global::<PathOpener>().cloned() {
            Some(opener) if !files.is_empty() => opener.open(files, pending, window, cx),
            _ => pending.settle(Vec::new()),
        }
    }

    /// Shows `named` as being opened until the [`PendingOpen`] returned is
    /// settled.
    pub fn begin_opening<'a>(
        &mut self,
        named: impl IntoIterator<Item = &'a Named>,
        cx: &mut Context<Self>,
    ) -> PendingOpen {
        self.show_opening(Opening::of(named), cx)
    }

    /// Shows `opening` until the [`PendingOpen`] returned is settled.
    fn show_opening(&mut self, opening: Opening, cx: &mut Context<Self>) -> PendingOpen {
        let note = self.add_note(Says::Opening(opening), cx);
        PendingOpen::new(cx.weak_entity(), note, cx)
    }

    /// Replaces the opening `note` shows with what `settle` leaves of it.
    pub(crate) fn settle_opening(
        &mut self,
        note: u64,
        settle: impl FnOnce(Opening) -> Option<Unopened>,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = (self.notes.iter()).position(|told| told.id == note) else {
            return;
        };
        let Note { id, says } = self.notes.remove(at);
        let says = match says {
            Says::Opening(opening) => settle(opening).map(Says::Unopened),
            says => Some(says),
        };
        if let Some(says) = says {
            self.notes.insert(at, Note { id, says });
        }
        cx.notify();
    }

    /// The control on an empty window running [`OpenFiles`], where the app
    /// opens files at all.
    fn open_files_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        if !cx.has_global::<PathOpener>() {
            return None;
        }
        let button = Button::new(
            "workspace-open-files",
            cx.listener(|workspace, _, window, cx| workspace.open_files(window, cx)),
        );
        let button = match action_label(&OpenFiles, cx) {
            Some(label) => button.label(label),
            None => button,
        };
        Some(button.keys_for(&OpenFiles, &self.window_focus))
    }

    /// What takes files dropped on the window, marking the window while files
    /// are carried over it; `None` where the app opens no files.
    fn file_drop_target(&self, colors: &ThemeColors, cx: &mut Context<Self>) -> Option<Div> {
        if !cx.has_global::<PathOpener>() {
            return None;
        }
        let (fill, edge) = (colors.selection, colors.accent);
        Some(
            div()
                .absolute()
                .inset_0()
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.bg(fill).border(DRAG_EDGE).border_color(edge)
                })
                .on_drop(
                    cx.listener(|workspace, dropped: &ExternalPaths, window, cx| {
                        workspace.open_paths(dropped.paths().to_vec(), window, cx);
                    }),
                ),
        )
    }

    /// Shows `note` above the window's panes until the user dismisses it.
    pub fn tell(&mut self, note: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.add_note(Says::Text(note.into()), cx);
    }

    /// Shows `note` above the window's panes for [`BRIEF_NOTE`], or until the
    /// user dismisses it sooner.
    pub fn tell_briefly(&mut self, note: impl Into<SharedString>, cx: &mut Context<Self>) {
        let id = self.add_note(Says::Text(note.into()), cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BRIEF_NOTE).await;
            this.update(cx, |workspace, cx| workspace.dismiss_note(id, cx))
                .ok();
        })
        .detach();
    }

    /// Adds a note saying `says`; the id naming it.
    fn add_note(&mut self, says: Says, cx: &mut Context<Self>) -> u64 {
        let id = self.notes_told;
        self.notes_told += 1;
        add_note(&mut self.notes, Note { id, says });
        cx.notify();
        id
    }

    fn dismiss_note(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notes.retain(|note| note.id != id);
        cx.notify();
    }

    /// A strip for each note the window holds, with the control dismissing
    /// it, but for the openings an empty window shows in its body.
    fn note_strips(&self, colors: &ThemeColors, cx: &mut Context<Self>) -> Vec<Div> {
        let framed = self.frame().is_some();
        let mut strips = Vec::with_capacity(self.notes.len());
        for note in &self.notes {
            let strip = match &note.says {
                Says::Text(text) => note_strip(text.clone(), colors).items_center(),
                _ if !framed => continue,
                says => note_frame(colors).items_start().children(
                    says.listing()
                        .map(|listing| file_list(note.id, listing, NOTE_TEXT_SIZE.into(), colors)),
                ),
            };
            strips.push(
                strip
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_2()
                    .children(self.dismiss_control(note, cx)),
            );
        }
        strips
    }

    /// The openings an empty window shows in its body, each with the control
    /// dismissing it once it can be.
    fn opening_blocks(&self, colors: &ThemeColors, cx: &mut Context<Self>) -> Vec<Div> {
        let mut blocks = Vec::with_capacity(self.notes.len());
        for note in &self.notes {
            let Some(listing) = note.says.listing() else {
                continue;
            };
            blocks.push(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_start()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .text_size(OPENINGS_TEXT_SIZE)
                    .text_color(colors.text_muted)
                    .child(file_list(
                        note.id,
                        listing,
                        OPENINGS_TEXT_SIZE.into(),
                        colors,
                    ))
                    .children(self.dismiss_control(note, cx)),
            );
        }
        blocks
    }

    /// The control dismissing `note`, which an opening still under way has
    /// none of.
    fn dismiss_control(&self, note: &Note, cx: &mut Context<Self>) -> Option<Button> {
        if let Says::Opening(_) = note.says {
            return None;
        }
        let this = cx.weak_entity();
        let id = note.id;
        Some(
            Button::new(
                ElementId::NamedInteger("workspace-note-dismiss".into(), id),
                move |_, _, cx| {
                    this.update(cx, |workspace, cx| workspace.dismiss_note(id, cx))
                        .ok();
                },
            )
            .ghost()
            .icon("x")
            .compact()
            .tooltip(DISMISS_NOTE),
        )
    }

    pub fn active_pane(&self) -> Option<&Entity<Pane>> {
        self.frame().map(|frame| &frame.active)
    }

    pub(crate) fn window(&self) -> AnyWindowHandle {
        self.window
    }

    /// Whether a frame of this window shows `panel`, which takes the pane
    /// holding it both showing it among its tabs and being a pane the window
    /// draws at all. `None` where no pane here holds it.
    pub(crate) fn shows(&self, panel: EntityId, cx: &App) -> Option<bool> {
        let frame = self.frame()?;
        shows_panel(&frame.arrangement, frame.holder(panel, cx), |pane| {
            pane.read(cx).showing()
        })
    }

    fn toggle_maximize(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_mut() else {
            return;
        };
        match frame.arrangement.toggle_maximize(pane) {
            Maximize::Unchanged => {}
            Maximize::Maximized => cx.notify(),
            Maximize::Restored(revealed) => mark_revealed(revealed, cx),
        }
    }

    /// Puts every pane back where the arrangement lays it out.
    fn restore(&mut self, cx: &mut Context<Self>) {
        if let Some(frame) = self.frame_mut() {
            frame.restore(cx);
        }
    }

    /// Lays the window out as it opened.
    ///
    /// The panels themselves are laid out again — each keeps what it has
    /// scrolled to, zoomed in on and selected, and goes on being the panel the
    /// registry records for its view.
    pub(crate) fn reset_arrangement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame() else {
            return;
        };
        let (leading, others) = frame.held_panels(cx);
        let mut unplaced: HashMap<EntityId, AnyPanel> = iter::once(&leading)
            .chain(&others)
            .map(|panel| (panel.entity_id(), panel.clone()))
            .collect();
        let placed = Documents::global(cx)
            .read(cx)
            .opening_regions(&mut |panel| unplaced.remove(&panel));
        let regions = put_back(placed, leading, others, AnyPanel::role, |panel| {
            unplaced.contains_key(&panel.entity_id())
        });
        self.reframe(regions, window, cx);
    }

    /// Frames `regions` in place of the arrangement the window lays out, making
    /// the pane holding their leading panel the active one.
    ///
    /// The panels move from the panes that held them to the panes built for
    /// them, and are held by both while the new panes are being built, so what
    /// the old panes answer for is let go of first: the panels they tell they
    /// are on show, and the subscriptions carrying their events here. The
    /// gestures aimed across them go with the frame they were aimed across.
    fn reframe(&mut self, regions: Regions<AnyPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(frame) = self.frame_mut() {
            frame.hide_shown_panels(window, cx);
            frame.panes.clear();
        }
        let (layout, leading, panes) = framed_panes(regions, window, cx);
        let mut frame = Frame::new(layout, leading.clone());
        for pane in panes {
            frame.watch(&pane, window, cx);
        }
        self.replace_body(Body::Framed(Box::new(frame)));
        self.activate_pane(leading, window, cx);
    }

    /// Puts `body` in place of what the window shows. A drag the window
    /// system carries is the window system's to end, and letting go of it here
    /// would leave the gesture out there with nothing to report to. Where it
    /// is aimed is let go of instead: a destination names a pane of the body
    /// replaced.
    fn replace_body(&mut self, body: Body) {
        self.carrying.aim(None);
        self.body = body;
    }

    /// Brings `panel` forward in the pane holding it and makes that the active
    /// pane, reporting whether this workspace holds it.
    pub(crate) fn reveal(
        &mut self,
        panel: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((pane, index)) = self.holder(panel, cx) else {
            return false;
        };
        pane.update(cx, |pane, cx| pane.activate(index, window, cx));
        self.activate_pane(pane, window, cx);
        true
    }

    /// Takes `panel` out of the pane holding it, for a caller that re-homes it
    /// in another window. `None` where no pane here holds it.
    pub(crate) fn take_panel(
        &mut self,
        panel: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyPanel> {
        let frame = self.frame_mut()?;
        let (pane, _) = frame.holder(panel, cx)?;
        // A menu names the panes its rows would move a panel to, which taking
        // a panel out may take away; and the bar may drive the clock of the
        // taken panel's document, which closing the document lets go of. The
        // next frame puts up the bar of the active panel's document afresh.
        frame.panel_menu = None;
        self.playback = None;
        self.vacate(&pane, panel, window, cx)
    }

    /// Takes `panel` out of the pane holding it and re-homes it at `to`, which
    /// becomes the active pane showing it. The panel itself is the one that
    /// moves — what it has scrolled to, zoomed in on and selected moves with it.
    /// A destination asking for where the panel already stands
    /// ([`Workspace::re_homes`]) leaves the arrangement and the panel as they
    /// are.
    pub(crate) fn move_panel(
        &mut self,
        panel: EntityId,
        to: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((from, at)) = self.holder(panel, cx) else {
            return;
        };
        if !self.re_homes(&to, Some(&from), cx) {
            return;
        }
        let Some(to) = to.in_window() else {
            self.detach(panel, &from, at, window, cx);
            return;
        };
        let Some(panel) = self.vacate(&from, panel, window, cx) else {
            return;
        };
        self.land(panel, to, Some((from, at)), window, cx);
    }

    /// Moves `panel` out of tab `at` of `from` into a window of its own, and
    /// leaves it there where the platform opens no window for it.
    fn detach(
        &mut self,
        panel: EntityId,
        from: &Entity<Pane>,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(handover) = from.update(cx, |pane, cx| pane.remove(panel, window, cx)) else {
            return;
        };
        let (Handover::Kept(taken) | Handover::Emptied(taken)) = &handover;
        let opened = self.open_window_for(taken.clone(), window, cx);
        match (handover, opened) {
            (Handover::Kept(_), Ok(())) => {}
            // The window opened here is one of the session's before this one
            // is left framing no pane.
            (Handover::Emptied(_), Ok(())) => self.close_pane(from, window, cx),
            (Handover::Kept(taken), Err(error)) => {
                no_window_opened(&error);
                from.update(cx, |pane, cx| pane.insert(at, taken, window, cx));
                self.activate_pane(from.clone(), window, cx);
            }
            (Handover::Emptied(_), Err(error)) => {
                no_window_opened(&error);
                from.update(cx, |pane, cx| pane.take_back(window, cx));
                self.activate_pane(from.clone(), window, cx);
            }
        }
    }

    /// Lays `panel` out at `to` and makes the pane it lands in the active one,
    /// or as [`Workspace::add_panel`] does where the window frames no pane.
    /// `taken` says which pane of this window it came out of and which tab of
    /// that pane it was, and is nothing where it comes from another window.
    fn land(
        &mut self,
        panel: AnyPanel,
        to: Landing,
        taken: Option<(Entity<Pane>, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(frame) = self.frame_mut() else {
            self.add_panel(panel, window, cx);
            return;
        };
        let landed = match to {
            Landing::Tab { pane, index } => {
                let taken = taken.and_then(|(from, at)| (from == pane).then_some(at));
                let index = landing_index(index, taken);
                pane.update(cx, |pane, cx| pane.insert(index, panel, window, cx));
                pane
            }
            Landing::Beside { pane, side } => {
                let landed = frame.pane_for(panel, window, cx);
                frame
                    .rearrange(cx)
                    .insert_beside(&pane, side, landed.clone());
                landed
            }
            Landing::WindowEdge(side) => {
                let landed = frame.pane_for(panel, window, cx);
                frame.rearrange(cx).insert_at_edge(side, landed.clone());
                landed
            }
        };
        self.activate_pane(landed, window, cx);
    }

    /// Opens a window of its own for `panel`, which another window has given
    /// up, and keeps it in this one where no window opens.
    fn adopt_into_new_window(
        &mut self,
        panel: AnyPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.open_window_for(panel.clone(), window, cx) {
            no_window_opened(&error);
            self.add_panel(panel, window, cx);
        }
    }

    /// [`re_homes`] for a pane this workspace frames, read for how many panels
    /// it holds.
    fn re_homes(&self, to: &Destination, from: Option<&Entity<Pane>>, cx: &App) -> bool {
        let Some(frame) = self.frame() else {
            return false;
        };
        let alone = from.is_some_and(|pane| pane.read(cx).tab_count() == 1);
        re_homes(to, from, alone, frame.arrangement.layout(), Host::CURRENT)
    }

    /// Takes `panel` out of `from` for a caller re-homing it, and takes `from`
    /// out of the arrangement where that panel was the only one it held.
    fn vacate(
        &mut self,
        from: &Entity<Pane>,
        panel: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyPanel> {
        match from.update(cx, |pane, cx| pane.remove(panel, window, cx))? {
            Handover::Kept(panel) => Some(panel),
            Handover::Emptied(panel) => {
                self.close_pane(from, window, cx);
                Some(panel)
            }
        }
    }

    fn open_window_for(
        &mut self,
        panel: AnyPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Result<()> {
        let app_name = self.app_name.clone();
        let size = window.viewport_size();
        bviz_ui_shell::open_window(&self.app_name, size, cx, move |window, cx| {
            let regions = Regions::new(panel.role(), panel);
            cx.new(|cx| Self::new(app_name, regions, window, cx))
        })
        .map(drop)
    }

    /// Puts up the menu of rearrangements `panel` offers, opening at `at` in
    /// the window. The menu is built from where the panel stands as it is asked
    /// for, so it offers what that moment's arrangement can carry out.
    fn open_panel_menu(
        &mut self,
        panel: EntityId,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let windows = self.other_windows(cx);
        let Some(frame) = self.frame_mut() else {
            return;
        };
        let Some((pane, _)) = frame.holder(panel, cx) else {
            return;
        };
        let active = frame.active_panel(cx);
        let keys_in = keys_focus(panel, Some(&active), cx);
        let items = panel_menu::items(
            panel,
            &cx.weak_entity(),
            &Standing {
                arrangement: &frame.arrangement,
                pane: &pane,
                alone: pane.read(cx).tab_count() == 1,
            },
            &windows,
            keys_in.as_ref(),
            window,
            cx,
        );
        let menu = cx.new(|cx| Menu::new(items, at, window, cx));
        let dismissed = cx.subscribe_in(
            &menu,
            window,
            |workspace, _, event: &MenuEvent, window, cx| {
                let MenuEvent::Dismissed { holding_focus } = event;
                if let Some(frame) = workspace.frame_mut() {
                    frame.panel_menu = None;
                }
                if *holding_focus {
                    workspace.focus_active_panel(window, cx);
                }
                cx.notify();
            },
        );
        frame.panel_menu = Some(PanelMenu {
            menu,
            _dismissed: dismissed,
        });
        cx.notify();
    }

    /// Closes the keys help the pane holding `panel` has up, or else puts the
    /// keys help of `panel` up over it, bringing it forward first where it is
    /// not the active panel.
    fn toggle_keys_help(&mut self, panel: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane, _)) = self.holder(panel, cx) else {
            return;
        };
        if pane.read(cx).shows_keys_help() {
            pane.update(cx, |pane, cx| pane.close_keys_help(window, cx));
            return;
        }
        if self
            .active_panel(cx)
            .is_none_or(|active| active.entity_id() != panel)
        {
            self.reveal(panel, window, cx);
        }
        let room = pane.read(cx).keys_help_room(window);
        let shown = pane.read(cx).active_panel().clone();
        let help = self.keys_help(&shown, room, window, cx);
        pane.update(cx, |pane, cx| pane.show_keys_help(help, window, cx));
    }

    /// The keys help of `panel`, which holds focus, standing at most
    /// `max_height` tall. Its keys are those in effect in the context stack the
    /// last frame drew from the window's root to `panel`, read along the path
    /// to the focused element; where that path does not pass through `panel`,
    /// [`stack_around`] stands in for it.
    fn keys_help(
        &self,
        panel: &AnyPanel,
        max_height: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<KeysHelp> {
        let scope = panel.key_scope();
        let mut stack = window.context_stack();
        match stack
            .iter()
            .rposition(|context| context.contains(scope.id()))
        {
            Some(at) => stack.truncate(at + 1),
            None => stack = stack_around(self.key_context(cx), scope),
        }
        let keys = {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            keys_in_effect(
                &keymap,
                declared_scopes(cx),
                &stack,
                scope,
                |action| action_label(action, cx).unwrap_or(action.name()).into(),
                KeyStyle::NATIVE,
            )
        };
        let gestures = vec![
            GestureGroup {
                scope: scope.label(),
                rows: panel.gestures(),
            },
            GestureGroup {
                scope: KEY_CONTEXT.label(),
                rows: chrome_gestures(),
            },
        ];
        cx.new(|cx| KeysHelp::new(keys, gestures, max_height, window, cx))
    }

    /// The key context the window's root carries: the workspace's own, and the
    /// playback scope while the active panel's document plays back.
    fn key_context(&self, cx: &mut App) -> KeyContext {
        let mut key_context = KeyContext::from(KEY_CONTEXT);
        if let Some(panel) = self.active_panel(cx)
            && Documents::global(cx)
                .read(cx)
                .playback(panel.document())
                .is_some()
        {
            key_context.add(PLAYBACK_KEY_CONTEXT.id());
        }
        key_context
    }

    /// Hands focus to where it rests in this window, the
    /// [`content_focus`](Self::content_focus). Which panel that is is read
    /// here and not before: a menu names rows that close a panel and that
    /// move one to another window, so the panel a gesture was opened over may
    /// be gone by the time focus is to be given back, and a focus handle naming
    /// nothing this window draws leaves every one of the window's own key
    /// bindings unreachable until something is clicked.
    fn focus_active_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.content_focus(cx), cx);
    }

    /// Where focus rests in this window: on the dialog standing over it while
    /// one awaits an answer, so that nothing changing underneath takes its
    /// keys, else on the panel the active pane shows, or on the chrome where
    /// the window frames no panel.
    pub(crate) fn content_focus(&self, cx: &App) -> FocusHandle {
        if let Some(asking) = self.dialogs.front() {
            return asking.answer.clone();
        }
        match self.active_panel(cx) {
            Some(panel) => panel.focus_handle(cx),
            None => self.window_focus.clone(),
        }
    }

    /// Gives focus to what the window shows.
    fn show_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.content_focus(cx), cx);
        cx.notify();
    }

    /// The session's other open windows, each named as another window's menu
    /// lists it. This workspace is left out rather than read: a menu is opened
    /// from a press this workspace is answering, which has it leased.
    fn other_windows(&self, cx: &mut Context<Self>) -> Vec<(WeakEntity<Self>, SharedString)> {
        let here = cx.entity_id();
        let open = Documents::global(cx).read(cx).workspaces();
        let mut named = Vec::with_capacity(open.len());
        for (place, workspace) in open.iter().enumerate() {
            if workspace.entity_id() != here {
                let shown = workspace.read(cx).shown_title(cx);
                named.push((workspace.downgrade(), window_name(place + 1, &shown)));
            }
        }
        named
    }

    /// Carries `deed` out on `panel`, which is what choosing a row of that
    /// panel's menu comes to.
    pub(crate) fn take_deed(
        &mut self,
        panel: EntityId,
        deed: Deed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match deed {
            Deed::Move(to) => self.move_panel(panel, to, window, cx),
            Deed::ToWindow(into) => {
                if let Some(into) = into.upgrade() {
                    self.move_panel_to_window(panel, &into, window, cx);
                }
            }
            Deed::Maximize => {
                if let Some((pane, _)) = self.holder(panel, cx) {
                    self.toggle_maximize(&pane, cx);
                }
            }
            Deed::EvenSplit => {
                if let Some((pane, _)) = self.holder(panel, cx) {
                    self.even_pane_split(&pane, cx);
                }
            }
            Deed::KeysHelp => self.toggle_keys_help(panel, window, cx),
            Deed::Close => {
                if let Some((pane, at)) = self.holder(panel, cx) {
                    pane.update(cx, |pane, cx| pane.close(at, window, cx));
                }
            }
        }
    }

    /// Takes `panel` out of this window and shows it in the window `into`
    /// frames, where it takes focus. The panel itself moves — a panel is
    /// rendered in one window at a time — so what it has scrolled to, zoomed in
    /// on and selected moves with it.
    fn move_panel_to_window(
        &mut self,
        panel: EntityId,
        into: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The panel comes out of its pane only once the window it is going to
        // answers: a window that has closed since the menu listed it would
        // leave the panel held by nothing at all.
        let handle = into.read(cx).window;
        if handle.update(cx, |_, _, _| ()).is_err() {
            return;
        }
        let Some(taken) = self.take_panel(panel, window, cx) else {
            return;
        };
        handle
            .update(cx, |_, window, cx| {
                into.update(cx, |workspace, cx| workspace.add_panel(taken, window, cx));
            })
            .ok();
    }

    /// Takes up a press that landed on the tab naming `panel`, which becomes a
    /// drag of that panel once it is carried [`DRAG_THRESHOLD`] from `at`.
    fn press_panel(&mut self, panel: EntityId, at: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.carrying.takes_a_press() {
            return;
        }
        let workspace = cx.weak_entity();
        let here = self.window;
        let cancel = cx.intercept_keystrokes(move |event, window, cx| {
            // Keystrokes of every window of the application come through here,
            // and a gesture belongs to the one it runs in. A modified escape is
            // a keystroke of its own and is left to whatever binds it.
            if window.window_handle() != here
                || event.keystroke.key != "escape"
                || event.keystroke.modifiers.modified()
            {
                return;
            }
            workspace
                .update(cx, |workspace, cx| {
                    if workspace
                        .frame()
                        .is_some_and(|frame| frame.panel_drag.is_some())
                    {
                        cx.stop_propagation();
                        workspace.end_panel_drag(cx);
                    }
                })
                .ok();
        });
        let Some(frame) = self.frame_mut() else {
            return;
        };
        frame.panel_drag = Some(PanelDrag {
            panel,
            carried: Carried::Pressed(at),
            to: None,
            refused: false,
            _cancel: cancel,
        });
        cx.notify();
    }

    /// Lets the press go without moving anything, retiring what it put over the
    /// window with it.
    fn end_panel_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(frame) = self.frame_mut() {
            frame.panel_drag = None;
        }
        cx.notify();
    }

    /// The panel the drag this window handed to the window system is carrying,
    /// while it is out there. Read by whichever window the drag is carried
    /// over, which is what tells that window what a drop of it would move.
    pub(crate) fn handed_over(&self) -> Option<EntityId> {
        self.carrying.handed_panel()
    }

    /// Carries the press to `at`, which becomes a drag of the panel once it is
    /// far enough from where it landed, and is offered to the window system
    /// once it is carried far enough out of the window.
    fn drag_panel(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.frame().and_then(|frame| frame.panel_drag.as_ref()) else {
            return;
        };
        let (panel, carried, refused) = (drag.panel, drag.carried, drag.refused);
        let carry = carried_to(carried, refused, at, viewport(window));
        if carry == Carry::Holds {
            return;
        }
        // A drag reads where the frame laid the panes out to tell where a drop
        // would put the panel, so every pane is on screen before it is carried
        // anywhere.
        let Some(frame) = self.frame_mut() else {
            return;
        };
        frame.restore(cx);
        let held = frame.holder(panel, cx).is_some();
        let drag = frame.panel_drag.take();
        frame.panel_drag = drag.filter(|_| held).map(|drag| PanelDrag {
            carried: Carried::Dragging(at),
            ..drag
        });
        cx.notify();
        if carry == Carry::Over
            && !self.hand_over(window, cx)
            && let Some(drag) = self.frame_mut().and_then(|frame| frame.panel_drag.as_mut())
        {
            drag.refused = true;
        }
    }

    /// Offers the drag to the window system and reports whether it took it. A
    /// window system that takes it draws the panel where the pointer carries
    /// it and tells every window it reaches — this application's own among
    /// them — where it goes and where it is dropped, and the gesture stops
    /// being this window's: the pointer is the window system's, and a drag
    /// carried back over this window arrives as one carried over any other. A
    /// window system that refuses leaves the press being carried here, drawn
    /// under the cursor and ended by the release.
    ///
    /// Offered where a release would ask for a window of its own
    /// ([`drop::tears_out`]), so that a drag flicked past the edge and back in
    /// is carried here as ever.
    fn hand_over(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(frame) = self.frame() else {
            return false;
        };
        let Some(drag) = frame.panel_drag.as_ref() else {
            return false;
        };
        let carried = drag.panel;
        let Some((pane, index)) = frame.holder(carried, cx) else {
            return false;
        };
        let Some(panel) = pane.read(cx).panel_at(index).cloned() else {
            return false;
        };
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let chip = drag_icon::Chip {
            title: panel.display_title(),
            font_size: GHOST_TEXT.to_pixels(window.rem_size()),
            height: GHOST_HEIGHT,
            max_width: GHOST_MAX_WIDTH,
            padding: GHOST_PADDING.to_pixels(window.rem_size()),
            radius: GHOST_RADIUS.to_pixels(window.rem_size()),
            edge: DRAG_EDGE,
            fill: colors.surface,
            edge_color: colors.accent,
            ink: colors.text,
            scale: IconScale::for_display(window.scale_factor()),
        };
        let icon = drag_icon::rasterize(&chip, window);
        if !window.start_platform_drag(panel_mime(), icon) {
            return false;
        }
        if let Some(frame) = self.frame_mut() {
            frame.panel_drag = None;
        }
        self.carrying.hand_over(carried);
        cx.notify();
        true
    }

    /// Ends the press where the button was let go: the panel goes where the
    /// drag last marked it landing, so that what the drag showed is what it
    /// does, and a release past the window's own edge — past every mark in the
    /// window — asks for a window of its own instead. Both the release the tab
    /// under the cursor takes and the one the window-global handlers take come
    /// here, so that whichever arrives first ends the one gesture. A press that
    /// never became a drag is left to the tab under it to make a click of.
    fn drop_panel(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.frame_mut().and_then(|frame| frame.panel_drag.take()) else {
            return;
        };
        cx.notify();
        if drag.carried.dragging().is_none() {
            return;
        }
        // The release is spent on the move; it is not also a click on the tab
        // the drag started from, which would bring another panel forward.
        cx.stop_propagation();
        let to = match drop::tears_out(at, viewport(window)) {
            true => Some(Destination::NewWindow),
            false => drag.to,
        };
        if let Some(to) = to {
            self.move_panel(drag.panel, to, window, cx);
        }
    }

    /// Takes up what the window system reports of a drag it carries: one it
    /// carries over this window, which the window marks and takes a drop of,
    /// and the end of the one this window handed it.
    fn platform_drag(
        &mut self,
        event: PlatformDragEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let report = match event {
            PlatformDragEvent::Entered(at) => {
                // A drag reads where the frame laid the panes out, so every
                // pane is on screen before one is aimed at.
                self.restore(cx);
                Reported::Entered {
                    panel: self.carried_panel(cx),
                    at,
                }
            }
            PlatformDragEvent::Moved(at) => Reported::Moved(at),
            PlatformDragEvent::Left => Reported::Left,
            PlatformDragEvent::Dropped(at) => Reported::Dropped {
                aimed: self
                    .carrying
                    .unaimed()
                    .and_then(|panel| self.destination_at(at, panel, window, cx)),
            },
            PlatformDragEvent::Ended(end) => Reported::Ended(end),
        };
        match self.carrying.reports(report) {
            Taken::Nothing => {}
            Taken::Land { panel, to } => self.take_in(panel, to, window, cx),
            Taken::Ended { panel, detach } => {
                if detach {
                    self.move_panel(panel, Destination::NewWindow, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Puts `panel`, which the window system was carrying, at `to`: out of a
    /// pane of this window where the drag started here, so that a drag carried
    /// back into the window it left lands exactly as one that never left it,
    /// and out of whichever other window of the session holds it otherwise.
    fn take_in(
        &mut self,
        panel: EntityId,
        to: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.holder(panel, cx).is_some() {
            self.move_panel(panel, to, window, cx);
        } else {
            let here = cx.entity_id();
            let open = Documents::global(cx).read(cx).workspaces();
            let Some(taken) = documents::take_panel_from(open, panel, here, cx) else {
                return;
            };
            match to.in_window() {
                Some(to) => self.land(taken, to, None, window, cx),
                None => self.adopt_into_new_window(taken, window, cx),
            }
        }
        // The panel was carried to this window, so this window comes forward
        // with it rather than leaving it behind the one it came from.
        window.activate_window();
    }

    /// The panel the drag the window system is carrying over this window holds:
    /// the one a window of the session handed over. The window system carries
    /// one drag of this application at a time — a window offers another only
    /// once the first is reported over — and the type the drag offers names
    /// this process, so the one window with a drag out there is the window this
    /// drag came from. `None` where no window of the session has one out there,
    /// and a drop then moves nothing.
    fn carried_panel(&self, cx: &mut Context<Self>) -> Option<EntityId> {
        if let Some(panel) = self.handed_over() {
            return Some(panel);
        }
        // This workspace is left out of the session's rather than read: it is
        // the one the report reached, and so is leased.
        let here = cx.entity_id();
        let open = Documents::global(cx).read(cx).workspaces();
        open.iter()
            .filter(|workspace| workspace.entity_id() != here)
            .find_map(|workspace| workspace.read(cx).handed_over())
    }

    /// The pane laid out nearest the active one on `side` of it, made active.
    fn focus_towards(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame() else {
            return;
        };
        if let Towards::Found(pane) = frame.pane_in_direction(&frame.active, side) {
            pane.read(cx).reveal_shown_tab();
            self.activate_pane(pane, window, cx);
        }
    }

    /// Moves the panel on show into the pane on `side` of the one showing it, as
    /// its last tab — or against that edge of the window, where no pane lies
    /// that way.
    fn move_towards(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame() else {
            return;
        };
        let to = match frame.pane_in_direction(&frame.active, side) {
            Towards::Found(pane) => {
                let index = pane.read(cx).tab_count();
                Destination::Tab { pane, index }
            }
            Towards::Edge => Destination::WindowEdge(side),
            Towards::Unknown => return,
        };
        let panel = frame.active_panel(cx).entity_id();
        self.move_panel(panel, to, window, cx);
    }

    /// Takes the panel on show into a pane of its own on `side` of the pane
    /// showing it.
    fn split_towards(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame() else {
            return;
        };
        let panel = frame.active_panel(cx).entity_id();
        let pane = frame.active.clone();
        self.move_panel(panel, Destination::Beside { pane, side }, window, cx);
    }

    /// Records where the frame being laid out put `pane`.
    fn laid_out(&mut self, pane: EntityId, bounds: Bounds<Pixels>) {
        if let Some(framed) = self
            .frame_mut()
            .and_then(|frame| frame.panes.get_mut(&pane))
        {
            framed.bounds = Some(bounds);
        }
    }

    /// The pane holding `panel` and the tab it holds it at.
    fn holder(&self, panel: EntityId, cx: &App) -> Option<(Entity<Pane>, usize)> {
        self.frame()?.holder(panel, cx)
    }

    /// The panel the active pane shows, `None` while the window frames no
    /// pane.
    pub fn active_panel(&self, cx: &App) -> Option<AnyPanel> {
        Some(self.frame()?.active_panel(cx))
    }

    /// What names the window: the application, then, where a panel is on
    /// show, the name of its document where that is an invocation's, then the
    /// panel's title.
    fn window_title(&self, panel: Option<&AnyPanel>, cx: &mut App) -> String {
        let Some(panel) = panel else {
            return self.app_name.to_string();
        };
        let app_name = &self.app_name;
        let title = panel.display_title();
        match Documents::global(cx).read(cx).joined_name(panel.document()) {
            Some(joined) => format!("{app_name} \u{2014} {joined} \u{2014} {title}"),
            None => format!("{app_name} \u{2014} {title}"),
        }
    }

    /// What the window shows, as another window's menu names it: the panel on
    /// show, or the application's name where it shows none.
    fn shown_title(&self, cx: &App) -> SharedString {
        match self.active_panel(cx) {
            Some(panel) => panel.display_title(),
            None => self.app_name.clone(),
        }
    }

    /// Applies `gesture` to the clock the active panel's document plays back
    /// on; where that document has nothing to play, the keystroke goes on to
    /// the focused element.
    fn drive_clock(
        &self,
        cx: &mut Context<Self>,
        gesture: impl FnOnce(&mut Clock, &mut Context<Clock>),
    ) {
        let Some(panel) = self.active_panel(cx) else {
            cx.propagate();
            return;
        };
        let documents = Documents::global(cx);
        match documents.read(cx).playback(panel.document()) {
            Some((_, clock)) if clock.read(cx).timeline().is_some() => clock.update(cx, gesture),
            _ => cx.propagate(),
        }
    }

    /// The bar driving the clock `panel`'s document plays back on, labelled
    /// with the name the document goes by, kept while the active panel's
    /// document stays the same one, and `None` for a document that does not
    /// play back and where no panel is shown.
    fn playback_bar(
        &mut self,
        panel: Option<&AnyPanel>,
        cx: &mut Context<Self>,
    ) -> Option<Entity<PlaybackBar>> {
        let documents = Documents::global(cx);
        let playback = panel.and_then(|panel| documents.read(cx).playback(panel.document()));
        let Some((name, clock)) = playback else {
            self.playback = None;
            return None;
        };
        match self.playback.as_ref() {
            Some(bar) if *bar.read(cx).clock() == clock => {
                if *bar.read(cx).name() != name {
                    bar.update(cx, |bar, cx| bar.rename(name, cx));
                }
            }
            _ => self.playback = Some(cx.new(|cx| PlaybackBar::new(name, clock, cx))),
        }
        self.playback.clone()
    }

    /// Lays `panel` out where its [`placement`] puts it and shows it there,
    /// framing it alone where the window frames no pane.
    pub(crate) fn add_panel(
        &mut self,
        panel: AnyPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.frame_mut() {
            Some(frame) => {
                let landed = frame.place(panel, window, cx);
                self.activate_pane(landed, window, cx);
            }
            None => self.reframe(Regions::new(panel.role(), panel), window, cx),
        }
    }

    /// Takes the panel the active pane shows into a window of its own, and
    /// focuses it there. A pane left with no panel leaves the arrangement.
    pub fn detach_active_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.active_panel(cx) {
            self.move_panel(panel.entity_id(), Destination::NewWindow, window, cx);
        }
    }

    fn close_pane(&mut self, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_mut() else {
            return;
        };
        let removal = frame.rearrange(cx).remove(pane);
        if removal != Removal::NotFound {
            // A pane out of the arrangement is no longer this workspace's to
            // answer for: an emptied one goes on holding the panel it gave up,
            // so its events would otherwise reach here as well as the workspace
            // the panel moved to.
            frame.panes.remove(&pane.entity_id());
        }
        match removal {
            Removal::NotFound => {}
            Removal::Emptied => self.frame_nothing(window, cx),
            Removal::Removed => {
                // A close can take away the boundary a drag is holding, and the
                // overlay that carries the drag goes with it.
                frame.divider_drag = None;
                if *pane == frame.active
                    && let Some(next) = frame.arrangement.layout().panes().next().cloned()
                {
                    self.activate_pane(next, window, cx);
                }
                cx.notify();
            }
        }
    }

    /// Leaves the window framing nothing: closed where the session holds
    /// another window open, and standing empty where it is the last, so that
    /// only closing a window itself ends the application, or where a dialog
    /// awaits an answer the window is to act on.
    fn frame_nothing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_body(Body::Empty);
        self.playback = None;
        let here = cx.entity_id();
        let closed = self.dialogs.is_empty()
            && Documents::global(cx).update(cx, |documents, cx| {
                documents.close_window_unless_last(here, cx)
            });
        if closed {
            window.remove_window();
        } else {
            self.show_active(window, cx);
        }
    }

    /// Makes `pane` the active one, which the panel actions act on and whose
    /// panel takes focus and titles the window. A pane the arrangement hides
    /// brings every pane back on screen first: a pane no frame draws is nothing
    /// for focus and the window's title to follow, so every path that brings a
    /// view forward passes through here.
    fn activate_pane(&mut self, pane: Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_mut() else {
            return;
        };
        let revealed = frame.arrangement.uncover(&pane);
        mark_revealed(revealed, cx);
        frame.active = pane;
        self.show_active(window, cx);
    }

    fn move_divider(&mut self, divider: DividerId, share: f32, cx: &mut Context<Self>) {
        if let Some(frame) = self.frame_mut()
            && frame.rearrange(cx).move_divider(divider, share)
        {
            cx.notify();
        }
    }

    /// Puts the split `divider` parts back to rest, and lets go of whatever
    /// boundary a drag is holding, so that the press ahead of this one leaves
    /// no drag running behind it.
    fn rest_split(&mut self, divider: DividerId, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_mut() else {
            return;
        };
        frame.divider_drag = None;
        frame.rearrange(cx).rest_split(divider);
        cx.notify();
    }

    /// Gives every side of the innermost split laying `pane` out the same share
    /// of that split's extent. A window of one pane lays it out in no split,
    /// and there is nothing to even.
    fn even_pane_split(&mut self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_mut() else {
            return;
        };
        let layout = frame.rearrange(cx);
        if let Some(divider) = layout.innermost_divider(pane) {
            layout.even_split(divider);
            cx.notify();
        }
    }

    /// Tells every pane how the window frames it and whether it is the active
    /// pane. The arrangement is the workspace's to know, and a frame draws a
    /// window from its root, so every pane hears this before it draws itself.
    fn tell_panes_where_they_stand(&self, cx: &mut Context<Self>) {
        let Some(frame) = self.frame() else {
            return;
        };
        let here = cx.entity_id();
        let elsewhere = Documents::global(cx).read(cx).holds_another_window(here);
        let layout = frame.arrangement.layout();
        let lone = layout.panes().nth(1).is_none();
        for pane in layout.panes() {
            let framing = if frame.arrangement.maximized() == Some(pane) {
                Framing::Maximized
            } else if lone {
                Framing::Lone { elsewhere }
            } else {
                Framing::Beside
            };
            let active = *pane == frame.active;
            pane.update(cx, |pane, _| {
                pane.set_framing(framing);
                pane.set_window_active(active);
            });
        }
    }

    /// Settles where a drop of the panel being dragged would put it, once for
    /// the frame about to be drawn: the pane that would take it among its tabs
    /// is told so, the mark this frame draws stands there, and the release
    /// ending the drag lands there. Told rather than read back: only the
    /// workspace knows the drag, and only it knows the whole arrangement the
    /// drag is aimed across.
    fn aim_panel_drag(&mut self, window: &Window, cx: &mut Context<Self>) {
        let to = self.drag_destination(window, cx);
        let Body::Framed(frame) = &mut self.body else {
            return;
        };
        for pane in frame.arrangement.layout().panes() {
            let joins = joined_pane(to.as_ref()) == Some(pane);
            pane.update(cx, |pane, _| pane.set_joined_by_drag(joins));
        }
        match self.carrying.incoming().is_some() {
            true => self.carrying.aim(to),
            false => {
                if let Some(drag) = frame.panel_drag.as_mut() {
                    drag.to = to;
                }
            }
        }
    }

    /// Where a drop of the panel a drag over the window is holding would put
    /// it, as the geometry the last frame laid out reads where that drag is.
    /// The drag the window system carries over the window is read ahead of the
    /// press this window carries: a press the window system took over is no
    /// longer carried here, so a window holding both is the one a handed-over
    /// drag has come back to, and the window system's reading of it is the live
    /// one. `None` while no drag is over the window, while a press is not yet a
    /// drag of the panel it landed on, and where the drag is aimed at nowhere
    /// the panel could land.
    fn drag_destination(&self, window: &Window, cx: &App) -> Option<Destination> {
        let (panel, at) = match self.carrying.incoming() {
            Some(incoming) => (incoming.panel, incoming.at),
            None => {
                let drag = self.frame()?.panel_drag.as_ref()?;
                (drag.panel, drag.carried.dragging()?)
            }
        };
        self.destination_at(at, panel, window, cx)
    }

    /// Where a drop of `panel` at `at` of the window would put it. The pane it
    /// comes out of is a pane of this window only where the drag started here;
    /// one carried in from another window comes out of none of them, and every
    /// destination re-homes it.
    fn destination_at(
        &self,
        at: Point<Pixels>,
        panel: EntityId,
        window: &Window,
        cx: &App,
    ) -> Option<Destination> {
        let frame = self.frame()?;
        let from = frame.holder(panel, cx).map(|(pane, _)| pane);
        let geometry = frame.pane_geometry(cx);
        drop::dropped_at(
            at,
            viewport(window),
            self.body_bounds?,
            &geometry,
            from.as_ref(),
            |to| self.re_homes(to, from.as_ref(), cx),
        )
    }

    /// What the window's body draws: the pane given the whole of it, every
    /// pane as the arrangement lays them out, or what an empty window offers.
    /// A pane given the whole body is drawn somewhere other than where the
    /// arrangement lays it out, so that frame measures no pane and the
    /// placements the arrangement was last drawn at go on saying where its
    /// panes stand.
    fn body_element(&self, colors: &ThemeColors, cx: &mut Context<Self>) -> AnyElement {
        let Some(frame) = self.frame() else {
            let any_open = !Documents::global(cx).read(cx).is_empty();
            let open = self.open_files_button(cx);
            let openings = self.opening_blocks(colors, cx);
            return empty_state(colors, any_open, openings, open).into_any_element();
        };
        match frame.arrangement.maximized() {
            Some(pane) => self.framed_pane(pane, cx).into_any_element(),
            None => self.layout_element(frame.arrangement.layout(), colors, cx),
        }
    }

    /// An arrangement laid out: panes side by side or one above the other, each
    /// side of a split at the share of it the split gives that side.
    fn layout_element(
        &self,
        layout: &Layout,
        colors: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match layout {
            Layout::Pane(pane) => self.pane_element(pane, cx),
            Layout::Split(split) => self.split_element(split, colors, cx),
        }
    }

    /// A pane where the arrangement lays it out, measured so that a later
    /// gesture can read where it stands.
    fn pane_element(&self, pane: &Entity<Pane>, cx: &mut Context<Self>) -> AnyElement {
        self.framed_pane(pane, cx)
            .child(measure(pane, cx))
            .into_any_element()
    }

    fn framed_pane(&self, pane: &Entity<Pane>, cx: &mut Context<Self>) -> Stateful<Div> {
        let handle = pane.clone();
        div()
            .id(ElementId::NamedInteger(
                "workspace-pane".into(),
                pane.entity_id().as_u64(),
            ))
            // A panel is laid out as the window's own column lays one out, so
            // that a pane frames it exactly as a window holding it alone does.
            .flex()
            .flex_col()
            .size_full()
            .child(pane.clone())
            // A press anywhere in a pane aims the panel actions at it. Taken in
            // the capture phase, so a panel that makes something of the press
            // itself does not keep it from reaching here; and on a layer over
            // the panel, so a part of the panel that blocks the mouse from what
            // lies under it does not either. A deferred draw lands above every
            // such layer, so a popup that blocks the mouse where it overhangs a
            // neighbouring pane does not activate the neighbour.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .capture_any_mouse_down(cx.listener(move |workspace, _, window, cx| {
                        if workspace.active_pane() != Some(&handle) {
                            workspace.activate_pane(handle.clone(), window, cx);
                        }
                    })),
            )
    }

    fn split_element(
        &self,
        split: &Split,
        colors: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let axis = split.axis();
        let mut container = match axis {
            Axis::Horizontal => div().flex_row(),
            Axis::Vertical => div().flex_col(),
        }
        .flex()
        .size_full();
        let mut boundaries = 0;
        let mut dragged = None;
        for SplitSide {
            divider: against,
            layout,
            share,
        } in split.sides()
        {
            if let Some(id) = against {
                let dragging = self
                    .frame()
                    .is_some_and(|frame| frame.divider_drag == Some(id));
                container = container.child(divider(id, axis, dragging, colors, cx));
                if dragging {
                    dragged = Some((id, boundaries));
                }
                boundaries += 1;
            }
            let held = self.layout_element(layout, colors, cx);
            container = container.child(side(held, share, axis));
        }
        container
            .when_some(dragged, |container, (id, boundary)| {
                container.child(divider_drag(id, axis, boundary, boundaries, cx))
            })
            .into_any_element()
    }

    /// What a drag of a panel puts over the window while it runs: for the press
    /// this window is carrying, the handlers that carry it and — once it is a
    /// drag — the panel's ghost under the cursor and a mark over where a drop
    /// would put it; for a drag the window system carries over the window, the
    /// mark alone, the window system drawing the panel and holding the pointer
    /// that ends the gesture. Drawn over the panes, and moved by notifying this
    /// workspace alone, so that every panel's cached layer replays through the
    /// drag instead of repainting per frame.
    fn panel_drag_overlay(
        &self,
        colors: &ThemeColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(incoming) = self.carrying.incoming() {
            let to = incoming.to.as_ref()?;
            let mark = self.marked(to, incoming.at, colors, cx)?;
            return Some(deferred(mark).into_any_element());
        }
        let drag = self.frame()?.panel_drag.as_ref()?;
        let mut over = div().absolute().inset_0().child(panel_drag_handlers(cx));
        if let Some(at) = drag.carried.dragging() {
            let marked = drag
                .to
                .as_ref()
                .and_then(|to| self.marked(to, at, colors, cx));
            over = over
                .children(marked)
                .children(self.ghost(drag.panel, at, colors, window, cx));
        }
        Some(deferred(over).into_any_element())
    }

    /// The mark over where a drop of the panel being dragged would put it: a
    /// fill over the region `to` would give it, or a caret at the boundary
    /// between tabs it would join.
    fn marked(
        &self,
        to: &Destination,
        at: Point<Pixels>,
        colors: &ThemeColors,
        cx: &App,
    ) -> Option<Div> {
        let frame = self.frame()?;
        let geometry = frame.pane_geometry(cx);
        let mark = drop::preview(to, at, self.body_bounds?, &geometry, |side| {
            frame.arrangement.layout().at_edge(side)
        })?;
        Some(match mark {
            Preview::Region(region) => placed(region)
                .bg(colors.accent.opacity(MARK_FILL))
                .border(DRAG_EDGE)
                .border_color(colors.accent),
            Preview::Caret(caret) => placed(caret).bg(colors.accent),
        })
    }

    /// The dragged panel under the cursor, named as its tab names it.
    fn ghost(
        &self,
        panel: EntityId,
        at: Point<Pixels>,
        colors: &ThemeColors,
        window: &Window,
        cx: &App,
    ) -> Option<Div> {
        let (pane, index) = self.holder(panel, cx)?;
        let panel = pane.read(cx).panel_at(index)?.clone();
        let ink = colors.text;
        Some(
            beside_cursor(at, window.viewport_size())
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px(GHOST_PADDING)
                .overflow_hidden()
                .rounded(GHOST_RADIUS)
                .bg(colors.surface)
                .border(DRAG_EDGE)
                .border_color(colors.accent)
                // The ghost is drawn over the window rather than in the chrome,
                // so it starts a text cascade of its own.
                .font_family(bviz_ui_assets::UI)
                .text_size(GHOST_TEXT)
                .text_color(ink)
                .children(panel.icon(cx).map(|name| {
                    bviz_ui_assets::icon(name)
                        .size(GHOST_ICON_SIZE)
                        .text_color(ink)
                }))
                .child(div().truncate().child(panel.display_title())),
        )
    }
}

/// Where the ghost is drawn for a cursor at `at` in a window of `viewport`:
/// down and right of the cursor, and back the other way against an edge it
/// would otherwise hang over, so that a drag reaching for the window's own edge
/// can still be read.
fn beside_cursor(at: Point<Pixels>, viewport: Size<Pixels>) -> Div {
    let ghost = div().absolute().max_w(GHOST_MAX_WIDTH).h(GHOST_HEIGHT);
    let ghost = match at.x + GHOST_OFFSET + GHOST_MAX_WIDTH > viewport.width {
        true => ghost.right(viewport.width - at.x + GHOST_OFFSET),
        false => ghost.left(at.x + GHOST_OFFSET),
    };
    match at.y + GHOST_OFFSET + GHOST_HEIGHT > viewport.height {
        true => ghost.bottom(viewport.height - at.y + GHOST_OFFSET),
        false => ghost.top(at.y + GHOST_OFFSET),
    }
}

/// `placed` with each of `leading` and `others` that is `unplaced` pushed into
/// its role's region, in the order given. Where nothing is placed, every panel
/// is unplaced and `leading` seeds the regions.
fn put_back<T>(
    placed: Option<Regions<T>>,
    leading: T,
    others: Vec<T>,
    role: impl Fn(&T) -> Role,
    unplaced: impl Fn(&T) -> bool,
) -> Regions<T> {
    let mut regions = match placed {
        None => Regions::new(role(&leading), leading),
        Some(mut placed) => {
            if unplaced(&leading) {
                placed.push(role(&leading), leading);
            }
            placed
        }
    };
    regions.extend(
        others
            .into_iter()
            .filter(|panel| unplaced(panel))
            .map(|panel| (role(&panel), panel)),
    );
    regions
}

/// The panes `regions` describes, each holding the panels the region gives it
/// and showing the first of them; the pane holding the leading panel of
/// `regions`, which a window built around it opens on; and every pane among
/// them for the caller to watch.
fn framed_panes(
    regions: Regions<AnyPanel>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> (Layout, Entity<Pane>, Vec<Entity<Pane>>) {
    let leading = regions.leading().entity_id();
    let group = regions.arrange();
    let mut panes = Vec::new();
    let mut leading_pane = None;
    let layout = group.arrange(&mut |panel, tabbed| {
        let leads = panel.entity_id() == leading;
        let pane = cx.new(|cx| Pane::new(panel, window, cx));
        if leads {
            leading_pane = Some(pane.clone());
        }
        pane.update(cx, |pane, cx| {
            for tab in tabbed {
                pane.add(tab, window, cx);
            }
            pane.activate(0, window, cx);
        });
        panes.push(pane.clone());
        pane
    });
    let leading_pane = leading_pane.expect("regions show their leading panel first in its pane");
    (layout, leading_pane, panes)
}

/// Whether moving a panel to `to` would lay it out anywhere other than where
/// the pane `from` holding it already stands, `alone` saying whether that pane
/// holds it and no other, and both saying nothing at all where the panel comes
/// from another window and every destination re-homes it. A destination has to
/// name a pane the arrangement lays out; and a pane holding one panel is already
/// that panel's own pane, so a destination asking for a pane of its own where
/// that one stands — `from` itself, or an edge of the window `from` already
/// spans — is refused: the move would take that pane out and build an equivalent
/// one at an even share of its split, putting the dividers around it back where
/// a drag had moved them from. A window of its own is a destination only where
/// `host` opens windows.
pub(crate) fn re_homes<P: Clone + PartialEq>(
    to: &Destination<P>,
    from: Option<&P>,
    alone: bool,
    layout: &Layout<P>,
    host: Host,
) -> bool {
    match to {
        Destination::Tab { pane, .. } | Destination::Beside { pane, .. } => {
            layout.contains(pane) && !(alone && Some(pane) == from)
        }
        Destination::WindowEdge(side) => {
            !(alone && from.is_some_and(|from| layout.spans_edge(from, *side)))
        }
        Destination::NewWindow => host.opens_windows(),
    }
}

/// Tells the user why a panel moved toward a window of its own stays put.
fn no_window_opened(error: &impl std::fmt::Display) {
    eprintln!("error: no window opened for the panel: {error:#}");
}

/// The pane a drop at `to` would put the panel among the tabs of. Nothing where
/// the drop would put it anywhere else — a pane of its own, an edge of the
/// window, a window of its own — and nothing where the drag is aimed at nowhere
/// the panel could be dropped at all.
fn joined_pane<P>(to: Option<&Destination<P>>) -> Option<&P> {
    match to? {
        Destination::Tab { pane, .. } => Some(pane),
        Destination::Beside { .. } | Destination::WindowEdge(_) | Destination::NewWindow => None,
    }
}

/// Whether a frame of the window `arrangement` is laid out in shows the panel
/// `holder` names the pane and tab of, `showing` giving which tab each pane
/// shows: the pane has to be showing that tab and to be a pane the arrangement
/// draws at all. `None` where no pane holds the panel.
fn shows_panel<P: Clone + PartialEq>(
    arrangement: &Arrangement<P>,
    holder: Option<(P, usize)>,
    showing: impl Fn(&P) -> usize,
) -> Option<bool> {
    let (pane, index) = holder?;
    Some(showing(&pane) == index && !arrangement.hides(&pane))
}

/// An element filling `bounds` of the window, for an overlay whose parent is the
/// window itself.
fn placed(bounds: Bounds<Pixels>) -> Div {
    div()
        .absolute()
        .left(bounds.left())
        .top(bounds.top())
        .w(bounds.size.width)
        .h(bounds.size.height)
}

/// What a move to `at` does to a press being carried in a window of `viewport`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Carry {
    /// Nothing: the press has not travelled far enough from where it landed to
    /// be a drag of the panel under it.
    Holds,
    /// Carries the drag on within the window.
    Here,
    /// Offers the drag to the window system, the cursor having left the window
    /// by as much as a release out there needs to ask for a window of its own
    /// ([`drop::tears_out`]), so that a drag flicked past the edge and back in
    /// is carried here as ever.
    Over,
}

/// `refused` says the window system has already turned this gesture down, which
/// it answers the same way for the rest of it.
fn carried_to(
    carried: Carried,
    refused: bool,
    at: Point<Pixels>,
    viewport: Bounds<Pixels>,
) -> Carry {
    match carried {
        Carried::Pressed(from) if !past_threshold(from, at) => Carry::Holds,
        _ if refused => Carry::Here,
        _ => match drop::tears_out(at, viewport) {
            true => Carry::Over,
            false => Carry::Here,
        },
    }
}

/// The whole of the window, which a drag reads its own edges against.
fn viewport(window: &Window) -> Bounds<Pixels> {
    Bounds::new(point(px(0.), px(0.)), window.viewport_size())
}

/// The mime type a drag of a panel offers. A panel is a view the process the
/// drag started in holds, which no other process — another of this application
/// among them — could make anything of, so the type names the process as well
/// as the application: a window of another bviz then finds nothing it takes on
/// offer, and refuses the drop rather than accepting one it cannot carry out.
/// The browser exposes no process id, so a page's type names the application
/// alone.
fn panel_mime() -> &'static str {
    static MIME: OnceLock<String> = OnceLock::new();
    MIME.get_or_init(|| {
        let application = "application/x-bviz-panel";
        if cfg!(target_family = "wasm") {
            application.to_string()
        } else {
            format!("{application};pid={}", std::process::id())
        }
    })
}

/// Whether a press carried from `pressed` to `at` has travelled far enough to be
/// a drag of the panel it landed on.
fn past_threshold(pressed: Point<Pixels>, at: Point<Pixels>) -> bool {
    let carried = |from: Pixels, to: Pixels| (to - from).abs() > DRAG_THRESHOLD;
    carried(pressed.x, at.x) || carried(pressed.y, at.y)
}

/// Where a panel goes back in among a pane's tabs to land at the `index` among
/// them a drag marked. `taken` gives where it came out of that same pane, every
/// tab after it having moved a place nearer the front as it went, so an index
/// past it is one place too far; a panel out of another pane leaves the
/// boundaries where they were and lands at the one it was carried to.
fn landing_index(index: usize, taken: Option<usize>) -> usize {
    match taken {
        Some(at) if index > at => index - 1,
        _ => index,
    }
}

/// A canvas over the pane, which draws nothing and tells the workspace where the
/// frame laid that pane out.
fn measure(pane: &Entity<Pane>, cx: &mut Context<Workspace>) -> Canvas<()> {
    let workspace = cx.entity();
    let pane = pane.entity_id();
    overlay_canvas(move |bounds, _, cx| {
        workspace.update(cx, |workspace, _| workspace.laid_out(pane, bounds));
    })
}

/// A canvas over the arrangement, which draws nothing and tells the workspace
/// the region of the window its panes are laid out across.
fn measure_body(cx: &mut Context<Workspace>) -> Canvas<()> {
    let workspace = cx.entity();
    overlay_canvas(move |bounds, _, cx| {
        workspace.update(cx, |workspace, _| workspace.body_bounds = Some(bounds));
    })
}

/// A canvas over the window, rendered while a press on a panel's tab is held,
/// which draws nothing and re-installs the window-global handlers that carry the
/// press past the tab it landed on and past the window's own edge.
fn panel_drag_handlers(cx: &mut Context<Workspace>) -> Canvas<()> {
    let workspace = cx.entity();
    overlay_canvas(move |_, window, _| {
        track_drag(
            window,
            &workspace,
            |workspace, at, window, cx| workspace.drag_panel(at, window, cx),
            |workspace, at, end, window, cx| match end {
                DragEnd::Released => workspace.drop_panel(at, window, cx),
                // An abandoned gesture reports only where the cursor was, not
                // where the button was let go, so it says nothing about where
                // the panel was meant to land.
                DragEnd::Abandoned => workspace.end_panel_drag(cx),
            },
        );
    })
}

/// Which pane `side` of `from` a direction points at, out of the panes `layout`
/// lays out, `bounds` giving where each of them was laid out. Any pane without
/// bounds leaves the answer [`Towards::Unknown`]: the pane a frame has yet to
/// measure may be the very one on that side.
fn in_direction<P: Clone + PartialEq>(
    layout: &Layout<P>,
    from: &P,
    side: Side,
    bounds: impl Fn(&P) -> Option<Bounds<Pixels>>,
) -> Towards<P> {
    let mut others = Vec::with_capacity(layout.panes().count());
    for pane in layout.panes().filter(|pane| *pane != from) {
        let Some(laid_out) = bounds(pane) else {
            return Towards::Unknown;
        };
        others.push((pane.clone(), laid_out));
    }
    // Nothing at all is laid out beyond any edge of the only pane of an
    // arrangement, whatever bounds a frame gave it.
    if others.is_empty() {
        return Towards::Edge;
    }
    let Some(here) = bounds(from) else {
        return Towards::Unknown;
    };
    match nearest(here, side, others) {
        Some(pane) => Towards::Found(pane),
        None => Towards::Edge,
    }
}

/// Of the panes laid out on `side` of `from`, the one a direction points at:
/// those whose span across that side's axis overlaps `from`'s come first, then
/// the nearest along that axis, then the nearest across it, and then whichever
/// was offered first.
fn nearest<T>(
    from: Bounds<Pixels>,
    side: Side,
    candidates: impl IntoIterator<Item = (T, Bounds<Pixels>)>,
) -> Option<T> {
    candidates
        .into_iter()
        .filter_map(|(pane, bounds)| Some((pane, reach(from, side, bounds)?)))
        .min_by(|(_, reached), (_, against)| {
            reached
                .0
                .cmp(&against.0)
                .then(reached.1.total_cmp(&against.1))
                .then(reached.2.total_cmp(&against.2))
        })
        .map(|(pane, _)| pane)
}

/// How far `other` is from `from` in the direction of `side`: whether their
/// spans across that side's axis are apart, how far along that axis the two are
/// parted, and how far apart their middles are across it. `None` where `other`
/// is not laid out beyond `from`'s edge on that side at all.
fn reach(from: Bounds<Pixels>, side: Side, other: Bounds<Pixels>) -> Option<(bool, f32, f32)> {
    let (from_start, from_end) = span(from, side.axis());
    let (other_start, other_end) = span(other, side.axis());
    let parted = match side.leads() {
        true => from_start - other_end,
        false => other_start - from_end,
    };
    if parted < 0.0 {
        return None;
    }
    let (from_near, from_far) = span(from, side.axis().invert());
    let (other_near, other_far) = span(other, side.axis().invert());
    let apart = other_far <= from_near || other_near >= from_far;
    let across = ((other_near + other_far) - (from_near + from_far)).abs() / 2.0;
    Some((apart, parted, across))
}

/// Where `bounds` starts and ends along `axis`.
fn span(bounds: Bounds<Pixels>, axis: Axis) -> (f32, f32) {
    match axis {
        Axis::Horizontal => (f32::from(bounds.left()), f32::from(bounds.right())),
        Axis::Vertical => (f32::from(bounds.top()), f32::from(bounds.bottom())),
    }
}

/// The grab band at one boundary between a split's sides, [`DIVIDER_GRIP`]
/// across.
fn divider(
    id: DividerId,
    axis: Axis,
    dragging: bool,
    colors: &ThemeColors,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let line = if dragging {
        colors.accent
    } else {
        colors.border
    };
    Divider::new(
        ElementId::NamedInteger("split-divider".into(), id.as_u64()),
        axis,
        cx.listener(move |workspace, event: &MouseDownEvent, _, cx| {
            CHROME_GESTURES.dispatch(Pointer::press(event), |intent| match intent {
                ChromeIntent::RestSplit => {
                    workspace.rest_split(id, cx);
                    true
                }
                ChromeIntent::ResizeSplit => {
                    if let Some(frame) = workspace.frame_mut() {
                        frame.divider_drag = Some(id);
                    }
                    cx.notify();
                    true
                }
                _ => false,
            });
        }),
    )
    .band(DIVIDER_GRIP)
    .line(DIVIDER_LINE, line)
}

/// One side of a split, taking `share` of the room the split's dividers leave
/// its sides, and clipping whatever it holds to the room it is given.
fn side(content: AnyElement, share: f32, axis: Axis) -> Div {
    let side = div()
        .flex_basis(px(0.))
        .flex_grow(share)
        .overflow_hidden()
        .child(content);
    match axis {
        Axis::Horizontal => side.h_full().min_w(px(0.)),
        Axis::Vertical => side.w_full().min_h(px(0.)),
    }
}

/// A canvas over the split, rendered while one of its dividers is being dragged,
/// which draws nothing and re-installs the window-global handlers that carry the
/// drag past the divider and past the window's own edge.
fn divider_drag(
    id: DividerId,
    axis: Axis,
    boundary: usize,
    boundaries: usize,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let workspace = cx.entity();
    overlay_canvas(move |bounds, window, _| {
        track_drag(
            window,
            &workspace,
            move |workspace, position, _, cx| {
                if let Some(share) = grabbed_share(bounds, position, axis, boundary, boundaries) {
                    workspace.move_divider(id, share, cx);
                }
            },
            move |workspace, _, _: DragEnd, _, cx| {
                if let Some(frame) = workspace.frame_mut() {
                    frame.divider_drag = None;
                }
                cx.notify();
            },
        );
    })
}

/// The share of `bounds` the divider at `boundary` leaves the sides ahead of it
/// when dragged to `position`, measured over the room the sides share — what is
/// left of the split once each of its `boundaries` bands has taken its own
/// extent, and what the bands ahead of this one push it past. `None` for a split
/// with no room to divide.
fn grabbed_share(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    axis: Axis,
    boundary: usize,
    boundaries: usize,
) -> Option<f32> {
    let (along, extent) = match axis {
        Axis::Horizontal => (position.x - bounds.origin.x, bounds.size.width),
        Axis::Vertical => (position.y - bounds.origin.y, bounds.size.height),
    };
    let grip = f32::from(DIVIDER_GRIP);
    let room = f32::from(extent) - grip * boundaries as f32;
    if room <= 0.0 {
        return None;
    }
    Some((f32::from(along) - grip * (boundary as f32 + 0.5)) / room)
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let panel = self.active_panel(cx);
        let panel = panel.as_ref();
        self.tell_panes_where_they_stand(cx);
        self.aim_panel_drag(window, cx);
        // The room the chrome's bands leave, asked for as the room left rather
        // than as the whole of the window, which would squeeze those bands out
        // of the height each of them asks for.
        let room_left = || div().flex().flex_col().flex_1().min_h(px(0.)).w_full();
        let panes = room_left()
            .child(self.body_element(&colors, cx))
            .child(measure_body(cx));
        let playback = self.playback_bar(panel, cx);
        let notes = self.note_strips(&colors, cx);
        let key_context = self.key_context(cx);
        let title = self.window_title(panel, cx);
        if title != self.titled {
            window.set_window_title(&title);
            self.titled.clone_from(&title);
        }
        let chrome = Chrome::new(&self.window_focus)
            .title(title)
            .menu(self.documents_menu.clone())
            .child(room_left().children(notes).child(panes).children(playback));
        let chrome = match panel.and_then(|panel| panel.status(cx)) {
            Some(status) => chrome.status(status),
            None => chrome,
        };
        let body = div()
            .key_context(key_context)
            .size_full()
            .relative()
            .on_action(cx.listener(|workspace, _: &OpenFiles, window, cx| {
                workspace.open_files(window, cx);
            }))
            .on_action(cx.listener(|workspace, _: &NextTab, window, cx| {
                if let Some(pane) = workspace.active_pane().cloned() {
                    pane.update(cx, |pane, cx| pane.activate_next(window, cx));
                }
            }))
            .on_action(cx.listener(|workspace, _: &PreviousTab, window, cx| {
                if let Some(pane) = workspace.active_pane().cloned() {
                    pane.update(cx, |pane, cx| pane.activate_previous(window, cx));
                }
            }))
            .on_action(cx.listener(|workspace, _: &ClosePanel, window, cx| {
                if let Some(pane) = workspace.active_pane().cloned() {
                    pane.update(cx, |pane, cx| pane.close_active(window, cx));
                }
            }))
            .on_action(cx.listener(|workspace, _: &CloseDocument, window, cx| {
                if let Some(panel) = workspace.active_panel(cx) {
                    workspace.close_document(panel.document(), window, cx);
                }
            }))
            .on_action(cx.listener(|workspace, _: &DetachPanel, window, cx| {
                workspace.detach_active_panel(window, cx);
            }))
            .on_action(cx.listener(|workspace, _: &ToggleMaximize, _, cx| {
                if let Some(pane) = workspace.active_pane().cloned() {
                    workspace.toggle_maximize(&pane, cx);
                }
            }))
            .on_action(cx.listener(|workspace, _: &EvenSplit, _, cx| {
                if let Some(pane) = workspace.active_pane().cloned() {
                    workspace.even_pane_split(&pane, cx);
                }
            }))
            .on_action(
                cx.listener(|workspace, _: &ShowKeysAndGestures, window, cx| {
                    if let Some(panel) = workspace.active_panel(cx) {
                        workspace.toggle_keys_help(panel.entity_id(), window, cx);
                    }
                }),
            )
            .on_action(cx.listener(|workspace, _: &ResetArrangement, window, cx| {
                workspace.reset_arrangement(window, cx);
            }))
            .on_action(cx.listener(|workspace, _: &PlayPause, _, cx| {
                workspace.drive_clock(cx, Clock::play_pause);
            }))
            .on_action(cx.listener(|workspace, _: &StepBack, _, cx| {
                workspace.drive_clock(cx, |clock, cx| clock.step(false, cx));
            }))
            .on_action(cx.listener(|workspace, _: &StepForward, _, cx| {
                workspace.drive_clock(cx, |clock, cx| clock.step(true, cx));
            }))
            .on_action(cx.listener(|workspace, _: &SeekStart, _, cx| {
                workspace.drive_clock(cx, Clock::seek_start);
            }))
            .on_action(cx.listener(|workspace, _: &SeekEnd, _, cx| {
                workspace.drive_clock(cx, Clock::seek_end);
            }))
            .on_action(cx.listener(|workspace, _: &SpeedUp, _, cx| {
                workspace.drive_clock(cx, |clock, cx| clock.shift_speed(1, cx));
            }))
            .on_action(cx.listener(|workspace, _: &SpeedDown, _, cx| {
                workspace.drive_clock(cx, |clock, cx| clock.shift_speed(-1, cx));
            }))
            // Not a listener: `toggle` reads this workspace.
            .on_action({
                let menu = self.documents_menu.clone();
                move |_: &ToggleDocuments, window, cx| {
                    menu.update(cx, |menu, cx| menu.toggle(window, cx));
                }
            });
        let app_actions = AppActions::of(cx).to_vec();
        let body = (app_actions.iter()).fold(body, |body, action| action.listen(body, cx));
        let body = towards::<FocusLeft, MovePanelLeft, SplitLeft>(body, Side::Left, cx);
        let body = towards::<FocusRight, MovePanelRight, SplitRight>(body, Side::Right, cx);
        let body = towards::<FocusUp, MovePanelUp, SplitUp>(body, Side::Top, cx);
        let body = towards::<FocusDown, MovePanelDown, SplitDown>(body, Side::Bottom, cx);
        let dragging = self.panel_drag_overlay(&colors, window, cx);
        let file_drop = self.file_drop_target(&colors, cx);
        let dialog = self.dialogs.front().map(|asking| {
            let sheet = div().child(asking.dialog.clone());
            dialog::over_window(sheet, &asking.answer, window, &colors)
        });
        let menu = self
            .frame()
            .and_then(|frame| frame.panel_menu.as_ref())
            .map(|open| deferred(open.menu.clone()).with_priority(MENU_LAYER));
        let body = body
            .child(chrome)
            .children(file_drop)
            .children(dragging)
            .children(menu);
        // A sibling of the body, so that no key or action of the workspace's
        // reaches through the dialog to what it stands over.
        div().size_full().child(body).children(dialog)
    }
}

/// Handlers for the three actions pointing at `side`.
fn towards<Focus: Action, Move: Action, SplitOff: Action>(
    body: Div,
    side: Side,
    cx: &mut Context<Workspace>,
) -> Div {
    body.on_action(cx.listener(move |workspace, _: &Focus, window, cx| {
        workspace.focus_towards(side, window, cx);
    }))
    .on_action(cx.listener(move |workspace, _: &Move, window, cx| {
        workspace.move_towards(side, window, cx);
    }))
    .on_action(cx.listener(move |workspace, _: &SplitOff, window, cx| {
        workspace.split_towards(side, window, cx);
    }))
}

/// What a window framing no panel shows in its place: the `openings` of files
/// named to it, or, where there are none, that the session holds no document,
/// or, where `any_open` says it holds one, that a view of it is to be shown
/// from the Documents menu; and `open`, where the app opens files.
fn empty_state(
    colors: &ThemeColors,
    any_open: bool,
    openings: Vec<Div>,
    open: Option<Button>,
) -> Div {
    let heading = if any_open {
        "No views shown"
    } else {
        "No documents open"
    };
    let idle = openings.is_empty();
    let heading = idle.then(|| div().text_color(colors.text).child(heading));
    let shown = (idle && any_open).then_some("Show one from the Documents menu.");
    let openings = (!openings.is_empty()).then(|| {
        div()
            .w(OPENINGS_WIDTH)
            .flex()
            .flex_col()
            .gap_2()
            .children(openings)
    });
    let hint = |text: &'static str| div().text_sm().text_color(colors.text_muted).child(text);
    let open = open.map(|open| {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(open)
            .child(hint(DROP_HINT))
    });
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .bg(colors.background)
        .children(heading)
        .children(openings)
        .children(shown.map(hint))
        .children(open)
}

/// `listing`'s heading over a line for each file, its label beside its state,
/// as `note` shows them in text of `text_size`.
fn file_list(
    note: u64,
    (heading, lines): Listing<'_>,
    text_size: AbsoluteLength,
    colors: &ThemeColors,
) -> Stateful<Div> {
    let line = |(at, (label, shown)): (usize, (&SharedString, Shown<'_>))| {
        let state = div().flex_1().min_w_0();
        let state = match shown {
            Shown::Busy(text) => state
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .text_color(colors.text_muted)
                .child(spinner(
                    ("workspace-opening-spinner", at),
                    text_size,
                    colors.text_muted,
                ))
                .child(text),
            Shown::Error(reason) => state.text_color(colors.error).child(reason.clone()),
        };
        div()
            .flex()
            .flex_row()
            .gap_3()
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(relative(LABEL_SHARE))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(colors.text)
                    .child(label.clone()),
            )
            .child(state)
    };
    div()
        .id(("workspace-note-files", note))
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(div().text_color(colors.text).child(heading))
        .children(lines.into_iter().enumerate().map(line))
}

/// How a window reads where another window's menu offers a move into it: what
/// it shows, headed by its `place` among the session's open windows in the
/// order they opened. Two windows can show panels that read exactly alike, so
/// the place is what tells one row from another.
fn window_name(place: usize, shown: &str) -> SharedString {
    format!("{place}. {shown}").into()
}

/// The context stack from the window's root to a panel scoped to `scope`,
/// where the root carries `workspace`.
fn stack_around(workspace: KeyContext, scope: KeyScope) -> Vec<KeyContext> {
    vec![workspace, bviz_ui_shell::KEY_CONTEXT.into(), scope.into()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drop::Band;
    use bviz_ui_components::keymap_probe::{dispatched, is};
    use gpui::{point, size};

    const EXTENT: f32 = 407.0;
    const GRIP: f32 = 7.0;

    /// A split of `EXTENT` along `axis`, offset from the window's own origin so
    /// a share measured from the wrong origin is told from a right one.
    fn split_bounds(axis: Axis) -> Bounds<Pixels> {
        let origin = point(px(30.), px(70.));
        let extent = match axis {
            Axis::Horizontal => size(px(EXTENT), px(100.)),
            Axis::Vertical => size(px(100.), px(EXTENT)),
        };
        Bounds::new(origin, extent)
    }

    /// The share a divider at `boundary`, one of `boundaries` in the split,
    /// leaves the sides ahead of it when grabbed `along` from the split's start.
    fn share(along: f32, axis: Axis, boundary: usize, boundaries: usize) -> Option<f32> {
        let bounds = split_bounds(axis);
        let position = match axis {
            Axis::Horizontal => point(bounds.origin.x + px(along), bounds.origin.y + px(50.)),
            Axis::Vertical => point(bounds.origin.x + px(50.), bounds.origin.y + px(along)),
        };
        grabbed_share(bounds, position, axis, boundary, boundaries)
    }

    /// The share is of the room the sides share, which is what a side is laid
    /// out at: a divider dragged to the middle of that room halves it, and one
    /// dragged to either end leaves the whole of it to one side of itself.
    #[test]
    fn a_divider_leaves_the_share_it_is_dragged_to() {
        for axis in [Axis::Horizontal, Axis::Vertical] {
            assert_eq!(share(GRIP / 2.0, axis, 0, 1), Some(0.0));
            assert_eq!(share(EXTENT / 2.0, axis, 0, 1), Some(0.5));
            assert_eq!(share(EXTENT - GRIP / 2.0, axis, 0, 1), Some(1.0));
        }
    }

    /// Every band takes its extent out of the room the sides divide, and the
    /// bands ahead of a divider push it that much further along the split, so a
    /// later divider reads its share from the same room measured from the same
    /// place.
    #[test]
    fn a_later_divider_reads_its_share_over_the_room_every_band_leaves() {
        let boundaries = 3;
        let room = EXTENT - GRIP * boundaries as f32;
        for boundary in 0..boundaries {
            let start = GRIP * (boundary as f32 + 0.5);
            let axis = Axis::Horizontal;
            assert_eq!(share(start, axis, boundary, boundaries), Some(0.0));
            assert_eq!(share(start + room, axis, boundary, boundaries), Some(1.0));
            assert_eq!(
                share(start + room / 2.0, axis, boundary, boundaries),
                Some(0.5)
            );
        }
    }

    /// A drag runs on past the split it belongs to, and reports where the cursor
    /// went; the share it asks for follows it out and is bounded on the way to
    /// the layout rather than here.
    #[test]
    fn a_divider_dragged_past_the_split_asks_for_a_share_past_its_end() {
        assert!(share(-100.0, Axis::Horizontal, 0, 1).is_some_and(|share| share < 0.0));
        assert!(share(EXTENT + 100.0, Axis::Horizontal, 0, 1).is_some_and(|share| share > 1.0));
    }

    /// A split with no more room than its own dividers has no room to divide.
    #[test]
    fn a_split_with_no_room_for_any_side_has_no_share_to_give() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(GRIP), px(100.)));
        let position = point(px(3.), px(50.));
        assert_eq!(
            grabbed_share(bounds, position, Axis::Horizontal, 0, 1),
            None
        );
    }

    /// The tabs left after the panel at `from` of them is dragged to the
    /// boundary at `index` of the tabs as they are shown, which is a move
    /// through the pane it is already in: out at `from`, and back in where the
    /// caret stood.
    fn reorder(tabs: &[&'static str], from: usize, index: usize) -> Vec<&'static str> {
        let mut tabs = tabs.to_vec();
        let dragged = tabs.remove(from);
        tabs.insert(landing_index(index, Some(from)).min(tabs.len()), dragged);
        tabs
    }

    /// A tab dragged across its own strip lands at the boundary the caret stood
    /// at, whichever way it was carried and however far.
    #[test]
    fn a_tab_dragged_across_its_own_strip_lands_where_the_caret_stood() {
        let tabs = ["a", "b", "c", "d"];
        assert_eq!(reorder(&tabs, 2, 1), ["a", "c", "b", "d"]);
        assert_eq!(reorder(&tabs, 1, 3), ["a", "c", "b", "d"]);
        assert_eq!(reorder(&tabs, 2, 0), ["c", "a", "b", "d"]);
        assert_eq!(reorder(&tabs, 1, 4), ["a", "c", "d", "b"]);
    }

    /// The boundaries either side of a tab are where that tab already stands,
    /// so a drag to one of them leaves the strip in the order it was.
    #[test]
    fn a_tab_dragged_to_either_boundary_of_its_own_place_stays_there() {
        let tabs = ["a", "b", "c"];
        assert_eq!(reorder(&tabs, 1, 1), tabs);
        assert_eq!(reorder(&tabs, 1, 2), tabs);
    }

    /// A panel out of another pane takes none of the tabs of the pane it joins
    /// out as it lands, so it lands at exactly the boundary the caret stood at.
    #[test]
    fn a_tab_dragged_in_from_another_pane_lands_at_the_boundary_untouched() {
        for index in 0..4 {
            assert_eq!(landing_index(index, None), index);
        }
        let mut tabs = vec!["a", "b", "c"];
        tabs.insert(landing_index(2, None), "z");
        assert_eq!(tabs, ["a", "b", "z", "c"]);
    }

    /// A press is carried far enough to be a drag only once one axis alone
    /// takes it past the threshold, so an unsteady hand on a tab is still a
    /// click on it — and a press the platform counts towards a double click,
    /// which it measures axis by axis too, is never also a drag.
    #[test]
    fn a_press_is_a_drag_only_once_one_axis_carries_it_past_the_threshold() {
        let pressed = point(px(100.), px(100.));
        let carried = |dx: f32, dy: f32| {
            past_threshold(pressed, point(pressed.x + px(dx), pressed.y + px(dy)))
        };
        let threshold = f32::from(DRAG_THRESHOLD);
        assert!(!carried(0.0, 0.0));
        assert!(!carried(threshold, 0.0));
        assert!(!carried(threshold, threshold));
        assert!(carried(0.0, -threshold - 1.0));
        assert!(carried(threshold + 1.0, threshold));
    }

    /// The window a drag is read against, wide enough that the tear-out margin
    /// around it stands clear of its middle.
    fn dragged_in() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)))
    }

    /// A press not yet carried far enough to be a drag of the panel under it is
    /// carried nowhere, so a press that slips past the window's edge is still
    /// the click the tab under it makes of one.
    #[test]
    fn a_press_below_the_threshold_is_carried_nowhere() {
        let pressed = Carried::Pressed(point(px(400.), px(4.)));
        for at in [point(px(400.), px(2.)), point(px(404.), px(0.))] {
            assert_eq!(carried_to(pressed, false, at, dragged_in()), Carry::Holds);
        }
    }

    /// A drag is carried within the window right up to the margin a release
    /// past the edge needs to ask for a window of its own, so that one flicked
    /// just past the edge and back in never leaves this window's hands.
    #[test]
    fn a_drag_within_the_tear_out_margin_stays_in_the_window() {
        let carried = |x: f32, y: f32| {
            let at = point(px(x), px(y));
            carried_to(Carried::Dragging(at), false, at, dragged_in())
        };
        assert_eq!(carried(400.0, 300.0), Carry::Here);
        assert_eq!(carried(0.0, 300.0), Carry::Here);
        assert_eq!(carried(-20.0, 300.0), Carry::Here);
        assert_eq!(carried(820.0, 300.0), Carry::Here);
    }

    /// A drag carried further out than that offers the gesture to the window
    /// system, on every side of the window.
    #[test]
    fn a_drag_past_the_tear_out_margin_offers_the_gesture_over() {
        let carried = |x: f32, y: f32| {
            let at = point(px(x), px(y));
            carried_to(Carried::Dragging(at), false, at, dragged_in())
        };
        assert_eq!(carried(-40.0, 300.0), Carry::Over);
        assert_eq!(carried(840.0, 300.0), Carry::Over);
        assert_eq!(carried(400.0, -40.0), Carry::Over);
        assert_eq!(carried(400.0, 640.0), Carry::Over);
    }

    /// One move can both carry a press past the threshold and take it out past
    /// the tear-out margin, and the gesture is offered to the window system on
    /// that one move rather than waiting for another.
    #[test]
    fn one_move_can_both_start_a_drag_and_offer_it_over() {
        let pressed = Carried::Pressed(point(px(400.), px(300.)));
        let at = point(px(-40.), px(300.));
        assert!(past_threshold(point(px(400.), px(300.)), at));
        assert_eq!(carried_to(pressed, false, at, dragged_in()), Carry::Over);
    }

    /// A window system that has turned the gesture down is not asked again: the
    /// drag goes on being carried here, however far out it is taken.
    #[test]
    fn a_refused_gesture_is_carried_on_here() {
        let carried = |x: f32, y: f32| {
            let at = point(px(x), px(y));
            carried_to(Carried::Dragging(at), true, at, dragged_in())
        };
        assert_eq!(carried(400.0, 300.0), Carry::Here);
        assert_eq!(carried(-400.0, 300.0), Carry::Here);
        assert_eq!(carried(400.0, 1200.0), Carry::Here);
    }

    /// A window shows a panel where the pane holding it shows it among its tabs
    /// and the window draws that pane: a panel behind another of its pane's
    /// tabs is not shown, nor is one in a pane the window draws another in
    /// place of.
    #[test]
    fn a_window_shows_a_panel_its_pane_shows_and_the_window_draws() {
        let mut arrangement = Arrangement::new(Layout::split_at(
            Axis::Horizontal,
            0.5,
            Layout::Pane('a'),
            Layout::Pane('b'),
        ));
        // Every pane shows the tab at index 1 of the ones it holds.
        let shows = |arrangement: &Arrangement<char>, pane, tab| {
            shows_panel(arrangement, Some((pane, tab)), |_| 1)
        };
        assert_eq!(shows(&arrangement, 'a', 1), Some(true));
        assert_eq!(shows(&arrangement, 'a', 0), Some(false));
        assert_eq!(shows(&arrangement, 'b', 1), Some(true));
        assert_eq!(arrangement.toggle_maximize(&'a'), Maximize::Maximized);
        assert_eq!(shows(&arrangement, 'a', 1), Some(true));
        assert_eq!(shows(&arrangement, 'b', 1), Some(false));
        assert_eq!(
            shows_panel(&arrangement, None::<(char, usize)>, |_| 1),
            None
        );
    }

    /// `width` by `height` at (`x`, `y`) of the window.
    fn at(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    /// A pane down the left of the window, two halves of the middle one above
    /// the other, and a pane down the right, each parted from its neighbours by
    /// a divider's band.
    fn quartered() -> [(&'static str, Bounds<Pixels>); 4] {
        [
            ("left", at(0.0, 0.0, 400.0, 600.0)),
            ("upper middle", at(410.0, 0.0, 590.0, 295.0)),
            ("lower middle", at(410.0, 305.0, 590.0, 295.0)),
            ("right", at(1010.0, 0.0, 190.0, 600.0)),
        ]
    }

    /// The arrangement `quartered` lays out: the left pane, the two middle ones
    /// one above the other, and the right pane.
    fn quartered_layout() -> Layout<&'static str> {
        let middle = Layout::split_at(
            Axis::Vertical,
            0.5,
            Layout::Pane("upper middle"),
            Layout::Pane("lower middle"),
        );
        Layout::divided(
            Axis::Horizontal,
            (Layout::Pane("left"), 1.0),
            vec![(middle, 1.0), (Layout::Pane("right"), 1.0)],
        )
    }

    /// Where `quartered` lays `pane` out.
    fn laid_out(pane: &&str) -> Option<Bounds<Pixels>> {
        quartered()
            .into_iter()
            .find(|(name, _)| name == pane)
            .map(|(_, bounds)| bounds)
    }

    /// The pane `side` of the one named `from` in `quartered`.
    fn pane_towards(from: &'static str, side: Side) -> Towards<&'static str> {
        in_direction(&quartered_layout(), &from, side, laid_out)
    }

    /// A direction picks the pane laid out beyond that edge of the one it starts
    /// from, and the nearest of them along the direction it points.
    #[test]
    fn a_direction_picks_the_pane_nearest_it_on_that_side() {
        assert_eq!(
            pane_towards("upper middle", Side::Left),
            Towards::Found("left")
        );
        assert_eq!(
            pane_towards("upper middle", Side::Right),
            Towards::Found("right")
        );
        assert_eq!(
            pane_towards("upper middle", Side::Bottom),
            Towards::Found("lower middle")
        );
        assert_eq!(
            pane_towards("lower middle", Side::Top),
            Towards::Found("upper middle")
        );
        assert_eq!(
            pane_towards("right", Side::Left),
            Towards::Found("upper middle")
        );
    }

    /// A pane laid out across an edge rather than beyond it is not on that side
    /// at all, so a direction pointing out of the window reaches the window's
    /// own edge.
    #[test]
    fn a_direction_with_no_pane_beyond_that_edge_reaches_the_windows_own() {
        assert_eq!(pane_towards("left", Side::Left), Towards::Edge);
        assert_eq!(pane_towards("right", Side::Right), Towards::Edge);
        assert_eq!(pane_towards("left", Side::Top), Towards::Edge);
        assert_eq!(pane_towards("upper middle", Side::Top), Towards::Edge);
    }

    /// Every direction out of the one pane of a window reaches the window's own
    /// edge because nothing else is laid out at all, which is an answer the
    /// arrangement gives whether or not a frame has measured that pane.
    #[test]
    fn every_direction_out_of_a_lone_pane_reaches_the_windows_edge() {
        let lone = Layout::Pane("only");
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            assert_eq!(in_direction(&lone, &"only", side, |_| None), Towards::Edge);
        }
    }

    /// A pane a frame has yet to lay out could be the very one on that side, so
    /// a direction out of an arrangement holding one answers neither a pane nor
    /// the window's edge — whether it is the pane the direction starts from that
    /// is unmeasured or any other.
    #[test]
    fn a_direction_out_of_an_unmeasured_arrangement_is_unknown() {
        let without = |unmeasured: &'static str, from: &'static str, side| {
            in_direction(&quartered_layout(), &from, side, |pane| match *pane {
                name if name == unmeasured => None,
                _ => laid_out(pane),
            })
        };
        assert_eq!(without("right", "left", Side::Right), Towards::Unknown);
        assert_eq!(without("left", "left", Side::Right), Towards::Unknown);
    }

    /// Panes equally far along the direction and equally far across it are
    /// picked between by the order they are offered, and a direction offers them
    /// in the order the arrangement lays them out.
    #[test]
    fn a_direction_settles_a_tie_by_the_order_the_panes_are_laid_out() {
        let [left, upper, lower, _] = quartered();
        assert_eq!(
            nearest(left.1, Side::Right, [upper, lower]),
            Some("upper middle")
        );
        assert_eq!(
            nearest(left.1, Side::Right, [lower, upper]),
            Some("lower middle")
        );
        assert_eq!(
            pane_towards("left", Side::Right),
            Towards::Found("upper middle")
        );
    }

    /// A pane whose span across the direction overlaps the one the direction
    /// starts from is picked over one laid out nearer that misses it, so a
    /// direction lands on a pane that is really beside the pane it came from.
    #[test]
    fn a_direction_picks_an_overlapping_pane_over_a_nearer_one_it_misses() {
        let from = at(0.0, 300.0, 200.0, 200.0);
        let missing = ("above", at(210.0, 0.0, 100.0, 100.0));
        let beside = ("beside", at(400.0, 350.0, 100.0, 100.0));
        assert_eq!(
            nearest(from, Side::Right, [missing, beside]),
            Some("beside")
        );
    }

    /// Panes equally far along the direction and both beside the one it starts
    /// from are picked between by how far apart their middles are across it, so
    /// a direction lands on the pane it most nearly points at.
    #[test]
    fn a_direction_settles_an_equal_reach_by_the_nearer_middle() {
        let from = at(0.0, 200.0, 100.0, 200.0);
        let level = ("level", at(200.0, 250.0, 100.0, 100.0));
        let offset = ("offset", at(200.0, 350.0, 100.0, 100.0));
        assert_eq!(nearest(from, Side::Right, [offset, level]), Some("level"));
    }

    /// A pane whose far edge is exactly level with the near edge of the one the
    /// direction starts from is beside it and is picked; one reaching past that
    /// edge lies over it rather than beyond it, and is not.
    #[test]
    fn a_direction_picks_a_pane_that_reaches_level_with_its_own_edge() {
        let from = at(100.0, 0.0, 100.0, 100.0);
        let level = ("level", at(0.0, 0.0, 100.0, 100.0));
        let over = ("over", at(0.0, 0.0, 101.0, 100.0));
        assert_eq!(nearest(from, Side::Left, [level]), Some("level"));
        assert_eq!(nearest(from, Side::Left, [over]), None);
    }

    /// The arrangement a window holding the panels `held`, in that order, is
    /// put back to, the documents having placed `placed`. Each panel's role is
    /// the role of its letter in `roles`.
    fn put_back_shape(placed: &[(Role, char)], held: &str, roles: &[(char, Role)]) -> String {
        let regions = Regions::gather(placed.iter().copied());
        let placed: Vec<char> = placed.iter().map(|(_, panel)| *panel).collect();
        let mut held = held.chars();
        let leading = held.next().expect("a window holds a panel");
        let role = |panel: &char| {
            let named = roles.iter().find(|(named, _)| named == panel);
            named.expect("a role for every held panel").1
        };
        let regions = put_back(regions, leading, held.collect(), role, |panel| {
            !placed.contains(panel)
        });
        crate::layout::shape(&crate::testing::arrange(regions.arrange()))
    }

    /// A panel the documents lay out nowhere joins its role's region behind the
    /// ones they place there, in the order the window held it, and a panel
    /// they place is not laid out again.
    #[test]
    fn a_panel_the_documents_lay_out_nowhere_joins_its_roles_region() {
        let roles = [
            ('a', Role::Canvas),
            ('b', Role::Analysis),
            ('x', Role::Analysis),
            ('y', Role::Canvas),
            ('z', Role::Analysis),
        ];
        let placed = [(Role::Canvas, 'a'), (Role::Analysis, 'b')];
        assert_eq!(put_back_shape(&placed, "zaybx", &roles), "(ay|bzx)");
        assert_eq!(put_back_shape(&placed, "ab", &roles), "(a|b)");
    }

    /// Where the documents lay out none of the window's panels, each joins its
    /// role's region in the order the window held them.
    #[test]
    fn a_window_no_document_lays_out_is_laid_out_by_role() {
        let roles = [('a', Role::Log), ('b', Role::Timeline), ('c', Role::Log)];
        assert_eq!(put_back_shape(&[], "abc", &roles), "(ac|b)");
    }

    /// A press is holding the panel nowhere until it is a drag of it, so what a
    /// drop would do is asked only of a gesture that has become one.
    #[test]
    fn a_press_holds_the_panel_nowhere_until_it_is_a_drag() {
        let at = point(px(120.), px(48.));
        assert_eq!(Carried::Pressed(at).dragging(), None);
        assert_eq!(Carried::Dragging(at).dragging(), Some(at));
    }

    /// A drop among a pane's tabs is the one destination a pane is told of, and
    /// exactly one pane of the arrangement is told of it. A drop that puts the
    /// panel in a pane of its own, against an edge of the window or in a window
    /// of its own joins no tabs, so every pane is told of nothing.
    #[test]
    fn one_pane_is_told_a_drop_would_join_it_and_only_for_a_drop_among_its_tabs() {
        let panes = ["left", "right"];
        let told = |to: Option<Destination<&'static str>>| {
            panes
                .iter()
                .filter(|pane| joined_pane(to.as_ref()) == Some(*pane))
                .count()
        };
        assert_eq!(
            told(Some(Destination::Tab {
                pane: "right",
                index: 1
            })),
            1
        );
        assert_eq!(
            told(Some(Destination::Beside {
                pane: "right",
                side: Side::Left
            })),
            0
        );
        assert_eq!(told(Some(Destination::WindowEdge(Side::Top))), 0);
        assert_eq!(told(Some(Destination::NewWindow)), 0);
        assert_eq!(told(None), 0);
    }

    /// The one panel of a window's one pane is already in a pane of its own, so
    /// a drag of it over that pane's own band re-homes it nowhere: no pane is
    /// told a drop would join it, and the pane goes on naming the panel as it
    /// did.
    #[test]
    fn a_drag_of_a_lone_panel_over_its_own_band_tells_no_pane() {
        let bounds = at(0.0, 40.0, 1200.0, 720.0);
        let lone = PaneGeometry {
            pane: "lone",
            bounds,
            tab_count: 1,
            band: Some(Band {
                bounds: at(0.0, 40.0, 1200.0, 24.0),
                tabs: vec![at(0.0, 40.0, 120.0, 23.0)],
            }),
        };
        let layout = Layout::Pane("lone");
        let to = drop::dropped_at(
            point(px(60.), px(50.)),
            at(0.0, 0.0, 1200.0, 800.0),
            bounds,
            &[lone],
            Some(&"lone"),
            |to| re_homes(to, Some(&"lone"), true, &layout, Host::Desktop),
        );
        assert_eq!(to, None);
        assert_eq!(joined_pane(to.as_ref()), None);
    }

    /// A drag past the tear-out margin asks for a window of its own on the
    /// desktop, and marks nowhere in a browser, which opens none.
    #[test]
    fn a_drag_past_the_tear_out_margin_asks_for_a_window_only_where_one_opens() {
        let bounds = at(0.0, 40.0, 1200.0, 720.0);
        let pane = PaneGeometry {
            pane: "pane",
            bounds,
            tab_count: 2,
            band: Some(Band {
                bounds: at(0.0, 40.0, 1200.0, 24.0),
                tabs: vec![at(0.0, 40.0, 120.0, 23.0), at(120.0, 40.0, 120.0, 23.0)],
            }),
        };
        let layout = Layout::Pane("pane");
        let dropped = |host| {
            drop::dropped_at(
                point(px(-40.), px(300.)),
                at(0.0, 0.0, 1200.0, 800.0),
                bounds,
                std::slice::from_ref(&pane),
                Some(&"pane"),
                |to| re_homes(to, Some(&"pane"), false, &layout, host),
            )
        };
        assert_eq!(dropped(Host::Desktop), Some(Destination::NewWindow));
        assert_eq!(dropped(Host::Browser), None);
    }

    /// A row of a menu names the window a move would put the panel in, so two
    /// windows showing panels that read exactly alike still read apart.
    #[test]
    fn two_windows_showing_the_same_thing_are_named_apart() {
        let first = window_name(1, "build.profile.gz");
        let second = window_name(2, "build.profile.gz");
        assert_ne!(first, second);
        assert!(first.contains("build.profile.gz"), "{first}");
    }

    /// The context stack of a key pressed with focus in a panel of the window.
    fn in_a_panel() -> [&'static str; 3] {
        [KEY_CONTEXT.id(), bviz_ui_shell::KEY_CONTEXT.id(), "Panel"]
    }

    #[test]
    fn a_window_closes_the_document_of_the_panel_with_focus_only_where_the_app_opens_files() {
        assert!(is(
            dispatched(
                bindings(true, Host::Desktop),
                "secondary-alt-w",
                &in_a_panel()
            ),
            CloseDocument
        ));
        assert!(
            dispatched(
                bindings(false, Host::Desktop),
                "secondary-alt-w",
                &in_a_panel()
            )
            .is_none()
        );
    }

    #[test]
    fn a_window_moves_its_panel_to_a_new_window_only_where_the_host_opens_windows() {
        assert!(is(
            dispatched(
                bindings(true, Host::Desktop),
                "secondary-shift-n",
                &in_a_panel()
            ),
            DetachPanel
        ));
        assert!(
            dispatched(
                bindings(true, Host::Browser),
                "secondary-shift-n",
                &in_a_panel()
            )
            .is_none()
        );
    }

    #[test]
    fn a_note_told_again_stands_once_as_last_told() {
        let note = |id, text: &'static str| Note {
            id,
            says: Says::Text(text.into()),
        };
        let mut notes = Vec::new();
        add_note(&mut notes, note(0, "copied"));
        add_note(&mut notes, note(1, "cannot open a"));
        add_note(&mut notes, note(2, "copied"));
        let standing: Vec<_> = notes.iter().map(|note| note.id).collect();
        assert_eq!(standing, [1, 2]);
    }

    #[test]
    fn a_window_binds_its_open_files_key_only_where_the_app_opens_files() {
        assert!(is(
            dispatched(bindings(true, Host::Desktop), "secondary-o", &in_a_panel()),
            OpenFiles
        ));
        assert!(dispatched(bindings(false, Host::Desktop), "secondary-o", &in_a_panel()).is_none());
    }

    /// The path dialog is drawn outside the workspace's key context, so with
    /// focus in its field the window's keys act on nothing it stands over.
    #[test]
    fn the_window_keys_reach_nothing_through_the_path_dialog() {
        let in_the_dialog = [
            bviz_ui_components::dialog::KEY_CONTEXT.id(),
            bviz_ui_components::text_input::KEY_CONTEXT.id(),
        ];
        assert!(is(
            dispatched(bindings(true, Host::Desktop), "secondary-w", &in_a_panel()),
            ClosePanel
        ));
        for host in [Host::Desktop, Host::Browser] {
            for keys in [
                "secondary-w",
                "secondary-alt-w",
                "secondary-o",
                "ctrl-tab",
                "secondary-shift-n",
                "secondary-k w",
                "secondary-k tab",
            ] {
                let mut window_keys = bindings(true, host);
                window_keys.extend(bviz_ui_shell::bindings(host));
                assert!(
                    dispatched(window_keys, keys, &in_the_dialog).is_none(),
                    "{host:?} {keys}"
                );
            }
        }
    }

    #[test]
    fn a_browser_closes_and_cycles_panels_by_chords() {
        let browser = || bindings(true, Host::Browser);
        assert!(dispatched(browser(), "secondary-w", &in_a_panel()).is_none());
        assert!(is(
            dispatched(browser(), "secondary-k w", &in_a_panel()),
            ClosePanel
        ));
        assert!(is(
            dispatched(browser(), "secondary-k tab", &in_a_panel()),
            NextTab
        ));
        assert!(is(
            dispatched(browser(), "secondary-k shift-tab", &in_a_panel()),
            PreviousTab
        ));
        let desktop = || bindings(true, Host::Desktop);
        assert!(dispatched(desktop(), "secondary-k w", &in_a_panel()).is_none());
        assert!(dispatched(desktop(), "secondary-k tab", &in_a_panel()).is_none());
    }

    /// The keys help of a panel the last frame's path to focus left out still
    /// lists the keys the window's frame binds.
    #[test]
    fn the_stack_around_a_panel_reaches_the_window_frames_keys() {
        const PANEL: KeyScope = KeyScope::new("Panel", "Panel");
        let keymap = gpui::Keymap::new(bviz_ui_shell::bindings(Host::Desktop));
        let groups = keys_in_effect(
            &keymap,
            &[KEY_CONTEXT, bviz_ui_shell::KEY_CONTEXT, PANEL],
            &stack_around(KEY_CONTEXT.into(), PANEL),
            PANEL,
            |action| action.name().into(),
            KeyStyle::Words,
        );
        let listed: Vec<SharedString> = groups
            .into_iter()
            .flat_map(|group| group.rows)
            .map(|row| row.label)
            .collect();
        for action in [&bviz_ui_shell::Tab as &dyn Action, &bviz_ui_shell::TabPrev] {
            assert!(
                listed.iter().any(|label| label == action.name()),
                "{listed:?}"
            );
        }
    }
}
