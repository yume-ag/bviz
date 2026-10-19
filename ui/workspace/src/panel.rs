use crate::{DocumentId, Documents, Pane, Regions, Role, ViewSpec};
use bviz_invocation::{InvocationId, Session};
use bviz_ui_components::KeyScope;
use bviz_ui_components::gesture::GestureRow;
use bviz_ui_playback::Clock;
use gpui::{
    AnyView, App, AppContext, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Subscription, Window,
};
use std::iter;
use std::num::NonZeroUsize;
use std::path::Path;
use std::rc::Rc;

/// How a path reads as a document's label: its file name, or the whole path
/// when it has none.
pub fn document_label(path: &Path) -> SharedString {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
        .into()
}

/// Which of a document's views a panel shows — its timeline, its flame graph,
/// its stats table. Distinct across panel types, so a view of an open document
/// can be named whether or not a panel showing it exists.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ViewKind(&'static str);

impl ViewKind {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// Stable identity of a panel type.
#[derive(Clone, Copy)]
pub struct PanelKind {
    pub view: ViewKind,
    /// The gpui key context this panel's own key bindings are scoped to; unique
    /// across kinds, so such a binding reaches only this panel.
    pub key_context: KeyScope,
    pub role: Role,
}

impl PanelKind {
    /// The name the view goes by among the views its document can show.
    pub fn label(&self) -> &'static str {
        self.key_context.label()
    }
}

/// A view a [`Workspace`] can host. It renders its own body; the workspace draws
/// the window chrome around it from the title, description, status and icon
/// asked for here.
pub trait Panel: Render + Focusable + EventEmitter<PanelEvent> {
    fn kind() -> PanelKind
    where
        Self: Sized;

    fn titled() -> Titled
    where
        Self: Sized,
    {
        Titled::View
    }

    /// A line saying what panels of this kind show, for a reader the title
    /// alone leaves guessing.
    fn description() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// The pointer gestures panels of this kind answer to, in the order their
    /// table tries them.
    fn gestures() -> Vec<GestureRow>
    where
        Self: Sized,
    {
        Vec::new()
    }

    fn status(&self, _cx: &App) -> Option<SharedString> {
        None
    }

    fn icon(&self) -> Option<&'static str> {
        None
    }

    /// Called when the workspace starts or stops showing this panel. Focus is
    /// taken here rather than by the panel's constructor, so building a panel
    /// the workspace does not go on to show leaves focus where it was.
    fn set_active(&mut self, _active: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
}

/// What a panel's title names first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Titled {
    /// The file, for a view that is the file's own rendering.
    File,
    /// The view, then the file it shows: one of several views a file offers.
    View,
}

const TITLE_SEPARATOR: &str = " \u{b7} ";

/// The title of a panel showing view `view` of `file`.
#[derive(Clone)]
struct PanelTitle {
    /// The title without the number of the file.
    plain: SharedString,
    ordinal: Option<NonZeroUsize>,
}

impl PanelTitle {
    fn new(titled: Titled, view: &str, file: &FileName) -> Self {
        let plain = match titled {
            Titled::File => file.label.clone(),
            Titled::View => format!("{view}{TITLE_SEPARATOR}{}", file.label).into(),
        };
        Self {
            plain,
            ordinal: file.ordinal,
        }
    }

    fn suffix(&self) -> Option<SharedString> {
        self.ordinal.map(ordinal_suffix)
    }

    fn display(&self) -> SharedString {
        numbered(self.plain.clone(), self.ordinal)
    }
}

/// The file a panel shows a view of, as its panels name it.
#[derive(Clone)]
pub(crate) struct FileName {
    /// The file's label, without its number.
    pub(crate) label: SharedString,
    pub(crate) ordinal: Option<NonZeroUsize>,
}

impl FileName {
    pub(crate) fn numbered(&self) -> SharedString {
        numbered(self.label.clone(), self.ordinal)
    }
}

/// What a [`Panel`] tells its workspace about itself.
pub enum PanelEvent {
    StatusChanged,
    Close,
    /// Asks to be brought forward in the pane holding it, and that pane made
    /// the active one, for a gesture elsewhere that names what this panel
    /// shows.
    Activate,
}

/// A panel of any kind, holding it through operations resolved once for its
/// concrete type. Cloning shares the one panel. Its title and number are fixed
/// for its lifetime, so a panel keeps them when it moves to another pane or
/// another window.
#[derive(Clone)]
pub struct AnyPanel {
    panel: Rc<dyn PanelOps>,
    /// The document the panel shows a view of, wherever the panel moves.
    document: DocumentId,
    title: PanelTitle,
}

impl AnyPanel {
    pub(crate) fn new<T: Panel>(panel: Entity<T>, document: DocumentId, file: &FileName) -> Self {
        Self {
            panel: Rc::new(panel),
            document,
            title: PanelTitle::new(T::titled(), T::kind().label(), file),
        }
    }

    pub(crate) fn document(&self) -> DocumentId {
        self.document
    }

    pub fn view_kind(&self) -> ViewKind {
        self.panel.view_kind()
    }

    pub fn role(&self) -> Role {
        self.panel.role()
    }

    /// The title without the number of the file.
    pub fn plain_title(&self) -> SharedString {
        self.title.plain.clone()
    }

    /// The number following the plain title, if the file the panel shows has one.
    pub fn title_suffix(&self) -> Option<SharedString> {
        self.title.suffix()
    }

    /// The plain title followed by its suffix, for a place that shows the title
    /// as one string.
    pub fn display_title(&self) -> SharedString {
        self.title.display()
    }

    pub fn description(&self) -> Option<&'static str> {
        self.panel.description()
    }

    pub fn gestures(&self) -> Vec<GestureRow> {
        self.panel.gestures()
    }

    pub fn key_scope(&self) -> KeyScope {
        self.panel.key_scope()
    }

    pub fn status(&self, cx: &App) -> Option<SharedString> {
        self.panel.status(cx)
    }

    pub fn icon(&self, cx: &App) -> Option<&'static str> {
        self.panel.icon(cx)
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }

    pub fn set_active(&self, active: bool, window: &mut Window, cx: &mut App) {
        self.panel.set_active(active, window, cx)
    }

    pub fn entity_id(&self) -> EntityId {
        self.panel.view().entity_id()
    }

    /// Calls `released` once this panel is dropped, which is the last pane
    /// holding it letting go: a pane is a panel's only owner.
    pub fn on_release(&self, cx: &App, released: impl FnOnce(&mut App) + 'static) -> Subscription {
        self.panel.on_release(cx, Box::new(released))
    }

    /// Marks the panel for a repaint of its own, which its pane owes it after
    /// frames that left it out.
    pub fn notify(&self, cx: &mut App) {
        self.panel.notify(cx)
    }

    pub fn view(&self) -> AnyView {
        self.panel.view()
    }

    /// Forward this panel's [`PanelEvent`]s to the pane `cx` belongs to.
    pub fn subscribe(&self, window: &Window, cx: &mut Context<Pane>) -> Subscription {
        self.panel.subscribe(window, cx)
    }
}

/// The focus a control acting on `panel` looks up the keys it names in,
/// `active` being the panel the window's keys act on, if any: that panel's
/// focus, wherever focus stands in the window, where it is `panel`; `None`
/// otherwise, as those keys would act on a panel other than `panel`.
pub(crate) fn keys_focus(
    panel: EntityId,
    active: Option<&AnyPanel>,
    cx: &App,
) -> Option<FocusHandle> {
    active
        .filter(|active| active.entity_id() == panel)
        .map(|active| active.focus_handle(cx))
}

/// The views one file brings to a window, at least one, and the label its
/// panels are titled by.
pub struct Member {
    pub(crate) label: SharedString,
    pub(crate) first: ViewSpec,
    pub(crate) rest: Vec<ViewSpec>,
    /// Whether the file has time to play back by itself, so a document it
    /// makes up alone plays back.
    pub(crate) plays_alone: bool,
}

impl Member {
    pub fn new(label: SharedString, first: ViewSpec, rest: Vec<ViewSpec>) -> Self {
        Self {
            label,
            first,
            rest,
            plays_alone: false,
        }
    }

    /// This file, as one with time to play back by itself.
    pub fn playing_alone(self) -> Self {
        Self {
            plays_alone: true,
            ..self
        }
    }
}

/// What the files of one document share: the session their readers report to
/// and the clock whose time every view of the document shows.
#[derive(Clone)]
pub struct Shared {
    session: Entity<Session>,
    clock: Entity<Clock>,
}

impl Shared {
    /// A fresh session and clock for a document's files. The registry joins
    /// the clock of a document opening with several files.
    pub fn new(cx: &mut App) -> Self {
        Self {
            session: cx.new(|_| Session::default()),
            clock: cx.new(|_| Clock::default()),
        }
    }

    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    pub fn clock(&self) -> &Entity<Clock> {
        &self.clock
    }
}

/// What a window opens, and lists as one entry of its Documents menu.
pub type Document = DocumentOf<Shared>;

/// A [`Document`] whose files share `S`, which the registry's bookkeeping is
/// tested over without a running application.
pub struct DocumentOf<S> {
    pub(crate) members: Members,
    pub(crate) shared: S,
}

pub(crate) enum Members {
    /// One file opened by itself, which the document goes by the label of, and
    /// the invocation that wrote it where the file names one.
    Alone {
        member: Member,
        invocation: Option<InvocationId>,
    },
    /// Several files of the invocation `invocation`, which the document goes
    /// by the label of.
    Joined {
        invocation: InvocationId,
        first: Member,
        second: Member,
        rest: Vec<Member>,
    },
}

impl<S> DocumentOf<S> {
    /// `member` as a document by itself, of `invocation` where the file names
    /// the one that wrote it.
    pub fn alone(member: Member, invocation: Option<InvocationId>, shared: S) -> Self {
        Self {
            members: Members::Alone { member, invocation },
            shared,
        }
    }

    /// `first`, `second` and each of `rest` as one document of `invocation`,
    /// which the Documents menu lists under the invocation's label. A window
    /// lays out and numbers each file as it does one opened by itself.
    pub fn join(
        invocation: InvocationId,
        first: Member,
        second: Member,
        rest: Vec<Member>,
        shared: S,
    ) -> Self {
        Self {
            members: Members::Joined {
                invocation,
                first,
                second,
                rest,
            },
            shared,
        }
    }
}

/// Characters of a UUID's first group, which is as much of an invocation id as
/// a label shows.
const SHORT_ID_CHARS: usize = 8;

/// How the invocation `id` reads as its document's label: by the first group
/// of the UUID Bazel names an invocation with, and by as many leading
/// characters of an id that is not one.
pub fn invocation_label(id: &InvocationId) -> SharedString {
    let id = id.as_str();
    let group = id.split('-').find(|group| !group.is_empty()).unwrap_or(id);
    let short: String = group.chars().take(SHORT_ID_CHARS).collect();
    format!("Invocation {short}").into()
}

impl Document {
    /// The panels of `first` and `others`, each document registered with the
    /// session's [`Documents`], gathered by role in the order the documents,
    /// their files and the files' views are given.
    pub(crate) fn open(first: Self, others: Vec<Self>, cx: &mut App) -> Regions<AnyPanel> {
        let by_role = |panel: AnyPanel| (panel.role(), panel);
        Documents::global(cx).update(cx, |documents, cx| {
            let (lead, following) = documents.open(first, cx);
            let mut regions = Regions::new(lead.role(), lead);
            regions.extend(following.into_iter().map(by_role));
            for document in others {
                let (first, rest) = documents.open(document, cx);
                regions.extend(iter::once(first).chain(rest).map(by_role));
            }
            regions
        })
    }
}

fn ordinal_suffix(ordinal: NonZeroUsize) -> SharedString {
    format!(" ({ordinal})").into()
}

/// `name` followed by the suffix `ordinal` reads as, if there is one.
pub(crate) fn numbered(name: SharedString, ordinal: Option<NonZeroUsize>) -> SharedString {
    match ordinal {
        Some(ordinal) => format!("{name}{}", ordinal_suffix(ordinal)).into(),
        None => name,
    }
}

/// [`Panel`] with its type parameter discharged, so a workspace can hold panels
/// of differing kinds. Implemented once, for the entity holding the panel.
trait PanelOps {
    fn view_kind(&self) -> ViewKind;
    fn role(&self) -> Role;
    fn description(&self) -> Option<&'static str>;
    fn gestures(&self) -> Vec<GestureRow>;
    fn key_scope(&self) -> KeyScope;
    fn status(&self, cx: &App) -> Option<SharedString>;
    fn icon(&self, cx: &App) -> Option<&'static str>;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App);
    fn notify(&self, cx: &mut App);
    fn on_release(&self, cx: &App, released: Box<dyn FnOnce(&mut App)>) -> Subscription;
    fn view(&self) -> AnyView;
    fn subscribe(&self, window: &Window, cx: &mut Context<Pane>) -> Subscription;
}

impl<T: Panel> PanelOps for Entity<T> {
    fn view_kind(&self) -> ViewKind {
        T::kind().view
    }

    fn role(&self) -> Role {
        T::kind().role
    }

    fn description(&self) -> Option<&'static str> {
        T::description()
    }

    fn gestures(&self) -> Vec<GestureRow> {
        T::gestures()
    }

    fn key_scope(&self) -> KeyScope {
        T::kind().key_context
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).status(cx)
    }

    fn icon(&self, cx: &App) -> Option<&'static str> {
        self.read(cx).icon()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |panel, cx| panel.set_active(active, window, cx));
    }

    fn notify(&self, cx: &mut App) {
        self.update(cx, |_, cx| cx.notify());
    }

    fn on_release(&self, cx: &App, released: Box<dyn FnOnce(&mut App)>) -> Subscription {
        cx.observe_release(self, move |_, cx| released(cx))
    }

    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn subscribe(&self, window: &Window, cx: &mut Context<Pane>) -> Subscription {
        cx.subscribe_in(self, window, |pane, panel, event, window, cx| {
            pane.on_panel_event(panel.entity_id(), event, window, cx);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_labelled_by_its_file_name() {
        assert_eq!(
            document_label(Path::new("/tmp/build.profile.gz")),
            "build.profile.gz"
        );
        assert_eq!(document_label(Path::new("aquery.pb")), "aquery.pb");
    }

    /// A path ending in `..` or in the root has no file name, and reads whole.
    #[test]
    fn a_path_with_no_file_name_is_its_own_label() {
        assert_eq!(document_label(Path::new("/tmp/..")), "/tmp/..");
        assert_eq!(document_label(Path::new("/")), "/");
    }

    fn build_profile(ordinal: Option<NonZeroUsize>) -> FileName {
        FileName {
            label: "build.profile".into(),
            ordinal,
        }
    }

    #[test]
    fn a_file_titled_panel_reads_as_its_file() {
        let title = PanelTitle::new(Titled::File, "Timeline", &build_profile(None));
        assert_eq!(title.display(), "build.profile");
    }

    #[test]
    fn a_view_titled_panel_reads_as_its_view_of_the_file() {
        let title = PanelTitle::new(Titled::View, "Flame Graph", &build_profile(None));
        assert_eq!(title.display(), "Flame Graph \u{b7} build.profile");
    }

    /// The number tells files apart, so it follows the file's label.
    #[test]
    fn a_numbered_file_numbers_the_title_last() {
        let title = PanelTitle::new(
            Titled::View,
            "Flame Graph",
            &build_profile(NonZeroUsize::new(2)),
        );
        assert_eq!(title.plain, "Flame Graph \u{b7} build.profile");
        assert_eq!(title.suffix(), Some(" (2)".into()));
        assert_eq!(title.display(), "Flame Graph \u{b7} build.profile (2)");
    }

    #[test]
    fn an_ordinal_reads_as_a_parenthesized_suffix() {
        assert_eq!(ordinal_suffix(NonZeroUsize::MIN), " (1)");
        assert_eq!(
            NonZeroUsize::new(12).map(ordinal_suffix),
            Some(" (12)".into())
        );
    }

    fn id(id: &str) -> InvocationId {
        InvocationId::new(id).expect("a non-empty id")
    }

    #[test]
    fn an_invocation_is_labelled_by_the_first_group_of_its_uuid() {
        assert_eq!(
            invocation_label(&id("a5f7f3e6-4fda-4675-bc16-8ede5420b210")),
            "Invocation a5f7f3e6"
        );
    }

    /// An id is whatever the file holds, so one that is no UUID is still cut to
    /// a label's length, and one led by a hyphen still shows a group.
    #[test]
    fn an_id_that_is_no_uuid_is_labelled_by_its_leading_characters() {
        assert_eq!(invocation_label(&id("abc")), "Invocation abc");
        assert_eq!(
            invocation_label(&id("0123456789abcdef0123456789abcdef")),
            "Invocation 01234567"
        );
        assert_eq!(invocation_label(&id("-4fda-4675")), "Invocation 4fda");
        assert_eq!(invocation_label(&id("--")), "Invocation --");
    }
}
