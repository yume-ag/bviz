#[cfg(test)]
use crate::OpenDocument;
use crate::documents::{Held, Registration};
use crate::{DocumentId, Documents, Pane, Regions, Role, ViewSpec};
use bviz_ui_components::KeyScope;
use bviz_ui_components::gesture::GestureRow;
use bviz_ui_playback::Clock;
use gpui::{
    AnyView, App, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Window,
};
use std::collections::HashMap;
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

/// The views one file brings to a window, at least one. Its label tells it
/// apart from the other files the window opens with, whichever documents hold
/// them: every panel of a file whose label another shares is numbered, counting
/// in the order the files are given.
pub struct Member {
    label: SharedString,
    first: ViewSpec,
    rest: Vec<ViewSpec>,
}

impl Member {
    pub fn new(label: SharedString, first: ViewSpec, rest: Vec<ViewSpec>) -> Self {
        Self { label, first, rest }
    }
}

/// What a window opens, and lists as one entry of its Documents menu.
pub struct Document {
    members: Members,
    /// The time every view of the document shows, for a document that can play
    /// back.
    clock: Option<Entity<Clock>>,
}

enum Members {
    /// One file opened by itself, which the document goes by the label of.
    Alone(Member),
    /// Several files under a label of the document's own.
    Joined {
        label: SharedString,
        first: Member,
        second: Member,
        rest: Vec<Member>,
    },
}

impl From<Member> for Document {
    fn from(member: Member) -> Self {
        Self {
            members: Members::Alone(member),
            clock: None,
        }
    }
}

impl Document {
    /// `first`, `second` and each of `rest` as one document, which the Documents
    /// menu lists under `label`. A window lays out and numbers each file as it
    /// does one opened by itself.
    pub fn join(label: SharedString, first: Member, second: Member, rest: Vec<Member>) -> Self {
        Self {
            members: Members::Joined {
                label,
                first,
                second,
                rest,
            },
            clock: None,
        }
    }

    /// This document, its files following `clock`, which the window drives
    /// from a bar while one of the document's panels is the active one.
    pub fn playing_on(self, clock: Entity<Clock>) -> Self {
        Self {
            clock: Some(clock),
            ..self
        }
    }

    fn members(&self) -> impl Iterator<Item = &Member> {
        let (first, second, rest) = match &self.members {
            Members::Alone(member) => (member, None, &[][..]),
            Members::Joined {
                first,
                second,
                rest,
                ..
            } => (first, Some(second), &rest[..]),
        };
        iter::once(first).chain(second).chain(rest)
    }

    fn joined_label(&self) -> Option<&SharedString> {
        match &self.members {
            Members::Alone(_) => None,
            Members::Joined { label, .. } => Some(label),
        }
    }

    /// `first` and `others` as the registry takes them. Files whose labels
    /// collide are numbered, and so are the joined documents whose own labels
    /// do. A file opened by itself is listed under its numbered label, which is
    /// how its panels' titles name it.
    fn registrations(first: Self, others: Vec<Self>) -> (Registration, Vec<Registration>) {
        let documents = || iter::once(&first).chain(&others);
        let mut files = Ordinals::of(
            documents()
                .flat_map(Self::members)
                .map(|member| &member.label),
        );
        let mut joined = Ordinals::of(documents().filter_map(Self::joined_label));
        let mut held = |member: Member| Held {
            file: FileName {
                ordinal: files.take(&member.label),
                label: member.label,
            },
            first: member.first,
            rest: member.rest,
        };
        let mut registration = |document: Self| match document.members {
            Members::Alone(member) => {
                let first = held(member);
                Registration {
                    name: first.file.numbered(),
                    first,
                    rest: Vec::new(),
                    clock: document.clock,
                }
            }
            Members::Joined {
                label,
                first,
                second,
                rest,
            } => {
                let ordinal = joined.take(&label);
                Registration {
                    name: numbered(label, ordinal),
                    first: held(first),
                    rest: iter::once(second).chain(rest).map(&mut held).collect(),
                    clock: document.clock,
                }
            }
        };
        let first = registration(first);
        (first, others.into_iter().map(registration).collect())
    }

    /// What a window opening with `first` and `others` lists before any panel
    /// shows a view.
    #[cfg(test)]
    pub(crate) fn unshown(first: Self, others: Vec<Self>) -> Vec<OpenDocument> {
        let (first, others) = Self::registrations(first, others);
        iter::once(first)
            .chain(others)
            .map(OpenDocument::unshown)
            .collect()
    }

    /// The panels of `first` and `others`, each document registered with the
    /// session's [`Documents`], gathered by role in the order the documents,
    /// their files and the files' views are given.
    pub(crate) fn open(first: Self, others: Vec<Self>, cx: &mut App) -> Regions<AnyPanel> {
        let (first, others) = Self::registrations(first, others);
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

/// The number that tells each of several equal labels apart, counted in the
/// order the labels are taken.
struct Ordinals {
    /// By label more than one bears, the number its next bearer takes.
    next: HashMap<SharedString, NonZeroUsize>,
}

impl Ordinals {
    fn of<'a>(labels: impl Iterator<Item = &'a SharedString>) -> Self {
        let mut bearers: HashMap<&SharedString, usize> =
            HashMap::with_capacity(labels.size_hint().0);
        for label in labels {
            *bearers.entry(label).or_default() += 1;
        }
        let next = bearers
            .into_iter()
            .filter(|(_, bearers)| *bearers > 1)
            .map(|(label, _)| (label.clone(), NonZeroUsize::MIN))
            .collect();
        Self { next }
    }

    /// The number of the next bearer of `label`, or `None` for a label no other
    /// one equals.
    fn take(&mut self, label: &SharedString) -> Option<NonZeroUsize> {
        let next = self.next.get_mut(label)?;
        let taken = *next;
        *next = next.saturating_add(1);
        Some(taken)
    }
}

fn ordinal_suffix(ordinal: NonZeroUsize) -> SharedString {
    format!(" ({ordinal})").into()
}

/// `name` followed by the suffix `ordinal` reads as, if there is one.
fn numbered(name: SharedString, ordinal: Option<NonZeroUsize>) -> SharedString {
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

    const TIMELINE: ViewKind = ViewKind::new("timeline");
    const FLAME: ViewKind = ViewKind::new("flame");
    const CONSOLE: ViewKind = ViewKind::new("console");
    const GRAPH: ViewKind = ViewKind::new("graph");

    fn profile(label: &'static str) -> Member {
        Member::new(
            label.into(),
            ViewSpec::for_test(TIMELINE, Role::Timeline),
            vec![ViewSpec::for_test(FLAME, Role::Analysis)],
        )
    }

    fn one_view(label: &'static str, kind: ViewKind, role: Role) -> Member {
        Member::new(label.into(), ViewSpec::for_test(kind, role), Vec::new())
    }

    fn session(label: &'static str, profile_label: &'static str) -> Document {
        Document::join(
            label.into(),
            profile(profile_label),
            one_view("build.bep", CONSOLE, Role::Log),
            Vec::new(),
        )
    }

    /// A file as its panels are numbered, and the kinds of the views it shows.
    type File = (String, Vec<ViewKind>);

    /// Each document a window opening with `first` and `others` holds: what it
    /// is listed under, and its files.
    fn opened(first: Document, others: Vec<Document>) -> Vec<(String, Vec<File>)> {
        let (first, others) = Document::registrations(first, others);
        iter::once(first)
            .chain(others)
            .map(|registration| {
                let files = iter::once(&registration.first)
                    .chain(&registration.rest)
                    .map(|held| {
                        (
                            held.file.numbered().to_string(),
                            held.views().map(ViewSpec::kind).collect(),
                        )
                    })
                    .collect();
                (registration.name.to_string(), files)
            })
            .collect()
    }

    /// The same, without what each file shows.
    fn named(first: Document, others: Vec<Document>) -> Vec<(String, Vec<String>)> {
        opened(first, others)
            .into_iter()
            .map(|(name, files)| (name, files.into_iter().map(|file| file.0).collect()))
            .collect()
    }

    fn names(document: &str, files: &[&str]) -> (String, Vec<String>) {
        let files = files.iter().map(|file| file.to_string()).collect();
        (document.to_string(), files)
    }

    #[test]
    fn a_file_opened_by_itself_is_a_document_going_by_its_label() {
        assert_eq!(
            opened(profile("build.profile.gz").into(), Vec::new()),
            [(
                "build.profile.gz".to_string(),
                vec![("build.profile.gz".to_string(), vec![TIMELINE, FLAME])],
            )]
        );
    }

    /// A joined document holds each file as the file's viewer opened it: under
    /// its own label, with the views it asked for.
    #[test]
    fn a_joined_document_holds_every_file_as_it_was_opened() {
        let joined = Document::join(
            "Invocation a5f7f3e6".into(),
            profile("build.profile.gz"),
            one_view("build.bep", CONSOLE, Role::Log),
            vec![one_view("aquery.pb", GRAPH, Role::Canvas)],
        );
        assert_eq!(
            opened(joined, Vec::new()),
            [(
                "Invocation a5f7f3e6".to_string(),
                vec![
                    ("build.profile.gz".to_string(), vec![TIMELINE, FLAME]),
                    ("build.bep".to_string(), vec![CONSOLE]),
                    ("aquery.pb".to_string(), vec![GRAPH]),
                ],
            )]
        );
    }

    /// A panel is titled by its file, so files of one name are told apart by
    /// number in whichever documents they are held.
    #[test]
    fn files_sharing_a_label_are_numbered_across_joined_documents() {
        assert_eq!(
            named(
                session("Invocation a5f7f3e6", "build.profile.gz"),
                vec![session("Invocation 0b437916", "build.profile.gz")],
            ),
            [
                names(
                    "Invocation a5f7f3e6",
                    &["build.profile.gz (1)", "build.bep (1)"]
                ),
                names(
                    "Invocation 0b437916",
                    &["build.profile.gz (2)", "build.bep (2)"]
                ),
            ]
        );
    }

    /// A file opened by itself is listed under the number its panels carry.
    #[test]
    fn a_joined_file_and_one_opened_by_itself_are_numbered_together() {
        assert_eq!(
            named(
                session("Invocation a5f7f3e6", "build.profile.gz"),
                vec![
                    profile("build.profile.gz").into(),
                    one_view("aquery.pb", GRAPH, Role::Canvas).into()
                ],
            ),
            [
                names(
                    "Invocation a5f7f3e6",
                    &["build.profile.gz (1)", "build.bep"]
                ),
                names("build.profile.gz (2)", &["build.profile.gz (2)"]),
                names("aquery.pb", &["aquery.pb"]),
            ]
        );
    }

    /// The labels joined documents are listed under are numbered among
    /// themselves: a file's label equal to one names something else.
    #[test]
    fn joined_documents_sharing_a_label_are_numbered_among_themselves() {
        assert_eq!(
            named(
                session("Invocation a5f7f3e6", "a.profile.gz"),
                vec![
                    one_view("Invocation a5f7f3e6", GRAPH, Role::Canvas).into(),
                    session("Invocation a5f7f3e6", "b.profile.gz"),
                    session("Invocation 0b437916", "c.profile.gz"),
                ],
            ),
            [
                names(
                    "Invocation a5f7f3e6 (1)",
                    &["a.profile.gz", "build.bep (1)"]
                ),
                names("Invocation a5f7f3e6", &["Invocation a5f7f3e6"]),
                names(
                    "Invocation a5f7f3e6 (2)",
                    &["b.profile.gz", "build.bep (2)"]
                ),
                names("Invocation 0b437916", &["c.profile.gz", "build.bep (3)"]),
            ]
        );
    }

    fn ordinals(labels: &[&'static str]) -> Vec<Option<usize>> {
        let labels: Vec<SharedString> = labels.iter().copied().map(SharedString::from).collect();
        let mut ordinals = Ordinals::of(labels.iter());
        labels
            .iter()
            .map(|label| ordinals.take(label).map(NonZeroUsize::get))
            .collect()
    }

    #[test]
    fn distinct_labels_are_not_numbered() {
        assert_eq!(ordinals(&[]), []);
        assert_eq!(ordinals(&["a.bep", "b.bep"]), [None, None]);
    }

    #[test]
    fn a_shared_label_numbers_every_document_bearing_it_from_one() {
        assert_eq!(ordinals(&["x", "x"]), [Some(1), Some(2)]);
        assert_eq!(
            ordinals(&["x", "y", "x", "x"]),
            [Some(1), None, Some(2), Some(3)]
        );
    }

    #[test]
    fn each_shared_label_counts_on_its_own() {
        assert_eq!(
            ordinals(&["x", "y", "y", "x", "z"]),
            [Some(1), Some(1), Some(2), Some(2), None]
        );
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
}
