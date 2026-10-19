use crate::Workspace;
use gpui::{Action, App, Context, Div, Global, InteractiveElement, Window};
use std::rc::Rc;

/// Something an app does in whichever of its windows asks for it, named by
/// the help label of its action.
#[derive(Clone)]
pub struct AppAction {
    action: Rc<dyn Action>,
    run: Rc<Run>,
    listen: Listen,
}

type Run = dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

/// Has a window's root run the action's `run` where its action is dispatched.
type Listen = fn(Div, Rc<Run>, &mut Context<Workspace>) -> Div;

impl AppAction {
    /// The action `action` names, running `run` in the window it is chosen
    /// in or dispatched to.
    pub fn new<A: Action>(
        action: A,
        run: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) -> Self {
        Self {
            action: Rc::new(action),
            run: Rc::new(run),
            listen: listen::<A>,
        }
    }

    pub(crate) fn action(&self) -> &dyn Action {
        &*self.action
    }

    pub(crate) fn run(
        &self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        (self.run)(workspace, window, cx);
    }

    /// `root`, running this action where it is dispatched.
    pub(crate) fn listen(&self, root: Div, cx: &mut Context<Workspace>) -> Div {
        (self.listen)(root, self.run.clone(), cx)
    }
}

fn listen<A: Action>(root: Div, run: Rc<Run>, cx: &mut Context<Workspace>) -> Div {
    root.on_action(cx.listener(move |workspace, _: &A, window, cx| run(workspace, window, cx)))
}

/// The actions of the app running, in the order its menus list them.
#[derive(Default)]
pub(crate) struct AppActions(pub(crate) Vec<AppAction>);

impl Global for AppActions {}

impl AppActions {
    pub(crate) fn of(cx: &App) -> &[AppAction] {
        cx.try_global::<Self>()
            .map_or(&[], |actions| actions.0.as_slice())
    }
}
