//! The bviz window: a document per file named on the command line or to a
//! running window, the files of one Bazel invocation sharing one.

mod classify;
mod group;
#[cfg(target_family = "wasm")]
mod web;

use bviz_bazel_flags_args::BazelrcArgs;
#[cfg(not(target_family = "wasm"))]
use bviz_cli::{Cli, Request, Source, Sources};
use bviz_input::Input;
use bviz_invocation::InvocationId;
#[cfg(not(target_family = "wasm"))]
use bviz_ui_components::Pending;
use bviz_ui_components::{ChoiceDialog, ChoiceDialogEvent, ChoiceItem};
use bviz_ui_graph_args::{GraphFiles, GraphSource};
use bviz_ui_session::Sharing;
use bviz_ui_workspace::{
    AppAction, Document, Documents, Joinable, Member, Panel, PathOpener, Shared, ViewKind,
    Workspace, document_label,
};
#[cfg(not(target_family = "wasm"))]
use clap::Parser;
use classify::Classified;
use gpui::{App, AppContext, ClipboardItem, Context, SharedString, WeakEntity, Window, actions};
use group::{File, Given, Kind, Open, Opening, Placing};
use std::iter;
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: bviz_alloc::Allocator = bviz_alloc::Allocator;

/// A document being read, whichever viewer shows it.
enum Load {
    Graph(bviz_ui_graph::Load),
    Trace(bviz_ui_trace::Load),
    Console(bviz_ui_console::Load),
    ExecLog(bviz_ui_execlog::Load),
}

impl Load {
    /// Starts reading `source`, having first asked its head which invocation
    /// wrote it.
    #[cfg(not(target_family = "wasm"))]
    fn start(source: Source) -> File<Self> {
        match source {
            Source::Trace(source) => Self::read(
                Kind::Profile,
                Input::open(&source.path),
                source,
                bviz_ui_trace::invocation_id,
                Self::Trace,
            ),
            Source::Console(source) => Self::read(
                Kind::Bep,
                Input::open(&source.path),
                source,
                bviz_ui_console::invocation_id,
                Self::Console,
            ),
            Source::ExecLog(source) => Self::read(
                Kind::ExecLog,
                Input::open(&source.path),
                source,
                bviz_ui_execlog::invocation_id,
                Self::ExecLog,
            ),
        }
    }

    /// [`start`](Self::start) of `source`, through `opened`, the file it names
    /// opened once for both the probe and the read. A source that did not open
    /// names no invocation, and is left to its viewer's loader to report why.
    #[cfg(not(target_family = "wasm"))]
    fn read<S, L: From<S> + From<Input>>(
        kind: Kind,
        opened: std::io::Result<Input>,
        source: S,
        probe: fn(&Input) -> Option<InvocationId>,
        load: fn(L) -> Self,
    ) -> File<Self> {
        match opened {
            Ok(input) => File {
                kind,
                invocation: probe(&input),
                file: load(input.into()),
            },
            Err(_) => File {
                kind,
                invocation: None,
                file: load(source.into()),
            },
        }
    }

    /// The load reading `input`, a file of `kind`.
    fn of(kind: Kind, input: Input) -> Self {
        match kind {
            Kind::Profile => Self::Trace(input.into()),
            Kind::Bep => Self::Console(input.into()),
            Kind::ExecLog => Self::ExecLog(input.into()),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn start_all(sources: Sources) -> Given<Self> {
        let start_each = |files: Vec<Source>| files.into_iter().map(Self::start).collect();
        match sources {
            Sources::Graph { graph, files } => Given::Graph {
                graph: Self::Graph(graph.into()),
                files: start_each(files),
            },
            Sources::Files { first, rest } => Given::Files {
                first: Self::start(first),
                rest: start_each(rest),
            },
        }
    }

    /// The views of what this reads, sharing `shared` with the other files of
    /// its document, which `sharing` says whether it is an invocation's.
    fn open(self, shared: &Shared, sharing: Sharing, window: &mut Window, cx: &mut App) -> Member {
        let (session, clock) = (shared.session().clone(), shared.clock().clone());
        match self {
            Self::Graph(load) => bviz_ui_graph::open(load, session, clock, sharing, window, cx),
            Self::Trace(load) => bviz_ui_trace::open(load, session, clock, window, cx),
            Self::Console(load) => bviz_ui_console::open(load, session, clock, window, cx),
            Self::ExecLog(load) => bviz_ui_execlog::open(load, session, clock, sharing, window, cx),
        }
    }
}

/// The document `opening` is shown as: a file's own, or an invocation's files
/// joined, each with a session and a clock of its own.
fn open(opening: Opening<Load>, window: &mut Window, cx: &mut App) -> Document {
    let shared = Shared::new(cx);
    match opening {
        Opening::Alone { file, invocation } => {
            let sharing = if invocation.is_some() {
                Sharing::Invocation
            } else {
                Sharing::Alone
            };
            let member = file.open(&shared, sharing, window, cx);
            Document::alone(member, invocation, shared)
        }
        Opening::Invocation {
            id,
            first,
            second,
            rest,
        } => {
            let others = iter::once(second).chain(rest);
            let (first, others) = open_invocation_files(first, others, &shared, window, cx);
            of_invocation(id, first, others, shared)
        }
    }
}

/// The views of `first` and `rest`, files of one invocation sharing `shared`.
fn open_invocation_files(
    first: Load,
    rest: impl IntoIterator<Item = Load>,
    shared: &Shared,
    window: &mut Window,
    cx: &mut App,
) -> (Member, Vec<Member>) {
    let mut open = |load: Load| load.open(shared, Sharing::Invocation, window, cx);
    let first = open(first);
    let rest = rest.into_iter();
    let mut others = Vec::with_capacity(rest.size_hint().0);
    for load in rest {
        others.push(open(load));
    }
    (first, others)
}

/// Opens `paths`, named to the window `into` frames, once a background task
/// has told what each holds.
fn open_paths(paths: Vec<PathBuf>, into: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    open_classified(move || classify::classify(paths), into, window, cx);
}

/// Opens what `classifying` tells of files named to the window `into` frames.
fn open_classified(
    classifying: impl FnOnce() -> Classified + Send + 'static,
    into: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    off_thread(classifying, place, into, window, cx);
}

/// Opens the files named on the command line, once `pending` has told what
/// each holds, into the window `into` frames.
#[cfg(not(target_family = "wasm"))]
fn open_named(
    pending: Pending<Given<Load>>,
    into: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    off_thread(move || pending.join(), place_named, into, window, cx);
}

/// Opens what `named` tells of the files named on the command line into
/// `workspace`, telling the user where it tells nothing.
#[cfg(not(target_family = "wasm"))]
fn place_named(
    named: Result<Given<Load>, String>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match named {
        Ok(given) => open_given(given, Vec::new(), workspace, window, cx),
        Err(why) => workspace.tell(format!("Cannot open the files named: {why}"), cx),
    }
}

/// Runs `work` on a background task, as reading the file system must be, and
/// then `then` with its result on the workspace `into`, while it stands.
fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    then: impl FnOnce(T, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    into: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let working = cx.background_executor().spawn(async move { work() });
    window
        .spawn(cx, async move |cx| {
            let worked = working.await;
            into.update_in(cx, |workspace, window, cx| {
                then(worked, workspace, window, cx);
            })
            .ok();
        })
        .detach();
}

/// What the dialog asking the kind of files says of why it asks.
const ASK_KIND: &str = "bviz cannot tell from how these files begin what they hold. \
     Choose what to read each as, or skip it.";

/// Asks the user what each of `unknown` holds, and opens those given a kind as
/// files named to the window `workspace` frames, after the files opened
/// with them.
fn ask_kinds(
    unknown: Vec<PathBuf>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let choices = (classify::KINDS.iter())
        .map(|&(kind, name)| (kind, SharedString::from(name)))
        .collect();
    let items = unknown.iter().map(|path| {
        ChoiceItem::new(document_label(path)).detail(path.to_string_lossy().into_owned())
    });
    let dialog =
        cx.new(|cx| ChoiceDialog::new("Open files as", ASK_KIND, "Open", choices, items, cx));
    workspace.ask(
        dialog,
        move |_, event: &ChoiceDialogEvent<bviz_sniff::Kind>, window, cx| {
            let ChoiceDialogEvent::Chosen(chosen) = event else {
                return;
            };
            let chosen: Vec<(PathBuf, bviz_sniff::Kind)> = (unknown.iter().zip(chosen))
                .filter_map(|(path, &kind)| Some((path.clone(), kind?)))
                .collect();
            if !chosen.is_empty() {
                let into = cx.weak_entity();
                open_classified(move || classify::classify_as(chosen), into, window, cx);
            }
        },
        window,
        cx,
    );
}

/// Opens what `classified` names into `workspace`, as documents grouped as
/// the command line groups them, tells the user of each file it cannot open,
/// and asks what each file it cannot tell holds.
fn place(
    classified: Classified,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Classified {
        graph,
        further_graphs,
        files,
        unknown,
        unreadable,
    } = classified;
    for (path, error) in unreadable {
        workspace.tell(format!("Cannot open {}: {error}", path.display()), cx);
    }
    open_batch(graph, further_graphs, files, workspace, window, cx);
    if !unknown.is_empty() {
        ask_kinds(unknown, workspace, window, cx);
    }
}

/// Opens `graph`, `further_graphs` and `files`, named together, into
/// `workspace` as documents grouped as the command line groups them.
fn open_batch(
    graph: GraphFiles,
    further_graphs: Vec<GraphSource>,
    files: Vec<File<Input>>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let files = files
        .into_iter()
        .map(|file| File {
            file: Load::of(file.kind, file.file),
            kind: file.kind,
            invocation: file.invocation,
        })
        .collect();
    if let Some(given) = given(graph.source(), files) {
        open_given(given, further_graphs, workspace, window, cx);
    }
}

/// Opens `given`, and `further_graphs` named with it, into `workspace`, as
/// documents grouped by invocation, joining those of their invocations open
/// already.
fn open_given(
    given: Given<Load>,
    further_graphs: Vec<GraphSource>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (first, rest) = {
        let registry = Documents::global(cx).read(cx);
        group::with_open(given, &|id| registry.by_invocation(id).map(standing))
    };
    let further = further_graphs.into_iter().map(|graph| {
        Placing::New(Opening::Alone {
            file: Load::Graph(graph.into()),
            invocation: None,
        })
    });
    let mut documents = Vec::with_capacity(1 + rest.len() + further.len());
    for placing in iter::once(first).chain(rest).chain(further) {
        match placing {
            Placing::New(opening) => documents.push(open(opening, window, cx)),
            Placing::Join {
                document,
                first,
                rest,
            } => {
                let joining = move |shared: &Shared, window: &mut Window, cx: &mut App| {
                    open_invocation_files(first, rest, shared, window, cx)
                };
                if let Err(joining) = workspace.join_document(&document, joining, window, cx) {
                    let shared = Shared::new(cx);
                    let (first, rest) = joining(&shared, window, cx);
                    let id = document.invocation().clone();
                    documents.push(of_invocation(id, first, rest, shared));
                }
            }
        }
    }
    workspace.open_documents(documents, window, cx);
}

/// `joinable` as files given later find it.
fn standing(joinable: Joinable) -> Open<Joinable> {
    let mut held = Vec::with_capacity(Kind::ALL.len());
    for kind in Kind::ALL {
        if joinable.shows(lead_view(kind)) {
            held.push(kind);
        }
    }
    Open {
        shows_graph: joinable.shows(bviz_ui_graph::LeadPanel::kind().view),
        held,
        document: joinable,
    }
}

/// The view a file of `kind` shows first, by which an open document tells it
/// holds one.
fn lead_view(kind: Kind) -> ViewKind {
    match kind {
        Kind::Profile => bviz_ui_trace::LeadPanel::kind().view,
        Kind::Bep => bviz_ui_console::LeadPanel::kind().view,
        Kind::ExecLog => bviz_ui_execlog::LeadPanel::kind().view,
    }
}

/// The document `first` and `rest` make, files of the invocation `id`.
fn of_invocation(id: InvocationId, first: Member, rest: Vec<Member>, shared: Shared) -> Document {
    let mut rest = rest.into_iter();
    match rest.next() {
        Some(second) => Document::join(id, first, second, rest.collect(), shared),
        None => Document::alone(first, Some(id), shared),
    }
}

/// What `graph` and `files` name, `None` where they name nothing.
fn given(graph: Option<GraphSource>, files: Vec<File<Load>>) -> Option<Given<Load>> {
    if let Some(graph) = graph {
        return Some(Given::Graph {
            graph: Load::Graph(graph.into()),
            files,
        });
    }
    let mut files = files.into_iter();
    let first = files.next()?;
    Some(Given::Files {
        first,
        rest: files.collect(),
    })
}

actions!(
    bviz_app,
    [
        /// Copy bazelrc snippet
        CopyBazelrc,
    ]
);

/// Puts the bazelrc `bviz bazelrc` prints with no flags on the clipboard,
/// telling the user so in the window that asked.
fn copy_bazelrc(workspace: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    cx.write_to_clipboard(ClipboardItem::new_string(BazelrcArgs::default().bazelrc()));
    workspace.tell_briefly("Copied the bazelrc snippet to the clipboard.", cx);
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    // SAFETY: the first statement of main, before any other thread starts.
    unsafe { bviz_alloc::retain_freed_memory_across_load() };
    match Cli::parse().request() {
        Request::Bazelrc(args) => args.print(),
        Request::Open(sources) => {
            let named = Pending::start(move || Ok(Load::start_all(sources)));
            open_window(move |into, window, cx| open_named(named, into, window, cx));
        }
        Request::Empty => open_window(|_, _, _| {}),
    }
}

#[cfg(target_family = "wasm")]
use web::main;

fn open_window(arriving: impl FnOnce(WeakEntity<Workspace>, &mut Window, &mut App) + 'static) {
    bviz_ui_workspace::run_panels(
        "bviz",
        bviz_ui_workspace::WINDOW_SIZE,
        arriving,
        Some(PathOpener::new(open_paths)),
        vec![AppAction::new(CopyBazelrc, copy_bazelrc)],
        &[
            bviz_ui_graph::register_keys,
            bviz_ui_trace::register_keys,
            bviz_ui_console::register_keys,
            bviz_ui_execlog::register_keys,
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_components::LABEL_MAX_CHARS;
    use std::collections::HashMap;

    /// What the namespace of every action bviz declares begins with.
    const ACTION_NAMESPACE_PREFIX: &str = "bviz_";

    #[test]
    fn every_action_has_a_help_label_the_keys_help_fits() {
        let mut declared = 0;
        let mut unfit = Vec::new();
        for action in gpui::generate_list_of_all_registered_actions() {
            if !action.name.starts_with(ACTION_NAMESPACE_PREFIX) {
                continue;
            }
            declared += 1;
            let label = action.documentation.and_then(|doc| doc.lines().next());
            if label.is_none_or(|label| label.chars().count() > LABEL_MAX_CHARS) {
                unfit.push((action.name, label));
            }
        }
        assert_ne!(declared, 0, "no action of bviz's is registered");
        assert_eq!(unfit, []);
    }

    /// gpui registers every action by name as the application starts, and
    /// panics there on a name registered twice.
    #[test]
    fn no_two_actions_share_a_name() {
        let mut named: HashMap<&str, usize> = HashMap::new();
        for action in gpui::generate_list_of_all_registered_actions() {
            *named.entry(action.name).or_default() += 1;
        }
        let mut shared: Vec<&str> = named
            .into_iter()
            .filter(|&(_, count)| count > 1)
            .map(|(name, _)| name)
            .collect();
        shared.sort_unstable();
        assert_eq!(shared, Vec::<&str>::new());
    }

    fn view_kind<T: Panel>() -> (&'static str, ViewKind) {
        (std::any::type_name::<T>(), T::kind().view)
    }

    #[test]
    fn no_two_panels_share_a_view_kind() {
        let panels = [
            view_kind::<bviz_ui_graph::GraphPanel>(),
            view_kind::<bviz_ui_graph::ScatterPanel>(),
            view_kind::<bviz_ui_graph::ExclusivePanel>(),
            view_kind::<bviz_ui_trace::TracePanel>(),
            view_kind::<bviz_ui_trace::FlamePanel>(),
            view_kind::<bviz_ui_trace::StatsPanel>(),
            view_kind::<bviz_ui_console::ConsolePanel>(),
            view_kind::<bviz_ui_execlog::SpawnTimePanel>(),
        ];
        let mut claimed: HashMap<ViewKind, &'static str> = HashMap::new();
        for (panel, view) in panels {
            if let Some(first) = claimed.insert(view, panel) {
                panic!("{first} and {panel} both call themselves {}", view.as_str());
            }
        }
    }

    #[test]
    fn a_file_that_does_not_open_names_no_invocation_and_still_loads() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let missing = dir.join("missing.profile.gz");
        let args = ["bviz".into(), "--profile".into(), missing.into_os_string()];
        let request = Cli::try_parse_from(args).map(Cli::request);
        let Ok(Request::Open(Sources::Files { first, .. })) = request else {
            panic!("a profile opens as a file");
        };
        let file = Load::start(first);
        assert!(file.kind == Kind::Profile);
        assert_eq!(file.invocation, None);
        assert!(matches!(file.file, Load::Trace(_)));
    }
}
