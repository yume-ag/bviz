# Keys and gestures

The Keys & Gestures sheet (`ui/components/src/keys_help.rs`) lists what a panel answers to
by reading the live keymap and the panel's gesture table. Anything bound or handled another
way is missing from it, and key text written by hand drifts from the binding. A new panel
follows every rule below.

## Keys

- **Every action gets a `///` doc on its `actions!` item.** The first line is its help label,
  read through `action_label`; menus, tooltips and the sheet all show that line. Phrase it
  tersely in sentence case, a verb and its object with no articles ("Show whole graph",
  "Negate chip"), and give the same operation the same words in every panel and in the
  gesture rows that do it too; a gesture's description takes the same style. It and every
  gesture's description and input stay within `LABEL_MAX_CHARS`/`GESTURE_MAX_CHARS`
  (`keys_help.rs`) so a row fits one line; the bviz binary's test checks every action.
- **Every key context is a `KeyScope`** (`ui/components/src/key_scope.rs`) carrying the heading
  its bindings are listed under. A field or state inside a panel is
  `KeyScope::within(&parent, Nesting::…, id, label)`. Pass each scope to `declare_scopes` in
  the crate's `register_keys`: the sheet lists bindings only under a scope it knows. A panel's
  `PanelKind::key_context` is its scope, and the scope's label is the panel's name. A window
  frame's context is `KeyScope::frame`: its bindings reach everywhere in the window, and the
  sheet lists them with the unscoped ones.
- **Bind through the shared helpers**: `zoom_key_bindings(scope)`,
  `find_key_bindings(scope, Find)`, and `text_input::unless_typing(scope)` as the predicate of
  every unmodified key that types a character (a letter, digit, punctuation or space), so
  typing in any field of the panel wins over it. A key that types nothing (Escape, an arrow,
  Enter) needs no guard: a field that answers it binds it in its own, deeper scope. Spell a
  command modifier `secondary-`, never a `cmd-`/`ctrl-` pair, in bindings and in the tests
  that press them; `keymap_probe` reads what a crate's bindings dispatch in a context stack.
- **Bind an action once per panel.** An action reaching the panel from two scopes is one
  binding whose predicate joins them (`a || b`), which dispatches at the same depth as two
  would; two bindings filed under different scopes list as a row under each.
- **A menu row an action also does is `MenuItem::action`**, which reads both its text and its
  keys from the action, so the row cannot word the operation apart from the action's label.
- **Never write key text by hand.** A button takes `Button::keys_for` (or `keys_for_focused`,
  `keys_in`), whose tooltip reads the action's label unless `tooltip` overrides it; anything
  else uses `key_text_in`. Each resolves the binding in effect for a focus handle, and only
  for one drawn in the last frame. Where that frame lags — an overlay just opened, a state
  the frame being drawn adds — name the context stack with `key_text_within`. A control
  acting on a particular panel names keys exactly when they act on that panel, which
  `panel::keys_focus` decides for every such control.

## Gestures

- **A panel's pointer handlers dispatch through one `Gestures` table**
  (`ui/components/src/gesture.rs`): `Entry::new(input, place, does, intent)` per row, and each
  handler calls `GESTURES.dispatch(Pointer::…(e), |intent| …)`, returning `false` to decline
  (the pointer is not over the entry's place). A pinch needs an `Input::PINCH` entry and an
  `.on_pinch` handler. Change behaviour in the table and its dispatch arm together.
- **Expose it**: `Panel::gestures` returns `GESTURES.rows().collect()`; window chrome lists its
  tables in `chrome_gestures` (`ui/workspace/src/gestures.rs`).
- **Test it**: `assert_eq!(GESTURES.unreachable(), [])` fails on a row an earlier one shadows,
  and `assert_eq!(GESTURES.overlong(), [])` on one too long for the sheet's line.
- Hint text naming a gesture comes from its entry (`Gesture::text`), never a literal.
- Scroll and zoom amounts go through `scroll_pixels`, `wheel_zoom_factor` and `pinch_factor`,
  so every canvas moves the same distance for the same input.
- A handler that stops a press from propagating must focus its panel itself, or the panel's
  keys go to another panel and the sheet stays up.

## Across panels

| Operation | Keys | Gesture |
|---|---|---|
| Pan | — | scroll on either axis, drag; shift-scroll pans across |
| Zoom | `zoom_key_bindings` | secondary-scroll, pinch, about the pointer |
| Show whole view | `0` | double-click empty space, whose first press was also clear |
| Find | `find_key_bindings` | — |
| Show legend | `legend_key_bindings` | click the legend's toggle (`legend_gestures`) |
| Focus the query field | `q` | — |
| History | Alt-← / Alt-→ ("Go back" / "Go forward") | mouse back / forward button |
| List | ↑ / ↓ move the aim, Enter acts on the aimed row where moving does not, Esc closes the list or clears its choice | a click acts on the row |
| Dialog | Enter answers, Esc cancels; in a choice dialog ↑ / ↓ aim at an item and ← / → step through its choices | a click on a button or a choice |
| Escape | closes the topmost transient (completion, field, palette, menu, popup card), else clears the selection, else puts away a lens such as the why paths | — |
| Close | secondary-W the panel, secondary-Shift-W the window | — |

Secondary is Ctrl on Linux and Windows and ⌘ on macOS; a gesture's `Held::SECONDARY` also
accepts Control on macOS. A transient takes Escape in a key context of its own while open, and
hands focus back to where it came from as it closes. A window's dialog instead keeps focus
while it stands, whatever changes beneath it, and once answered hands focus to the dialog
waiting next, else to the active panel (`Workspace::content_focus`). A menu lets a modified
keystroke through to the panel.
