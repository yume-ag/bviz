use crate::panel::{FileName, Members, numbered};
use crate::{
    AnyPanel, Document, DocumentOf, Member, Panel, Regions, Role, Shared, ViewKind, Workspace,
    invocation_label,
};
use bviz_invocation::InvocationId;
use bviz_ui_playback::Clock;
use gpui::{
    App, AppContext, Context, Entity, EntityId, EventEmitter, Global, SharedString, Subscription,
    WeakEntity, Window,
};
use std::collections::HashMap;
use std::iter;
use std::num::NonZeroUsize;
use std::rc::Rc;

/// Identity of a document the session holds open. Two readings of one file are
/// two documents, each with views of its own.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DocumentId(u64);

impl DocumentId {
    /// An identity for a test that stands in for the registry minting them.
    #[cfg(test)]
    pub(crate) const fn for_test(id: u64) -> Self {
        Self(id)
    }
}

/// Builds a panel showing one view of a document, the panel carrying the
/// document's identity and the name of its file. Takes no window, so a view
/// can be minted wherever the reader asks for it.
type Mint = Rc<dyn Fn(DocumentId, &FileName, &mut App) -> AnyPanel>;

/// One view a document can show, and how to build a panel showing it.
pub struct ViewSpec {
    kind: ViewKind,
    role: Role,
    label: &'static str,
    mint: Mint,
}

impl ViewSpec {
    /// The view panels of type `T` show, `mint` building one over whatever the
    /// document holds its content in.
    pub fn new<T: Panel>(mint: impl Fn(&mut App) -> Entity<T> + 'static) -> Self {
        let kind = T::kind();
        Self {
            kind: kind.view,
            role: kind.role,
            label: kind.label(),
            mint: Rc::new(move |document, file, cx| AnyPanel::new(mint(cx), document, file)),
        }
    }

    #[cfg(test)]
    pub(crate) fn kind(&self) -> ViewKind {
        self.kind
    }

    /// A view of `kind` that mints no panel: minting one takes a running
    /// application, which the bookkeeping a test exercises stands apart from.
    #[cfg(test)]
    pub(crate) fn for_test(kind: ViewKind, role: Role) -> Self {
        Self {
            kind,
            role,
            label: kind.as_str(),
            mint: Rc::new(move |_, _, _| panic!("{} mints no panel here", kind.as_str())),
        }
    }

    /// The registry's record of this view, which no panel shows yet.
    fn registered(&self) -> View {
        View {
            kind: self.kind,
            role: self.role,
            label: self.label,
            mint: self.mint.clone(),
            live: None,
        }
    }
}

/// What a document's view has on screen. A pane draws only the panel it shows,
/// so a view can have a panel that no frame draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewState {
    /// No panel shows this view.
    Closed,
    /// A pane holds a panel showing this view, but no frame draws it: the pane
    /// shows another of its tabs, or the window draws another pane in place of
    /// the one holding it.
    Open,
    /// The pane holding this view's panel shows it.
    Visible,
}

/// A document of the session, read out of the registry for a reader choosing
/// among what is open.
pub struct OpenDocument {
    pub id: DocumentId,
    /// How the document reads, number included.
    pub label: SharedString,
    pub views: Vec<OpenView>,
}

#[cfg(test)]
impl OpenDocument {
    /// `documents` as a registry opening them in their order lists them while
    /// no panel shows a view of any.
    pub(crate) fn unshown<S>(documents: impl IntoIterator<Item = DocumentOf<S>>) -> Vec<Self> {
        let mut registry = Registry::new();
        let mut listed = Vec::new();
        for document in documents {
            let document = registry.numbered(document);
            let registered = Registered {
                id: DocumentId::for_test(0),
                name: document.name,
                first: document.first.registered(),
                rest: document.rest.iter().map(Held::registered).collect(),
                shared: document.shared,
            };
            listed.push(registered.listed(&mut |_| ViewState::Closed));
        }
        listed
    }
}

/// Which of a document's views: the file bringing it, by its place among the
/// document's files, and its kind. Files of one document may show views of a
/// kind in common.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewId {
    member: usize,
    kind: ViewKind,
}

impl ViewId {
    #[cfg(test)]
    pub(crate) const fn for_test(member: usize, kind: ViewKind) -> Self {
        Self { member, kind }
    }
}

pub struct OpenView {
    pub id: ViewId,
    /// How this view reads where it is listed among the ones its document can
    /// show.
    pub label: &'static str,
    pub state: ViewState,
}

/// The documents a session holds open and the views each can show, so a view
/// that closed with its panel can be shown again over the document it was
/// showing. One registry serves the whole application: a document outlives the
/// panels showing it, and its views may be asked for from any window. Each
/// document keeps what its files share for as long as it is open.
pub type Documents = Registry<Shared>;

/// [`Documents`] keeping `S` for what each document's files share, which its
/// bookkeeping is tested over without a running application.
pub struct Registry<S> {
    next: u64,
    documents: Vec<Registered<S>>,
    /// The numbers of the labels of files.
    files: Numbering,
    /// The numbers of the labels joined documents are listed under, which
    /// count apart from those of files.
    joined: Numbering,
    /// The session's open windows, in the order they opened.
    windows: Vec<OpenWindow>,
}

/// A window the session holds open, named by the workspace framing it. A
/// workspace belongs to the window it is the root view of, so it is named
/// weakly here rather than held alive by the registry. The subscription drops
/// this record when that workspace is released, so no closed window is offered
/// as a place to show a view or to move a panel to; a window closing as it
/// empties drops it ahead of that.
struct OpenWindow {
    workspace: WeakEntity<Workspace>,
    _closed: Subscription,
}

/// An open document of an invocation, as a file of that invocation joining it
/// finds it.
pub struct Joinable {
    document: DocumentId,
    /// The invocation the document was found by.
    invocation: InvocationId,
    /// The kinds of the views its files show, in their order.
    views: Vec<ViewKind>,
}

impl Joinable {
    pub fn document(&self) -> DocumentId {
        self.document
    }

    pub fn invocation(&self) -> &InvocationId {
        &self.invocation
    }

    /// Whether a file of the document shows a view of `kind`.
    pub fn shows(&self, kind: ViewKind) -> bool {
        self.views.contains(&kind)
    }
}

/// That the session has closed the document named, whose panels have left
/// every window and whose views can be shown no more.
pub struct Closed(pub DocumentId);

impl EventEmitter<Closed> for Documents {}

/// That the session has opened or closed a window, so where the panels of the
/// windows still open can be moved to has changed.
pub(crate) struct WindowsChanged;

impl EventEmitter<WindowsChanged> for Documents {}

/// The numbers telling apart what bears one label: its first bearer goes
/// unnumbered, and each later one takes the number after the last one taken.
/// A label's count only rises, so no number is borne twice in a session, and
/// none changes once taken.
#[derive(Default)]
struct Numbering {
    /// By label borne so far, the number its latest bearer took, the unnumbered
    /// first counting as one.
    latest: HashMap<SharedString, NonZeroUsize>,
}

impl Numbering {
    /// The number of the next bearer of `label`, `None` for its first.
    fn take(&mut self, label: &SharedString) -> Option<NonZeroUsize> {
        match self.latest.get_mut(label) {
            Some(latest) => {
                *latest = latest.saturating_add(1);
                Some(*latest)
            }
            None => {
                self.latest.insert(label.clone(), NonZeroUsize::MIN);
                None
            }
        }
    }
}

/// A document as the registry opens it, its labels numbered.
pub(crate) struct Registration<S> {
    pub(crate) name: Name,
    pub(crate) first: Held<ViewSpec>,
    pub(crate) rest: Vec<Held<ViewSpec>>,
    pub(crate) shared: S,
}

/// What a document is listed under, and the invocation it is of.
pub(crate) enum Name {
    /// A file opened by itself, listed under its numbered label.
    File {
        invocation: Option<InvocationId>,
        /// Whether the file has time to play back by itself.
        plays: bool,
    },
    /// Files of the invocation `id`, listed under its label.
    Invocation {
        id: InvocationId,
        /// The invocation's label, its number included.
        label: SharedString,
    },
}

impl Name {
    /// What a document of this name whose first file is `first` is listed
    /// under.
    fn listed_as(&self, first: &FileName) -> SharedString {
        match self {
            Self::File { .. } => first.numbered(),
            Self::Invocation { label, .. } => label.clone(),
        }
    }

    fn invocation(&self) -> Option<&InvocationId> {
        match self {
            Self::File { invocation, .. } => invocation.as_ref(),
            Self::Invocation { id, .. } => Some(id),
        }
    }

    /// Whether the document plays back: every invocation's does, its files'
    /// times standing where the wall clock puts them, and a file's alone where
    /// it has time to play by itself.
    fn plays(&self) -> bool {
        match self {
            Self::File { plays, .. } => *plays,
            Self::Invocation { .. } => true,
        }
    }
}

/// The views of one file of a document, in the order its viewer gave them.
pub(crate) struct Held<V> {
    pub(crate) file: FileName,
    pub(crate) first: V,
    pub(crate) rest: Vec<V>,
}

impl<V> Held<V> {
    pub(crate) fn views(&self) -> impl Iterator<Item = &V> {
        iter::once(&self.first).chain(&self.rest)
    }

    fn views_mut(&mut self) -> impl Iterator<Item = &mut V> {
        iter::once(&mut self.first).chain(&mut self.rest)
    }

    fn view_count(&self) -> usize {
        1 + self.rest.len()
    }
}

impl Held<ViewSpec> {
    /// The registry's record of this file, which no panel shows yet.
    fn registered(&self) -> Held<View> {
        Held {
            file: self.file.clone(),
            first: self.first.registered(),
            rest: self.rest.iter().map(ViewSpec::registered).collect(),
        }
    }
}

impl Held<View> {
    /// A panel for each of the file's views, in their order.
    fn mint(&self, document: DocumentId, cx: &mut App) -> (AnyPanel, Vec<AnyPanel>) {
        let mut mint = |view: &View| (view.mint)(document, &self.file, cx);
        let first = mint(&self.first);
        (first, self.rest.iter().map(mint).collect())
    }
}

struct Registered<S> {
    id: DocumentId,
    name: Name,
    first: Held<View>,
    rest: Vec<Held<View>>,
    shared: S,
}

impl<S> Registered<S> {
    /// The document's files, in the order they joined it, which is the order
    /// a [`ViewId`] counts them in.
    fn members(&self) -> impl Iterator<Item = &Held<View>> {
        iter::once(&self.first).chain(&self.rest)
    }

    fn member(&self, at: usize) -> Option<&Held<View>> {
        match at.checked_sub(1) {
            None => Some(&self.first),
            Some(at) => self.rest.get(at),
        }
    }

    fn member_mut(&mut self, at: usize) -> Option<&mut Held<View>> {
        match at.checked_sub(1) {
            None => Some(&mut self.first),
            Some(at) => self.rest.get_mut(at),
        }
    }

    fn listed_as(&self) -> SharedString {
        self.name.listed_as(&self.first.file)
    }

    /// Appends `members` to the document's files, numbered by `files` after
    /// every file opened before them, and lists a document that was a file's
    /// alone under the label of `invocation`, numbered by `joined`, from here
    /// on. The place of the first file appended among the document's.
    fn append(
        &mut self,
        invocation: &InvocationId,
        members: impl IntoIterator<Item = Member>,
        files: &mut Numbering,
        joined: &mut Numbering,
    ) -> usize {
        let start = 1 + self.rest.len();
        let held = members
            .into_iter()
            .map(|member| numbered_file(files, member));
        self.rest.extend(held.map(|held| held.registered()));
        if let Name::File { .. } = self.name {
            let label = invocation_label(invocation);
            let ordinal = joined.take(&label);
            self.name = Name::Invocation {
                id: invocation.clone(),
                label: numbered(label, ordinal),
            };
        }
        start
    }

    /// The document as a reader choosing among what is open is given it,
    /// `state` saying what each view has on screen. The views are in the order
    /// the document's files give them.
    fn listed(&self, state: &mut impl FnMut(&View) -> ViewState) -> OpenDocument {
        let mut views = Vec::with_capacity(self.members().map(Held::view_count).sum());
        for (member, held) in self.members().enumerate() {
            views.extend(held.views().map(|view| OpenView {
                id: ViewId {
                    member,
                    kind: view.kind,
                },
                label: view.label,
                state: state(view),
            }));
        }
        OpenDocument {
            id: self.id,
            label: self.listed_as(),
            views,
        }
    }

    /// The view `id` names, and the file bringing it.
    fn view(&self, id: ViewId) -> Option<(&Held<View>, &View)> {
        let member = self.member(id.member)?;
        let view = member.views().find(|held| held.kind == id.kind)?;
        Some((member, view))
    }

    fn view_mut(&mut self, id: ViewId) -> Option<&mut View> {
        let member = self.member_mut(id.member)?;
        member.views_mut().find(|held| held.kind == id.kind)
    }
}

struct View {
    kind: ViewKind,
    role: Role,
    label: &'static str,
    mint: Mint,
    live: Option<Live>,
}

/// The panel showing a view, named by its identity alone: a pane is a panel's
/// only owner, so a handle held here would keep a closed view alive for the life
/// of the session. The subscription clears this record when that panel is
/// dropped, and is the one writer that clears it, so no close path can leave the
/// registry claiming a view that no pane holds.
struct Live {
    panel: EntityId,
    _released: Subscription,
}

struct GlobalDocuments(Entity<Documents>);

impl Global for GlobalDocuments {}

impl Documents {
    /// The session's registry, opened on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if cx.try_global::<GlobalDocuments>().is_none() {
            let documents = cx.new(|_| Self::new());
            cx.set_global(GlobalDocuments(documents));
        }
        cx.global::<GlobalDocuments>().0.clone()
    }
}

impl<S> Registry<S> {
    fn new() -> Self {
        Self {
            next: 0,
            documents: Vec::new(),
            files: Numbering::default(),
            joined: Numbering::default(),
            windows: Vec::new(),
        }
    }

    /// `document` with its labels numbered after those of every document
    /// opened before it. A file opened by itself is listed under its numbered
    /// label, which is how its panels' titles name it.
    fn numbered(&mut self, document: DocumentOf<S>) -> Registration<S> {
        let DocumentOf { members, shared } = document;
        let files = &mut self.files;
        let mut held = |member: Member| numbered_file(files, member);
        match members {
            Members::Alone { member, invocation } => {
                let plays = member.plays_alone;
                let first = held(member);
                Registration {
                    name: Name::File { invocation, plays },
                    first,
                    rest: Vec::new(),
                    shared,
                }
            }
            Members::Joined {
                invocation,
                first,
                second,
                rest,
            } => {
                let first = held(first);
                let mut others = Vec::with_capacity(1 + rest.len());
                others.extend(iter::once(second).chain(rest).map(held));
                let label = invocation_label(&invocation);
                let ordinal = self.joined.take(&label);
                Registration {
                    name: Name::Invocation {
                        id: invocation,
                        label: numbered(label, ordinal),
                    },
                    first,
                    rest: others,
                    shared,
                }
            }
        }
    }
}

impl Documents {
    /// Registers `document`, and mints a panel for each view of each of its
    /// files, in the order the files and their views are given. The registry
    /// keeps what it takes to mint them again. The clock of a document of
    /// several files is joined, its readers placing their times anew.
    pub(crate) fn open(
        &mut self,
        document: Document,
        cx: &mut Context<Self>,
    ) -> (AnyPanel, Vec<AnyPanel>) {
        let Registration {
            name,
            first,
            rest,
            shared,
        } = self.numbered(document);
        if let Name::Invocation { .. } = name {
            shared.clock().update(cx, |clock, cx| clock.join(cx));
        }
        let (id, (first, rest)) = self.register(name, shared, |id| {
            let mut minted = |member: Held<ViewSpec>| {
                let held = member.registered();
                let panels = held.mint(id, cx);
                (held, panels)
            };
            let (first, first_panels) = minted(first);
            let mut others = Vec::with_capacity(rest.len());
            let mut others_panels = Vec::with_capacity(rest.len());
            for member in rest {
                let (held, panels) = minted(member);
                others.push(held);
                others_panels.push(panels);
            }
            ((first, others), (first_panels, others_panels))
        });
        for (member, (first, more)) in iter::once(&first).chain(&rest).enumerate() {
            for panel in iter::once(first).chain(more) {
                let kind = panel.view_kind();
                self.watch(id, ViewId { member, kind }, panel, cx);
            }
        }
        cx.notify();
        let (lead, mut following) = first;
        following.reserve(rest.iter().map(|(_, more)| 1 + more.len()).sum());
        for (first, more) in rest {
            following.push(first);
            following.extend(more);
        }
        (lead, following)
    }

    /// Joins the files `open` makes to the document `to` names, sharing what
    /// that document's files share, and mints a panel for each of their views,
    /// in the order the files and their views are given. The document's clock
    /// is joined before `open` runs, so the readers it opens place their times
    /// on the clock of an invocation. `Err` hands `open` back unrun where the
    /// document has closed, for the caller to open the files otherwise.
    pub fn join<F>(
        &mut self,
        to: &Joinable,
        open: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Vec<AnyPanel>, F>
    where
        F: FnOnce(&Shared, &mut Window, &mut App) -> (Member, Vec<Member>),
    {
        let Some(at) = self.position(to.document) else {
            return Err(open);
        };
        let shared = self.documents[at].shared.clone();
        shared.clock().update(cx, |clock, cx| clock.join(cx));
        let (first, rest) = open(&shared, window, cx);
        let registered = &mut self.documents[at];
        let start = registered.append(
            &to.invocation,
            iter::once(first).chain(rest),
            &mut self.files,
            &mut self.joined,
        );
        let joined = || registered.members().enumerate().skip(start);
        let mut minted = Vec::with_capacity(joined().map(|(_, held)| held.view_count()).sum());
        for (member, held) in joined() {
            let (first, more) = held.mint(to.document, cx);
            minted.extend(iter::once(first).chain(more).map(|panel| (member, panel)));
        }
        for (member, panel) in &minted {
            let kind = panel.view_kind();
            let view = ViewId {
                member: *member,
                kind,
            };
            self.watch(to.document, view, panel, cx);
        }
        cx.notify();
        Ok(minted.into_iter().map(|(_, panel)| panel).collect())
    }

    /// Forgets `document`, which [`close`] calls for once no window holds a
    /// panel showing it.
    fn forget(&mut self, document: DocumentId, cx: &mut Context<Self>) {
        if self.remove(document).is_some() {
            cx.emit(Closed(document));
            cx.notify();
        }
    }

    /// The panel to show `view` of `document` with: the one already showing
    /// it, or a fresh one minted where none is open. A reopened view comes back
    /// as its document stands, not as its last panel had been scrolled or
    /// zoomed.
    fn showing(
        &mut self,
        document: DocumentId,
        view: ViewId,
        cx: &mut Context<Self>,
    ) -> Option<Showing> {
        let (member, found) = self.registered(document)?.view(view)?;
        if let Some(live) = &found.live {
            return Some(Showing::Open(live.panel));
        }
        let panel = (found.mint.clone())(document, &member.file, cx);
        self.watch(document, view, &panel, cx);
        cx.notify();
        Some(Showing::Minted(panel))
    }

    /// Every document the session holds, in the order they opened, each with
    /// what every view it can show has on screen.
    pub fn documents(&self, cx: &App) -> Vec<OpenDocument> {
        let workspaces = self.workspaces();
        let mut open = Vec::with_capacity(self.documents.len());
        for document in &self.documents {
            open.push(document.listed(&mut |view| state(view, &workspaces, cx)));
        }
        open
    }

    /// Takes `workspace` as a place a view can be shown and a panel can be
    /// moved to, for as long as the window it frames is open.
    pub(crate) fn watch_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let opened = workspace.entity_id();
        let closed = cx.observe_release(workspace, move |documents, _, cx| {
            documents
                .windows
                .retain(|held| held.workspace.entity_id() != opened);
            cx.emit(WindowsChanged);
        });
        self.windows.push(OpenWindow {
            workspace: workspace.downgrade(),
            _closed: closed,
        });
        cx.emit(WindowsChanged);
    }

    /// Lets go of the window `workspace` frames, as [`let_go_unless_last`]
    /// does, for a window closing as it empties; whether it let go of it.
    pub(crate) fn close_window_unless_last(
        &mut self,
        workspace: EntityId,
        cx: &mut Context<Self>,
    ) -> bool {
        let open = |held: &OpenWindow| {
            let id = held.workspace.entity_id();
            held.workspace.upgrade().map(|_| id)
        };
        let closed = let_go_unless_last(&mut self.windows, workspace, open);
        if closed {
            cx.emit(WindowsChanged);
        }
        closed
    }

    /// Whether the session holds a window open besides the one `workspace`
    /// frames.
    pub(crate) fn holds_another_window(&self, workspace: EntityId) -> bool {
        self.open_windows()
            .any(|held| held.entity_id() != workspace)
    }

    /// Records `panel` as the one showing `view` of `document`, watched so that
    /// its release closes the record.
    fn watch(
        &mut self,
        document: DocumentId,
        view: ViewId,
        panel: &AnyPanel,
        cx: &mut Context<Self>,
    ) {
        let documents = cx.weak_entity();
        let released = panel.on_release(cx, move |cx| {
            let _ = documents.update(cx, |documents, cx| {
                documents.closed(document, view);
                cx.notify();
            });
        });
        self.opened(document, view, panel.entity_id(), released);
    }

    /// The clock `document` plays back on and the name it is listed under,
    /// where it plays back.
    pub(crate) fn playback(&self, document: DocumentId) -> Option<(SharedString, Entity<Clock>)> {
        let registered = self.registered(document)?;
        registered
            .name
            .plays()
            .then(|| (registered.listed_as(), registered.shared.clock().clone()))
    }

    /// The workspaces framing the session's open windows, in the order their
    /// windows opened.
    pub(crate) fn workspaces(&self) -> Vec<Entity<Workspace>> {
        let mut open = Vec::with_capacity(self.windows.len());
        open.extend(self.open_windows());
        open
    }

    fn open_windows(&self) -> impl Iterator<Item = Entity<Workspace>> {
        self.windows
            .iter()
            .filter_map(|held| held.workspace.upgrade())
    }
}

impl<S> Registry<S> {
    /// Which panel shows `view` of `document`, if one does.
    #[cfg(test)]
    fn panel(&self, document: DocumentId, view: ViewId) -> Option<EntityId> {
        let (_, view) = self.registered(document)?.view(view)?;
        view.live.as_ref().map(|live| live.panel)
    }

    /// What `panel` makes of the panel showing each view of the session's
    /// documents, gathered by the view's role in the order the documents opened
    /// and gave their files and views. A view whose panel `panel` makes nothing
    /// of takes no place, and `None` where none takes one.
    pub(crate) fn opening_regions<T>(
        &self,
        panel: &mut impl FnMut(EntityId) -> Option<T>,
    ) -> Option<Regions<T>> {
        let views = self
            .documents
            .iter()
            .flat_map(Registered::members)
            .flat_map(Held::views);
        Regions::gather(views.filter_map(|view| {
            let shown = panel(view.live.as_ref()?.panel)?;
            Some((view.role, shown))
        }))
    }

    /// Records `panel` as the one showing `view` of `document`, until
    /// `released` reports it dropped.
    fn opened(
        &mut self,
        document: DocumentId,
        view: ViewId,
        panel: EntityId,
        released: Subscription,
    ) {
        if let Some(found) = self.view_mut(document, view) {
            found.live = Some(Live {
                panel,
                _released: released,
            });
        }
    }

    /// Forgets the panel that was showing `view` of `document`.
    fn closed(&mut self, document: DocumentId, view: ViewId) {
        if let Some(found) = self.view_mut(document, view) {
            found.live = None;
        }
    }

    /// Lists a document under `name`, its first file and the others as `held`
    /// makes them knowing the document's id, with what else `held` gives.
    fn register<T>(
        &mut self,
        name: Name,
        shared: S,
        held: impl FnOnce(DocumentId) -> ((Held<View>, Vec<Held<View>>), T),
    ) -> (DocumentId, T) {
        let id = DocumentId(self.next);
        self.next += 1;
        let ((first, rest), given) = held(id);
        self.documents.push(Registered {
            id,
            name,
            first,
            rest,
            shared,
        });
        (id, given)
    }

    /// What `document` is listed under where it is an invocation's, a name no
    /// title of its panels gives.
    pub(crate) fn joined_name(&self, document: DocumentId) -> Option<SharedString> {
        match &self.registered(document)?.name {
            Name::File { .. } => None,
            Name::Invocation { label, .. } => Some(label.clone()),
        }
    }

    fn registered(&self, document: DocumentId) -> Option<&Registered<S>> {
        self.documents.iter().find(|held| held.id == document)
    }

    fn registered_mut(&mut self, document: DocumentId) -> Option<&mut Registered<S>> {
        self.documents.iter_mut().find(|held| held.id == document)
    }

    fn view_mut(&mut self, document: DocumentId, view: ViewId) -> Option<&mut View> {
        self.registered_mut(document)?.view_mut(view)
    }

    /// Whether the session holds no document.
    pub(crate) fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// The first-opened document of the invocation `id`, as a file of that
    /// invocation joining it finds it. A document that names no invocation is
    /// never found.
    pub fn by_invocation(&self, id: &InvocationId) -> Option<Joinable> {
        let found = self
            .documents
            .iter()
            .find(|held| held.name.invocation() == Some(id))?;
        let mut views = Vec::with_capacity(found.members().map(Held::view_count).sum());
        for member in found.members() {
            views.extend(member.views().map(|view| view.kind));
        }
        Some(Joinable {
            document: found.id,
            invocation: id.clone(),
            views,
        })
    }

    fn position(&self, document: DocumentId) -> Option<usize> {
        self.documents.iter().position(|held| held.id == document)
    }

    /// Takes `document` out of the registry, which lets go of what its files
    /// share and of every model its views were minted over. The numbers its
    /// labels took stay taken.
    fn remove(&mut self, document: DocumentId) -> Option<Registered<S>> {
        let at = self.position(document)?;
        Some(self.documents.remove(at))
    }
}

/// `member` as a file of a document, its label numbered by `files`.
fn numbered_file(files: &mut Numbering, member: Member) -> Held<ViewSpec> {
    Held {
        file: FileName {
            ordinal: files.take(&member.label),
            label: member.label,
        },
        first: member.first,
        rest: member.rest,
    }
}

/// Where the panel showing a view comes from.
enum Showing {
    /// The panel already showing it, in whichever window holds it.
    Open(EntityId),
    Minted(AnyPanel),
}

/// Shows `view` of `document` in `workspace`: the panel already showing it,
/// moved here out of whatever window holds it, or a fresh panel where none is
/// open. Panels move once the registry's update has ended, so a window giving
/// one up may read the registry.
pub(crate) fn show(
    document: DocumentId,
    view: ViewId,
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let registry = Documents::global(cx);
    let showing = registry.update(cx, |registry, cx| registry.showing(document, view, cx));
    match showing {
        Some(Showing::Open(panel)) => reveal(panel, workspace, window, cx),
        Some(Showing::Minted(panel)) => {
            workspace.update(cx, |workspace, cx| workspace.add_panel(panel, window, cx));
        }
        None => {}
    }
}

/// Brings `panel` forward where `into` holds it, and otherwise moves it there
/// out of the window that does: a panel is rendered in one window at a time,
/// and Wayland offers no way to raise the window it sits in.
fn reveal(panel: EntityId, into: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    if into.update(cx, |workspace, cx| workspace.reveal(panel, window, cx)) {
        return;
    }
    let open = Documents::global(cx).read(cx).workspaces();
    if let Some(taken) = take_panel_from(open, panel, into.entity_id(), cx) {
        into.update(cx, |workspace, cx| workspace.add_panel(taken, window, cx));
    }
}

/// Takes the window `here` names out of `windows` where another of them is
/// open, `open` naming each window that is; whether it took it out. A window
/// taken out ahead of its closing counts as closed to the windows emptied
/// after it, so of several emptied one after another the last stays.
fn let_go_unless_last<W>(
    windows: &mut Vec<W>,
    here: EntityId,
    open: impl Fn(&W) -> Option<EntityId>,
) -> bool {
    let another = windows
        .iter()
        .any(|window| open(window).is_some_and(|id| id != here));
    if another {
        windows.retain(|window| open(window) != Some(here));
    }
    another
}

/// Takes `panel` out of whichever of `open` holds it, for a caller re-homing it
/// in the window it frames. The workspace `here` names is left alone: it is the
/// caller's own, which takes the panel out for itself. `None` where none of the
/// others holds it.
pub(crate) fn take_panel_from(
    open: Vec<Entity<Workspace>>,
    panel: EntityId,
    here: EntityId,
    cx: &mut App,
) -> Option<AnyPanel> {
    find_in_other_workspaces(open, here, cx, |workspace, window, cx| {
        workspace.take_panel(panel, window, cx)
    })
}

/// [`Workspace::close_document`] on `here`.
pub(crate) fn close(
    document: DocumentId,
    here: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // The other windows go first, so that where the close empties every
    // window, the one it was asked from is the last and stands empty.
    let documents = Documents::global(cx);
    let open = documents.read(cx).workspaces();
    find_in_other_workspaces(open, cx.entity_id(), cx, |workspace, window, cx| {
        workspace.drop_panels_of(document, window, cx);
        None::<()>
    });
    here.drop_panels_of(document, window, cx);
    documents.update(cx, |documents, cx| documents.forget(document, cx));
}

/// The first thing `visit` finds in the workspaces of `open`, visited in turn
/// in the windows they frame. The workspace `here` names is left alone, as the
/// caller holds it, and so is one whose window has closed.
fn find_in_other_workspaces<T>(
    open: Vec<Entity<Workspace>>,
    here: EntityId,
    cx: &mut App,
    mut visit: impl FnMut(&mut Workspace, &mut Window, &mut Context<Workspace>) -> Option<T>,
) -> Option<T> {
    for elsewhere in open {
        if elsewhere.entity_id() == here {
            continue;
        }
        let handle = elsewhere.read(cx).window();
        let found = handle.update(cx, |_, window, cx| {
            elsewhere.update(cx, |workspace, cx| visit(workspace, window, cx))
        });
        if let Ok(Some(found)) = found {
            return Some(found);
        }
    }
    None
}

/// Where `view`'s panel stands in the windows `workspaces` frame.
fn state(view: &View, workspaces: &[Entity<Workspace>], cx: &App) -> ViewState {
    let Some(live) = &view.live else {
        return ViewState::Closed;
    };
    let shown = workspaces
        .iter()
        .find_map(|workspace| workspace.read(cx).shows(live.panel, cx));
    match shown {
        Some(true) => ViewState::Visible,
        _ => ViewState::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry of documents sharing nothing, as bookkeeping apart from a
    /// running application holds them.
    type TestRegistry = Registry<()>;

    use crate::PanelGroup;
    use crate::layout::shape;
    use crate::testing::{Extent, arrange, assert_extents, extents, letter};
    use std::collections::HashMap;
    use std::num::NonZeroUsize;

    const TIMELINE: ViewKind = ViewKind::new("timeline");
    const FLAME: ViewKind = ViewKind::new("flame");
    const STATS: ViewKind = ViewKind::new("stats");
    const CONSOLE: ViewKind = ViewKind::new("console");
    const GRAPH: ViewKind = ViewKind::new("graph");

    /// The role of a view of `kind`, as the viewer showing it declares.
    fn role(kind: ViewKind) -> Role {
        match kind {
            TIMELINE => Role::Timeline,
            CONSOLE => Role::Log,
            GRAPH => Role::Canvas,
            _ => Role::Analysis,
        }
    }

    fn spec(kind: ViewKind) -> ViewSpec {
        ViewSpec::for_test(kind, role(kind))
    }

    fn view(kind: ViewKind) -> View {
        spec(kind).registered()
    }

    fn unnumbered(label: &str) -> FileName {
        FileName {
            label: label.to_string().into(),
            ordinal: None,
        }
    }

    /// `views` as the views `file` brings.
    fn held<V>(file: FileName, views: Vec<V>) -> Held<V> {
        let mut views = views.into_iter();
        Held {
            file,
            first: views.next().expect("a file brings a view"),
            rest: views.collect(),
        }
    }

    /// The name of a file opened by itself, of no invocation.
    fn file_name() -> Name {
        Name::File {
            invocation: None,
            plays: false,
        }
    }

    /// Registers a file opened by itself, which no other shares a label with.
    fn register(documents: &mut TestRegistry, label: &str, views: Vec<View>) -> DocumentId {
        let file = held(unnumbered(label), views);
        documents
            .register(file_name(), (), |_| ((file, Vec::new()), ()))
            .0
    }

    /// The view of `kind` a file opened by itself shows.
    fn alone(kind: ViewKind) -> ViewId {
        ViewId { member: 0, kind }
    }

    /// A timeline, a flame graph and a stats table, which is what a profile's
    /// viewer shows.
    fn profile_views() -> Vec<View> {
        vec![view(TIMELINE), view(FLAME), view(STATS)]
    }

    /// A registry holding a document per label, each showing a timeline and a
    /// flame graph.
    fn registry(labels: [&str; 2]) -> (TestRegistry, [DocumentId; 2]) {
        let mut documents = TestRegistry::new();
        let ids =
            labels.map(|label| register(&mut documents, label, vec![view(TIMELINE), view(FLAME)]));
        (documents, ids)
    }

    /// The panel a pane holds, named by a number so a fresh one is told from the
    /// one it replaces.
    fn panel(id: u64) -> EntityId {
        EntityId::from(id)
    }

    /// Which of the windows `ids` names, emptied in turn, close; each window
    /// open until it is let go of.
    fn closing(ids: [u64; 3], emptied: [u64; 3]) -> [bool; 3] {
        let mut windows: Vec<EntityId> = ids.into_iter().map(EntityId::from).collect();
        emptied.map(|id| let_go_unless_last(&mut windows, EntityId::from(id), |id| Some(*id)))
    }

    #[test]
    fn of_windows_emptied_one_after_another_the_last_stays() {
        for emptied in [[1, 2, 3], [3, 1, 2], [2, 3, 1], [3, 2, 1]] {
            assert_eq!(closing([1, 2, 3], emptied), [true, true, false]);
        }
    }

    #[test]
    fn a_closed_window_keeps_no_emptied_one_open() {
        let mut windows = vec![(EntityId::from(1), true), (EntityId::from(2), false)];
        let open = |(id, open): &(EntityId, bool)| open.then_some(*id);
        assert!(!let_go_unless_last(&mut windows, EntityId::from(1), open));
    }

    /// Records `panel` as showing `view` of `document`, watched by a
    /// subscription over nothing in place of the release observer a running
    /// application installs.
    fn opened(documents: &mut TestRegistry, document: DocumentId, view: ViewId, panel: EntityId) {
        documents.opened(document, view, panel, Subscription::new(|| {}));
    }

    #[test]
    fn a_view_no_panel_shows_is_closed() {
        let (documents, [profile, _]) = registry(["a.profile.gz", "b.profile.gz"]);
        assert_eq!(documents.panel(profile, alone(TIMELINE)), None);
    }

    #[test]
    fn a_view_is_open_at_the_panel_showing_it() {
        let (mut documents, [profile, _]) = registry(["a.profile.gz", "b.profile.gz"]);
        opened(&mut documents, profile, alone(TIMELINE), panel(7));
        assert_eq!(documents.panel(profile, alone(TIMELINE)), Some(panel(7)));
        assert_eq!(documents.panel(profile, alone(FLAME)), None);
    }

    /// The release of the panel showing a view is what closes it, and it closes
    /// that view alone.
    #[test]
    fn a_released_panel_closes_the_view_it_showed() {
        let (mut documents, [profile, _]) = registry(["a.profile.gz", "b.profile.gz"]);
        opened(&mut documents, profile, alone(TIMELINE), panel(1));
        opened(&mut documents, profile, alone(FLAME), panel(2));
        documents.closed(profile, alone(FLAME));
        assert_eq!(documents.panel(profile, alone(FLAME)), None);
        assert_eq!(documents.panel(profile, alone(TIMELINE)), Some(panel(1)));
    }

    /// Showing a closed view mints a panel of its own, which the record then
    /// names in place of the one that closed.
    #[test]
    fn a_reopened_view_is_open_at_a_fresh_panel() {
        let (mut documents, [profile, _]) = registry(["a.profile.gz", "b.profile.gz"]);
        opened(&mut documents, profile, alone(FLAME), panel(1));
        documents.closed(profile, alone(FLAME));
        opened(&mut documents, profile, alone(FLAME), panel(2));
        assert_eq!(documents.panel(profile, alone(FLAME)), Some(panel(2)));
    }

    /// Two documents show views of the same kinds, so a record kept per kind
    /// alone would have either one answering for the other.
    #[test]
    fn documents_showing_the_same_views_keep_records_of_their_own() {
        let (mut documents, [first, second]) = registry(["a.profile.gz", "b.profile.gz"]);
        opened(&mut documents, first, alone(TIMELINE), panel(1));
        opened(&mut documents, second, alone(TIMELINE), panel(2));
        documents.closed(first, alone(TIMELINE));
        assert_eq!(documents.panel(first, alone(TIMELINE)), None);
        assert_eq!(documents.panel(second, alone(TIMELINE)), Some(panel(2)));
    }

    #[test]
    fn every_document_registers_under_an_identity_of_its_own() {
        let (_, [first, second]) = registry(["a.profile.gz", "b.profile.gz"]);
        assert_ne!(first, second);
    }

    /// A document answers for the views it registered and no others, so asking
    /// it for a view another viewer shows names nothing and records nothing.
    #[test]
    fn a_view_a_document_does_not_show_is_not_its_own() {
        let (mut documents, [profile, _]) = registry(["a.profile.gz", "b.profile.gz"]);
        let graph = ViewKind::new("graph");
        opened(&mut documents, profile, alone(graph), panel(1));
        assert_eq!(documents.panel(profile, alone(graph)), None);
    }

    /// A registry holding a document per label, each showing a profile's views
    /// with a panel open for every one of them. The panels are named by letter
    /// in the order the documents registered and gave their views.
    fn shown(labels: &[&str]) -> (TestRegistry, Vec<(EntityId, char)>) {
        let mut documents = TestRegistry::new();
        let mut panels = Vec::new();
        for label in labels {
            let id = register(&mut documents, label, profile_views());
            for kind in [TIMELINE, FLAME, STATS] {
                let named = (panel(panels.len() as u64), letter(panels.len()));
                opened(&mut documents, id, alone(kind), named.0);
                panels.push(named);
            }
        }
        (documents, panels)
    }

    /// The arrangement `documents` describes for a window holding the panels
    /// `here` names, over the letters naming those panels. Each panel is taken
    /// as the registry meets it, exactly as a window's rebuild takes it, so a
    /// registry naming one panel from two places is handed it once.
    fn rebuilt(
        documents: &TestRegistry,
        panels: &[(EntityId, char)],
        here: &str,
    ) -> Option<PanelGroup<char>> {
        let mut held: HashMap<EntityId, char> = panels
            .iter()
            .filter(|(_, letter)| here.contains(*letter))
            .copied()
            .collect();
        documents
            .opening_regions(&mut |panel| held.remove(&panel))
            .map(Regions::arrange)
    }

    /// That arrangement written out, each pane as the panels it holds in tab
    /// order. A panel is laid out in one place: a window holds it in one.
    fn rebuilt_shape(documents: &TestRegistry, panels: &[(EntityId, char)], here: &str) -> String {
        let group = rebuilt(documents, panels, here).expect("an arrangement of panels");
        let written = shape(&arrange(group));
        let mut laid_out: Vec<char> = written.chars().filter(char::is_ascii_alphabetic).collect();
        let places = laid_out.len();
        laid_out.sort_unstable();
        laid_out.dedup();
        assert_eq!(laid_out.len(), places, "{written} lays a panel out twice");
        written
    }

    #[test]
    fn a_window_holding_every_view_lays_each_out_in_its_roles_region() {
        let (documents, panels) = shown(&["build.profile.gz"]);
        assert_eq!(rebuilt_shape(&documents, &panels, "abc"), "(a/bc)");
    }

    /// A view whose panel closed takes no place in the arrangement, and the
    /// panes left divide what they were laid out in.
    #[test]
    fn a_closed_view_takes_no_place_in_the_arrangement() {
        let (mut documents, panels) = shown(&["build.profile.gz"]);
        documents.closed(documents.documents[0].id, alone(FLAME));
        assert_eq!(rebuilt_shape(&documents, &panels, "abc"), "(a/c)");
        documents.closed(documents.documents[0].id, alone(TIMELINE));
        assert_eq!(rebuilt_shape(&documents, &panels, "abc"), "c");
    }

    /// A view another window holds goes on being open there: putting this window
    /// back leaves that panel where it is and lays out the rest.
    #[test]
    fn a_view_another_window_holds_stays_there() {
        let (documents, panels) = shown(&["build.profile.gz"]);
        assert_eq!(rebuilt_shape(&documents, &panels, "ac"), "(a/c)");
        assert_eq!(
            documents.panel(documents.documents[0].id, alone(FLAME)),
            Some(panels[1].0)
        );
    }

    /// A document none of whose panels the window holds is laid out nowhere in
    /// it, and a window holding none at all has no arrangement to be put back
    /// to.
    #[test]
    fn a_document_the_window_holds_nothing_of_is_left_out() {
        let (documents, panels) = shown(&["a.profile.gz", "b.profile.gz"]);
        assert_eq!(rebuilt_shape(&documents, &panels, "abcdef"), "(ad/bcef)");
        assert_eq!(rebuilt_shape(&documents, &panels, "def"), "(d/ef)");
        assert_eq!(rebuilt_shape(&documents, &panels, "b"), "b");
        assert!(rebuilt(&documents, &panels, "").is_none());
    }

    /// Putting a window back settles it: the arrangement described for the
    /// panels it holds once it has been put back is the one it was put into.
    #[test]
    fn putting_a_window_back_twice_lays_it_out_the_same_way() {
        let (documents, panels) = shown(&["a.profile.gz", "b.profile.gz"]);
        let once = rebuilt_shape(&documents, &panels, "abcdef");
        assert_eq!(rebuilt_shape(&documents, &panels, "abcdef"), once);
    }

    /// A document whose group asks twice for a view of one kind keeps the one
    /// record that kind is looked up by, so its second panel is one no document
    /// lays out and the window holding it has to keep a place for it itself.
    #[test]
    fn a_kind_a_document_asks_for_twice_is_laid_out_once() {
        let mut documents = TestRegistry::new();
        let id = register(
            &mut documents,
            "build.profile.gz",
            vec![view(TIMELINE), view(TIMELINE)],
        );
        opened(&mut documents, id, alone(TIMELINE), panel(0));
        opened(&mut documents, id, alone(TIMELINE), panel(1));
        assert_eq!(
            rebuilt_shape(&documents, &[(panel(0), 'a'), (panel(1), 'b')], "ab"),
            "b"
        );
    }

    /// A profile, a build event stream and a graph, each with the views its
    /// viewer shows.
    fn invocation_files() -> [Held<ViewSpec>; 3] {
        let file = |label, views| held(unnumbered(label), views);
        [
            file(
                "build.profile.gz",
                vec![spec(TIMELINE), spec(FLAME), spec(STATS)],
            ),
            file("build.bep", vec![spec(CONSOLE)]),
            file("aquery.pb", vec![spec(GRAPH)]),
        ]
    }

    /// A registry holding a document per entry of `documents`, each the files
    /// given for it, with a panel open for every view. The panels are named by
    /// letter in the order the documents registered and their files gave their
    /// views.
    fn showing(documents: Vec<Vec<Held<ViewSpec>>>) -> (TestRegistry, Vec<(EntityId, char)>) {
        let mut registry = TestRegistry::new();
        let mut panels = Vec::new();
        for (at, files) in documents.into_iter().enumerate() {
            let members: Vec<Held<View>> = files.iter().map(Held::registered).collect();
            let views: Vec<ViewId> = members
                .iter()
                .enumerate()
                .flat_map(|(member, held)| {
                    let kinds = held.views().map(|view| view.kind);
                    kinds.map(move |kind| ViewId { member, kind })
                })
                .collect();
            let label = format!("d{at}");
            let name = match members.len() {
                1 => file_name(),
                _ => Name::Invocation {
                    id: InvocationId::new(&label).expect("a non-empty id"),
                    label: label.into(),
                },
            };
            let mut members = members.into_iter();
            let first = members.next().expect("a document holds a file");
            let rest = members.collect();
            let (id, ()) = registry.register(name, (), |_| ((first, rest), ()));
            for view in views {
                let named = (panel(panels.len() as u64), letter(panels.len()));
                opened(&mut registry, id, view, named.0);
                panels.push(named);
            }
        }
        (registry, panels)
    }

    /// The arrangement a window holding the panels `here` names is put back
    /// to.
    fn rebuilt_layout(
        documents: &TestRegistry,
        panels: &[(EntityId, char)],
        here: &str,
    ) -> crate::Layout<String> {
        arrange(rebuilt(documents, panels, here).expect("an arrangement of panels"))
    }

    /// Joining files changes what they are listed under, not where a window lays
    /// their views out. Whichever of its panels the window holds, every pane
    /// takes the same place with the files opened apart, joined into one
    /// document, and two of them joined beside the third.
    #[test]
    fn joined_files_are_laid_out_as_they_are_apart() {
        let apart = || {
            let [profile, bep, graph] = invocation_files();
            showing(vec![vec![profile], vec![bep], vec![graph]])
        };
        let joined = || showing(vec![invocation_files().into()]);
        let beside = || {
            let [profile, bep, graph] = invocation_files();
            showing(vec![vec![profile, bep], vec![graph]])
        };
        let held = [
            ("abcde", "((e/d)|(a/bc))"),
            ("abce", "(e|(a/bc))"),
            ("de", "(e/d)"),
            ("ad", "(d|a)"),
        ];
        let (apart, joined, beside) = (apart(), joined(), beside());
        assert_eq!(
            [&apart, &joined, &beside].map(|(documents, _)| documents.documents.len()),
            [3, 1, 2]
        );
        for (here, want_shape) in held {
            let [apart, joined, beside] = [&apart, &joined, &beside]
                .map(|(documents, panels)| rebuilt_layout(documents, panels, here));
            assert_eq!(shape(&apart), want_shape);
            let laid_out_apart = extents(&apart);
            let apart: Vec<Extent> = laid_out_apart
                .iter()
                .map(|(pane, width, height)| (pane.as_str(), *width, *height))
                .collect();
            for (opened_as, layout) in [("joined", joined), ("joined beside a file", beside)] {
                assert_eq!(shape(&layout), want_shape, "{opened_as}");
                assert_extents(&layout, &apart);
            }
        }
    }

    #[test]
    fn only_a_document_joining_files_has_a_joined_name() {
        let [profile, bep, graph] = invocation_files();
        let (documents, _) = showing(vec![vec![profile, bep], vec![graph]]);
        let [joined, alone] = [0, 1].map(|at| documents.documents[at].id);
        assert_eq!(documents.joined_name(joined), Some("d0".into()));
        assert_eq!(documents.joined_name(alone), None);
    }

    /// Two profiles joined into one document, each a timeline over a flame
    /// graph and a stats table, with a panel open for every view.
    fn two_profiles_joined() -> (TestRegistry, Vec<(EntityId, char)>) {
        let profile = || {
            held(
                unnumbered("build.profile.gz"),
                vec![spec(TIMELINE), spec(FLAME), spec(STATS)],
            )
        };
        showing(vec![vec![profile(), profile()]])
    }

    /// The views of a profile that is the `member`-th file of its document, in
    /// the order its viewer gives them.
    fn profile_views_of(member: usize) -> [ViewId; 3] {
        [TIMELINE, FLAME, STATS].map(|kind| ViewId { member, kind })
    }

    /// Files of one document showing views of the same kinds each keep a
    /// record per view, so every panel is the one its own file's view names.
    #[test]
    fn files_sharing_every_kind_keep_records_of_their_own() {
        let (documents, panels) = two_profiles_joined();
        let id = documents.documents[0].id;
        let found: Vec<Option<EntityId>> = (0..2)
            .flat_map(profile_views_of)
            .map(|view| documents.panel(id, view))
            .collect();
        let held: Vec<Option<EntityId>> = panels.iter().map(|(panel, _)| Some(*panel)).collect();
        assert_eq!(found, held);
    }

    #[test]
    fn files_sharing_every_kind_list_a_view_each() {
        let (documents, _) = two_profiles_joined();
        let listed = documents.documents[0].listed(&mut |view| match view.live {
            Some(_) => ViewState::Open,
            None => ViewState::Closed,
        });
        let views: Vec<ViewId> = listed.views.iter().map(|view| view.id).collect();
        let expected: Vec<ViewId> = (0..2).flat_map(profile_views_of).collect();
        assert_eq!(views, expected);
        assert!(
            listed
                .views
                .iter()
                .all(|view| view.state == ViewState::Open)
        );
    }

    /// Closing a view of one file leaves the same view of the other open, and
    /// reopening it records the fresh panel under the file it was closed in.
    #[test]
    fn a_view_two_files_share_the_kind_of_closes_and_reopens_in_its_own_file() {
        let (mut documents, panels) = two_profiles_joined();
        let id = documents.documents[0].id;
        let [_, first_flame, _] = profile_views_of(0);
        let [_, second_flame, _] = profile_views_of(1);
        documents.closed(id, second_flame);
        assert_eq!(documents.panel(id, first_flame), Some(panels[1].0));
        assert_eq!(documents.panel(id, second_flame), None);
        opened(&mut documents, id, second_flame, panel(9));
        assert_eq!(documents.panel(id, first_flame), Some(panels[1].0));
        assert_eq!(documents.panel(id, second_flame), Some(panel(9)));
    }

    /// Each file of a document is minted with the number of its own label, so a
    /// view reopened in one file carries that file's number.
    #[test]
    fn a_view_is_found_with_the_number_of_its_own_file() {
        let mut documents = TestRegistry::new();
        let file = |ordinal| Held {
            file: FileName {
                ordinal: NonZeroUsize::new(ordinal),
                ..unnumbered("build.profile.gz")
            },
            first: view(TIMELINE),
            rest: Vec::new(),
        };
        let (id, ()) = documents.register(file_name(), (), |_| ((file(1), vec![file(2)]), ()));
        let ordinals: Vec<Option<NonZeroUsize>> = (0..2)
            .map(|member| {
                let timeline = ViewId {
                    member,
                    kind: TIMELINE,
                };
                let (file, _) = documents.registered(id)?.view(timeline)?;
                file.file.ordinal
            })
            .collect();
        assert_eq!(ordinals, [NonZeroUsize::new(1), NonZeroUsize::new(2)]);
    }

    /// Putting a window back lays the panels of files sharing every kind out as
    /// tabs of their roles' regions, file by file, whichever of them the window
    /// holds.
    #[test]
    fn files_sharing_every_kind_are_laid_out_file_by_file() {
        let (documents, panels) = two_profiles_joined();
        assert_eq!(rebuilt_shape(&documents, &panels, "abcdef"), "(ad/bcef)");
        assert_eq!(rebuilt_shape(&documents, &panels, "def"), "(d/ef)");
        assert_eq!(rebuilt_shape(&documents, &panels, "aef"), "(a/ef)");
    }

    /// A view is a file's own: naming it by a file the document does not hold,
    /// or by a kind that file does not show, names nothing.
    #[test]
    fn a_view_is_found_in_the_file_bringing_it_alone() {
        let [profile, bep, graph] = invocation_files();
        let (documents, panels) = showing(vec![vec![profile, bep, graph]]);
        let id = documents.documents[0].id;
        let console = |member| ViewId {
            member,
            kind: CONSOLE,
        };
        assert_eq!(documents.panel(id, console(1)), Some(panels[3].0));
        assert_eq!(documents.panel(id, console(0)), None);
        assert_eq!(documents.panel(id, console(3)), None);
    }

    fn numbers(labels: &[&'static str]) -> Vec<Option<usize>> {
        let mut numbering = Numbering::default();
        labels
            .iter()
            .map(|&label| numbering.take(&label.into()).map(NonZeroUsize::get))
            .collect()
    }

    #[test]
    fn distinct_labels_are_not_numbered() {
        assert_eq!(numbers(&[]), []);
        assert_eq!(numbers(&["a.bep", "b.bep"]), [None, None]);
    }

    #[test]
    fn a_label_s_first_bearer_is_unnumbered_and_later_ones_count_from_two() {
        assert_eq!(numbers(&["x", "x"]), [None, Some(2)]);
        assert_eq!(
            numbers(&["x", "y", "x", "x"]),
            [None, None, Some(2), Some(3)]
        );
    }

    #[test]
    fn each_label_counts_on_its_own() {
        assert_eq!(
            numbers(&["x", "y", "y", "x", "z"]),
            [None, None, Some(2), Some(2), None]
        );
    }

    fn member(label: &'static str, kinds: &[ViewKind]) -> Member {
        let mut specs = kinds.iter().map(|&kind| spec(kind));
        let first = specs.next().expect("a file brings a view");
        Member::new(label.into(), first, specs.collect())
    }

    fn profile_member(label: &'static str) -> Member {
        member(label, &[TIMELINE, FLAME])
    }

    fn id(id: &str) -> InvocationId {
        InvocationId::new(id).expect("a non-empty id")
    }

    fn lone(member: Member) -> DocumentOf<()> {
        DocumentOf::alone(member, None, ())
    }

    /// A profile and a build event stream of the invocation `invocation`.
    fn session(invocation: &'static str, profile: &'static str) -> DocumentOf<()> {
        DocumentOf::join(
            id(invocation),
            profile_member(profile),
            member("build.bep", &[CONSOLE]),
            Vec::new(),
            (),
        )
    }

    /// Each document `registry` opens of `documents`, in their order, as what
    /// it is listed under and the numbered labels of its files.
    fn named_in(
        registry: &mut TestRegistry,
        documents: Vec<DocumentOf<()>>,
    ) -> Vec<(String, Vec<String>)> {
        documents
            .into_iter()
            .map(|document| {
                let registration = registry.numbered(document);
                let files = iter::once(&registration.first)
                    .chain(&registration.rest)
                    .map(|held| held.file.numbered().to_string())
                    .collect();
                let name = registration.name.listed_as(&registration.first.file);
                (name.to_string(), files)
            })
            .collect()
    }

    fn named(documents: Vec<DocumentOf<()>>) -> Vec<(String, Vec<String>)> {
        named_in(&mut TestRegistry::new(), documents)
    }

    fn names(document: &str, files: &[&str]) -> (String, Vec<String>) {
        let files = files.iter().map(|file| file.to_string()).collect();
        (document.to_string(), files)
    }

    /// A joined document holds each file as the file's viewer opened it: under
    /// its own label, with the views it asked for.
    #[test]
    fn a_joined_document_holds_every_file_as_it_was_opened() {
        let joined = DocumentOf::join(
            id("a5f7f3e6"),
            profile_member("build.profile.gz"),
            member("build.bep", &[CONSOLE]),
            vec![member("aquery.pb", &[GRAPH])],
            (),
        );
        let registration = TestRegistry::new().numbered(joined);
        assert_eq!(
            registration.name.listed_as(&registration.first.file),
            "Invocation a5f7f3e6"
        );
        let files: Vec<(String, Vec<ViewKind>)> = iter::once(&registration.first)
            .chain(&registration.rest)
            .map(|held| {
                let kinds = held.views().map(ViewSpec::kind).collect();
                (held.file.numbered().to_string(), kinds)
            })
            .collect();
        assert_eq!(
            files,
            [
                ("build.profile.gz".to_string(), vec![TIMELINE, FLAME]),
                ("build.bep".to_string(), vec![CONSOLE]),
                ("aquery.pb".to_string(), vec![GRAPH]),
            ]
        );
    }

    /// A panel is titled by its file, so files of one name are told apart by
    /// number in whichever documents they are held.
    #[test]
    fn files_sharing_a_label_are_numbered_across_joined_documents() {
        assert_eq!(
            named(vec![
                session("a5f7f3e6", "build.profile.gz"),
                session("0b437916", "build.profile.gz"),
            ]),
            [
                names("Invocation a5f7f3e6", &["build.profile.gz", "build.bep"]),
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
            named(vec![
                session("a5f7f3e6", "build.profile.gz"),
                lone(profile_member("build.profile.gz")),
                lone(member("aquery.pb", &[GRAPH])),
            ]),
            [
                names("Invocation a5f7f3e6", &["build.profile.gz", "build.bep"]),
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
            named(vec![
                session("a5f7f3e6", "a.profile.gz"),
                lone(member("Invocation a5f7f3e6", &[GRAPH])),
                session("a5f7f3e6", "b.profile.gz"),
                session("0b437916", "c.profile.gz"),
            ]),
            [
                names("Invocation a5f7f3e6", &["a.profile.gz", "build.bep"]),
                names("Invocation a5f7f3e6", &["Invocation a5f7f3e6"]),
                names(
                    "Invocation a5f7f3e6 (2)",
                    &["b.profile.gz", "build.bep (2)"]
                ),
                names("Invocation 0b437916", &["c.profile.gz", "build.bep (3)"]),
            ]
        );
    }

    /// Numbers count across everything the session opens, not one window's
    /// worth: a file opened later takes the number after the last one taken.
    #[test]
    fn a_later_opening_numbers_on_from_the_earlier_ones() {
        let mut registry = TestRegistry::new();
        let profile = || lone(profile_member("build.profile.gz"));
        named_in(&mut registry, vec![profile(), profile()]);
        assert_eq!(
            named_in(&mut registry, vec![profile()]),
            [names("build.profile.gz (3)", &["build.profile.gz (3)"])]
        );
    }

    /// Registers `document` as opening it does, minting no panel.
    fn open_unshown(registry: &mut TestRegistry, document: DocumentOf<()>) -> DocumentId {
        let Registration {
            name,
            first,
            rest,
            shared,
        } = registry.numbered(document);
        let members = (
            first.registered(),
            rest.iter().map(Held::registered).collect(),
        );
        registry.register(name, shared, |_| (members, ())).0
    }

    #[test]
    fn a_file_naming_its_invocation_is_found_by_it_with_the_views_it_shows() {
        let mut registry = TestRegistry::new();
        let profile =
            DocumentOf::alone(profile_member("build.profile.gz"), Some(id("a5f7f3e6")), ());
        let opened = open_unshown(&mut registry, profile);
        let found = registry
            .by_invocation(&id("a5f7f3e6"))
            .expect("the profile's invocation");
        assert_eq!(found.document(), opened);
        assert!(found.shows(TIMELINE) && found.shows(FLAME));
        assert!(!found.shows(CONSOLE));
        assert!(registry.by_invocation(&id("0b437916")).is_none());
    }

    /// Only an invocation's id finds a document, so one that names none is
    /// never joined.
    #[test]
    fn a_document_naming_no_invocation_is_never_found() {
        let mut registry = TestRegistry::new();
        open_unshown(&mut registry, lone(profile_member("build.profile.gz")));
        assert!(
            registry
                .documents
                .iter()
                .all(|held| held.name.invocation().is_none())
        );
        assert!(registry.by_invocation(&id("build.profile.gz")).is_none());
    }

    /// Two documents may name one invocation, as a second profile of it opens
    /// by itself; a joiner finds the one that opened first.
    #[test]
    fn of_two_documents_of_an_invocation_the_first_opened_is_found() {
        let mut registry = TestRegistry::new();
        let joined = open_unshown(&mut registry, session("a5f7f3e6", "build.profile.gz"));
        let profile =
            DocumentOf::alone(profile_member("build.profile.gz"), Some(id("a5f7f3e6")), ());
        open_unshown(&mut registry, profile);
        let found = registry
            .by_invocation(&id("a5f7f3e6"))
            .expect("the invocation");
        assert_eq!(found.document(), joined);
        assert!(found.shows(CONSOLE));
    }

    /// Every invocation's document plays back; a file by itself plays only
    /// where it has time to play by itself.
    #[test]
    fn an_invocation_and_a_file_playing_alone_play_back() {
        let mut registry = TestRegistry::new();
        let still = open_unshown(&mut registry, lone(profile_member("build.profile.gz")));
        let console = lone(member("build.bep", &[CONSOLE]).playing_alone());
        let console = open_unshown(&mut registry, console);
        let joined = open_unshown(&mut registry, session("a5f7f3e6", "a.profile.gz"));
        let plays = |document| {
            let registered = registry.registered(document).expect("an open document");
            registered.name.plays()
        };
        assert_eq!([still, console, joined].map(plays), [false, true, true]);
    }

    /// The document `id` names, as a registry lists it while no panel shows
    /// a view: its name and its views.
    fn listing(registry: &TestRegistry, id: DocumentId) -> (String, Vec<ViewId>) {
        let registered = registry.registered(id).expect("an open document");
        let listed = registered.listed(&mut |_| ViewState::Closed);
        let views = listed.views.iter().map(|view| view.id).collect();
        (listed.label.to_string(), views)
    }

    /// Appends `members` to the document `to` names, as joining it does, and
    /// gives the place of the first among the document's files.
    fn append(registry: &mut TestRegistry, to: &Joinable, members: Vec<Member>) -> usize {
        let at = registry.position(to.document).expect("an open document");
        let (files, joined) = (&mut registry.files, &mut registry.joined);
        registry.documents[at].append(&to.invocation, members, files, joined)
    }

    /// A file by itself that another of its invocation joins becomes that
    /// invocation's document: listed under its label, playing back, and
    /// holding its first file's views where they were, the joining file's
    /// after them.
    #[test]
    fn a_file_joined_by_another_of_its_invocation_becomes_the_invocation_s() {
        let mut registry = TestRegistry::new();
        let profile =
            DocumentOf::alone(profile_member("build.profile.gz"), Some(id("a5f7f3e6")), ());
        let opened = open_unshown(&mut registry, profile);
        let to = registry
            .by_invocation(&id("a5f7f3e6"))
            .expect("the invocation");
        assert_eq!(
            append(&mut registry, &to, vec![member("build.bep", &[CONSOLE])]),
            1
        );
        let (name, views) = listing(&registry, opened);
        assert_eq!(name, "Invocation a5f7f3e6");
        assert_eq!(
            views,
            [
                ViewId::for_test(0, TIMELINE),
                ViewId::for_test(0, FLAME),
                ViewId::for_test(1, CONSOLE),
            ]
        );
        let registered = registry.registered(opened).expect("an open document");
        assert!(registered.name.plays());
        assert_eq!(
            registry.joined_name(opened),
            Some("Invocation a5f7f3e6".into())
        );
    }

    /// A document renamed on joining takes the next number of its
    /// invocation's label, as one opened joined does.
    #[test]
    fn a_renamed_document_is_numbered_among_the_joined_ones() {
        let mut registry = TestRegistry::new();
        open_unshown(&mut registry, session("a5f7f3e6", "a.profile.gz"));
        let profile = DocumentOf::alone(profile_member("b.profile.gz"), Some(id("a5f7f3e6")), ());
        let lone = open_unshown(&mut registry, profile);
        let to = Joinable {
            document: lone,
            invocation: id("a5f7f3e6"),
            views: Vec::new(),
        };
        append(&mut registry, &to, vec![member("build.bep", &[CONSOLE])]);
        assert_eq!(listing(&registry, lone).0, "Invocation a5f7f3e6 (2)");
    }

    /// Files joining an invocation's document leave its name as it is, and are
    /// numbered among every file opened before them.
    #[test]
    fn files_joining_an_invocation_s_document_keep_its_name() {
        let mut registry = TestRegistry::new();
        let joined = open_unshown(&mut registry, session("a5f7f3e6", "build.profile.gz"));
        let to = registry
            .by_invocation(&id("a5f7f3e6"))
            .expect("the invocation");
        let joining = vec![
            member("aquery.pb", &[GRAPH]),
            member("build.bep", &[CONSOLE]),
        ];
        assert_eq!(append(&mut registry, &to, joining), 2);
        assert_eq!(listing(&registry, joined).0, "Invocation a5f7f3e6");
        let registered = registry.registered(joined).expect("an open document");
        let files: Vec<String> = registered
            .members()
            .map(|held| held.file.numbered().to_string())
            .collect();
        assert_eq!(
            files,
            [
                "build.profile.gz",
                "build.bep",
                "aquery.pb",
                "build.bep (2)"
            ]
        );
    }

    /// A closed document is found no more, by its invocation or otherwise, and
    /// the documents opened beside it stay as they were.
    #[test]
    fn a_closed_document_is_gone_and_the_others_stay() {
        let mut registry = TestRegistry::new();
        let closed = open_unshown(&mut registry, session("a5f7f3e6", "a.profile.gz"));
        let kept = open_unshown(&mut registry, session("0b437916", "b.profile.gz"));
        assert!(registry.remove(closed).is_some());
        assert!(registry.registered(closed).is_none());
        assert!(registry.by_invocation(&id("a5f7f3e6")).is_none());
        assert_eq!(listing(&registry, kept).0, "Invocation 0b437916");
        assert!(registry.remove(closed).is_none());
    }

    /// A number a closed document took is not handed to the next document
    /// bearing its label, and the documents still open keep theirs.
    #[test]
    fn numbers_are_not_reused_after_a_close() {
        let mut registry = TestRegistry::new();
        let profile = || lone(profile_member("build.profile.gz"));
        let first = open_unshown(&mut registry, profile());
        let second = open_unshown(&mut registry, profile());
        registry.remove(second);
        let third = open_unshown(&mut registry, profile());
        assert_eq!(listing(&registry, first).0, "build.profile.gz");
        assert_eq!(listing(&registry, third).0, "build.profile.gz (3)");
    }
}
