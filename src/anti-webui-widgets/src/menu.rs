//! Menus.
//!
//! `menu_with` keeps what a menu does (where it hangs, the focus, the keys, the row that is lit,
//! submenus and closing when something is chosen) and leaves what it looks like to the program.
//! See `docs/design/33-customization.md`.

use crate::theme::{Theme, theme, themed};
use anti_webui_core::prelude::*;
use std::rc::Rc;
use std::time::Duration;

const SUBMENU_AFTER: Duration = Duration::from_millis(150);

/// A row of a menu.
#[derive(Clone)]
pub struct MenuItem {
    label: String,
    shortcut: Option<String>,
    checked: Option<Rc<dyn Fn() -> bool>>,
    disabled: Option<Rc<dyn Fn() -> bool>>,
    on_click: Option<Rc<dyn Fn()>>,
}

/// What a menu has.
#[derive(Clone)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
    Submenu(String, Rc<Vec<MenuEntry>>),
}

/// A row of a menu.
pub fn menu_item(label: impl Into<String>) -> MenuItem {
    MenuItem {
        label: label.into(),
        shortcut: None,
        checked: None,
        disabled: None,
        on_click: None,
    }
}

/// A line between what belongs together.
pub fn menu_separator() -> MenuEntry {
    MenuEntry::Separator
}

/// A row that opens another menu.
pub fn submenu(label: impl Into<String>, entries: Vec<MenuEntry>) -> MenuEntry {
    MenuEntry::Submenu(label.into(), Rc::new(entries))
}

impl MenuItem {
    /// What it says at the end of the row. It doesn't make the keys work.
    pub fn shortcut(mut self, keys: impl Into<String>) -> Self {
        self.shortcut = Some(keys.into());
        self
    }

    /// With a check in front of it while `f` returns true.
    pub fn checked_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.checked = Some(Rc::new(f));
        self
    }

    /// Doesn't react and looks that way while `f` returns true.
    pub fn disabled_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.disabled = Some(Rc::new(f));
        self
    }

    /// What it does.
    pub fn on_click(mut self, f: impl Fn() + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl From<MenuItem> for MenuEntry {
    fn from(item: MenuItem) -> Self {
        MenuEntry::Item(item)
    }
}

/// Where a menu hangs and how it takes the pointer.
#[derive(Clone, Copy)]
pub struct MenuOptions {
    placement: Placement,
    gap: i32,
    anchor: Option<NodeRef>,
    at: Option<Signal<Option<RectI>>>,
    match_anchor_width: bool,
    modal: bool,
    kind: PopupKind,
}

impl Default for MenuOptions {
    fn default() -> Self {
        Self {
            placement: Placement::default(),
            gap: 4,
            anchor: None,
            at: None,
            match_anchor_width: false,
            modal: true,
            kind: PopupKind::Menu,
        }
    }
}

impl MenuOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// The room between the menu and what it hangs from, in logical pixels. 4 by default.
    pub fn gap(mut self, gap: i32) -> Self {
        self.gap = gap;
        self
    }

    /// What it hangs from, rather than what it is a child of.
    pub fn anchor(mut self, anchor: NodeRef) -> Self {
        self.anchor = Some(anchor);
        self
    }

    /// Hangs from the rectangle in `at` as it is when the menu opens, in window physical pixels:
    /// where the pointer was pressed, for a context menu.
    pub fn at(mut self, at: Signal<Option<RectI>>) -> Self {
        self.at = Some(at);
        self
    }

    /// At least as wide as what it hangs from.
    pub fn match_anchor_width(mut self) -> Self {
        self.match_anchor_width = true;
        self
    }

    /// Lets the pointer go to the rest of the window while it is open. By default a menu takes the
    /// pointer, and a press outside of it closes it before it reaches anything.
    pub fn not_modal(mut self) -> Self {
        self.modal = false;
        self
    }

    /// The kind of popup the menu and its submenus are. `PopupKind::Menu` by default: the menu
    /// takes the pointer, as a menu does. `PopupKind::Bubble` draws it inside the window, without
    /// a grab: for a window that takes the presses around it itself, such as a full-screen layer
    /// opened for the menu, which never had a press of its own to grab with.
    pub fn kind(mut self, kind: PopupKind) -> Self {
        self.kind = kind;
        self
    }
}

/// What choosing a row does, and whether it can be chosen.
#[derive(Clone, Default)]
pub struct MenuChoice {
    run: Option<Rc<dyn Fn()>>,
    disabled: Option<Rc<dyn Fn() -> bool>>,
}

impl MenuChoice {
    /// Runs `f` when it is chosen, after the menu (and the menus it is in) are told to close.
    pub fn new(f: impl Fn() + 'static) -> Self {
        Self {
            run: Some(Rc::new(f)),
            disabled: None,
        }
    }

    /// Can't be chosen or lit while `f` returns true. The row is disabled for that time.
    pub fn disabled_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.disabled = Some(Rc::new(f));
        self
    }

    fn off(&self) -> bool {
        self.disabled.as_ref().is_some_and(|f| f())
    }
}

#[derive(Clone)]
enum Row {
    Item(MenuChoice),
    Submenu,
}

/// What the look of a menu gets to see, and what it can ask for. Reading it tracks it.
///
/// Rows are numbered by the look; the keys go through the numbers that were made rows with
/// [`item`](Self::item) or [`submenu`](Self::submenu), in order, and skip the rest.
#[derive(Clone, Copy)]
pub struct MenuState {
    open: Memo<bool>,
    close: Signal<Rc<dyn Fn()>>,
    close_all: Signal<Rc<dyn Fn()>>,
    lit: Signal<Option<usize>>,
    keyed: Signal<bool>,
    sub_open: Signal<Option<usize>>,
    hold: Signal<bool>,
    rows: Signal<Vec<Option<Row>>>,
    refs: Signal<Vec<NodeRef>>,
    timer: Signal<Option<Timeout>>,
    is_sub: bool,
    /// The kind of popup of this menu, and of its submenus (`MenuOptions::kind`).
    kind: PopupKind,
}

impl MenuState {
    fn new(open: Memo<bool>, close: Rc<dyn Fn()>, close_all: Rc<dyn Fn()>, is_sub: bool) -> Self {
        Self {
            open,
            close: signal(close),
            close_all: signal(close_all),
            lit: signal(None),
            keyed: signal(false),
            sub_open: signal(None),
            hold: signal(false),
            rows: signal(Vec::new()),
            refs: signal(Vec::new()),
            timer: signal(None),
            is_sub,
            kind: PopupKind::Menu,
        }
    }

    /// It should be up.
    pub fn open(&self) -> bool {
        self.open.get()
    }

    /// The row the keys or the pointer are on.
    pub fn lit(&self) -> Option<usize> {
        self.lit.get()
    }

    pub fn is_lit(&self, i: usize) -> bool {
        self.lit.get() == Some(i)
    }

    /// The lit row came from the keys, so it should show the focus.
    pub fn keyed(&self) -> bool {
        self.keyed.get()
    }

    /// The submenu of row `i` is open.
    pub fn is_open(&self, i: usize) -> bool {
        self.sub_open.get() == Some(i)
    }

    /// A submenu of another menu.
    pub fn is_submenu(&self) -> bool {
        self.is_sub
    }

    /// Keeps it there after it should go, while `f` returns true: for a look that fades out. Call
    /// it once, where the look is set up.
    pub fn hold_while(&self, f: impl Fn() -> bool + 'static) {
        let hold = self.hold;
        effect(move || hold.set_if_changed(f()));
    }

    /// Asks for this menu to close; a submenu goes back to its menu.
    pub fn close(&self) {
        let close = self.close.get_untracked();
        close();
    }

    /// Asks for this menu and the menus it is in to close.
    pub fn close_all(&self) {
        let close = self.close_all.get_untracked();
        close();
    }

    /// Makes `el` row `i`, which runs `choice` when it is chosen.
    pub fn item(&self, i: usize, el: Div, choice: MenuChoice) -> Div {
        let state = *self;
        let off = choice.disabled.clone();
        self.register(i, Row::Item(choice));
        let el = el
            .node_ref(self.ref_of(i))
            // A look may have said it is something more particular, like a menu item checkbox.
            .access(|a| match a.role {
                Some(_) => a,
                None => a.role(Role::MenuItem),
            })
            .on_hover(move |inside| state.hover(i, inside))
            .on_click(move |e| {
                e.stop_propagation();
                state.activate(i);
            });
        match off {
            Some(off) => el.disabled(move || off()),
            None => el,
        }
    }

    /// Makes `el` row `i`, which opens a menu that `setup` makes, as [`menu_with`] does, beside it:
    /// when it is clicked, when the pointer rests on it, and with Right or Enter. Left goes back.
    pub fn submenu<F>(&self, i: usize, el: Div, setup: impl FnOnce(MenuState) -> F) -> Div
    where
        F: Fn() -> Div + 'static,
    {
        let parent = *self;
        self.register(i, Row::Submenu);
        let anchor = self.ref_of(i);
        let child = MenuState::new(
            memo(move || parent.sub_open.get() == Some(i)),
            Rc::new(move || parent.sub_open.set(None)),
            parent.close_all.get_untracked(),
            true,
        );
        let child = MenuState {
            kind: parent.kind,
            ..child
        };
        let options = MenuOptions::new()
            .placement(Placement::RightStart)
            .anchor(anchor)
            .gap(2)
            .kind(parent.kind);
        el.node_ref(anchor)
            .access(move |a| a.role(Role::MenuItem).expanded(parent.is_open(i)))
            .on_hover(move |inside| parent.hover(i, inside))
            .on_click(move |e| {
                e.stop_propagation();
                parent.activate(i);
            })
            .child(build(child, options, setup))
    }

    fn register(&self, i: usize, row: Row) {
        self.rows.update_untracked(|rows| {
            if rows.len() <= i {
                rows.resize(i + 1, None);
            }
            rows[i] = Some(row);
        });
    }

    fn ref_of(&self, i: usize) -> NodeRef {
        self.refs.update_untracked(|refs| {
            while refs.len() <= i {
                refs.push(node_ref());
            }
            refs[i]
        })
    }

    fn row(&self, i: usize) -> Option<Row> {
        self.rows
            .with_untracked(|rows| rows.get(i).cloned().flatten())
    }

    fn can_be_lit(&self, i: usize) -> bool {
        match self.row(i) {
            Some(Row::Item(choice)) => !untrack(|| choice.off()),
            Some(Row::Submenu) => true,
            None => false,
        }
    }

    fn count(&self) -> usize {
        self.rows.with_untracked(Vec::len)
    }

    /// The next row that can be lit after `from`, down or up, around the ends.
    fn step(&self, from: Option<usize>, down: bool) -> Option<usize> {
        let count = self.count();
        let mut at = from;
        for _ in 0..count {
            let next = match at {
                Some(i) if down => (i + 1) % count,
                Some(i) => (i + count - 1) % count,
                None if down => 0,
                None => count - 1,
            };
            if self.can_be_lit(next) {
                return Some(next);
            }
            at = Some(next);
        }
        None
    }

    fn activate(&self, i: usize) {
        match self.row(i) {
            Some(Row::Item(choice)) if !untrack(|| choice.off()) => {
                self.close_all();
                if let Some(run) = &choice.run {
                    run();
                }
            }
            Some(Row::Submenu) => self.sub_open.set(Some(i)),
            _ => {}
        }
    }

    fn hover(&self, i: usize, inside: bool) {
        if let Some(running) = self.timer.get_untracked() {
            running.cancel();
            self.timer.set(None);
        }
        if !inside {
            return;
        }
        self.lit.set(Some(i));
        self.keyed.set(false);
        if self.sub_open.get_untracked() != Some(i) {
            let sub_open = self.sub_open;
            // A row that opens nothing closes what another one opened.
            let wait = set_timeout(SUBMENU_AFTER, move || sub_open.set(Some(i)));
            self.timer.set(Some(wait));
        }
    }

    fn key(&self, e: &KeyboardEvent) {
        // The menu it is a submenu of, or the button it hangs from, has no part in it.
        e.stop_propagation();
        let lit = self.lit.get_untracked();
        let next = match e.key {
            Key::Named(NamedKey::ArrowDown) => self.step(lit, true),
            Key::Named(NamedKey::ArrowUp) => self.step(lit, false),
            Key::Named(NamedKey::Home) => self.step(None, true),
            Key::Named(NamedKey::End) => (0..self.count()).rev().find(|&i| self.can_be_lit(i)),
            Key::Named(NamedKey::Enter | NamedKey::Space) => {
                e.prevent_default();
                if let Some(i) = lit {
                    self.activate(i);
                }
                return;
            }
            Key::Named(NamedKey::ArrowRight) => {
                e.prevent_default();
                if let Some(i) = lit
                    && matches!(self.row(i), Some(Row::Submenu))
                {
                    self.activate(i);
                }
                return;
            }
            Key::Named(NamedKey::ArrowLeft) if self.is_sub => {
                e.prevent_default();
                self.close();
                return;
            }
            Key::Named(NamedKey::Tab) => {
                e.prevent_default();
                self.close_all();
                return;
            }
            _ => return,
        };
        e.prevent_default();
        self.sub_open.set(None);
        self.keyed.set(true);
        self.lit.set(next);
        if let Some(row) = next.and_then(|i| self.refs.with_untracked(|r| r.get(i).copied())) {
            row.scroll_into_view();
        }
    }

    /// Forgets the rows of what was shown before: they are made again each time it opens.
    fn reset(&self) {
        self.rows.set(Vec::new());
        self.refs.set(Vec::new());
        self.lit.set(None);
        self.keyed.set(false);
        self.sub_open.set(None);
    }
}

fn build<F>(state: MenuState, options: MenuOptions, setup: impl FnOnce(MenuState) -> F) -> Show
where
    F: Fn() -> Div + 'static,
{
    let content = setup(state);
    // What the platform closed can't come back, so it goes at once and gets built again.
    let gone = signal(false);
    effect(move || {
        if state.open.get() {
            gone.set(false);
        }
    });
    show(
        move || state.open.get() || (state.hold.get() && !gone.get()),
        move || {
            state.reset();
            let mut surface = popup()
                .placement(options.placement)
                .gap(options.gap)
                .kind(options.kind)
                .on_dismiss(move || {
                    gone.set(true);
                    state.close();
                });
            if options.modal {
                surface = surface.modal();
            }
            if let Some(anchor) = options.anchor {
                surface = surface.anchor(anchor);
            }
            if let Some(rect) = options.at.and_then(|at| at.get_untracked()) {
                surface = surface.anchor_rect(rect);
            }
            if options.match_anchor_width {
                surface = surface.match_anchor_width();
            }
            let root = node_ref();
            let view = surface
                .child(
                    content()
                        .select_none()
                        .focusable()
                        .node_ref(root)
                        .access(|a| a.role(Role::Menu))
                        // What it is declared in has nothing to do with what is pressed here.
                        .on_pointer_down(|e| e.stop_propagation())
                        .on_click(|e| e.stop_propagation())
                        .on_key_down(move |e| state.key(e)),
                )
                .into_view();
            root.focus();
            view
        },
    )
}

/// A menu that looks like whatever is built. `setup` runs once with the state, and returns what
/// builds the menu each time it opens: a box that gets the focus and the keys, with rows made with
/// [`MenuState::item`] and [`MenuState::submenu`]. It hangs as `options` say, from what it is a
/// child of unless they say otherwise, while `open` says so. Choosing a row, pressing outside of it,
/// Escape and Tab call `on_dismiss`, which is the place to change `open`.
pub fn menu_with<F>(
    open: impl Fn() -> bool + 'static,
    on_dismiss: impl Fn() + 'static,
    options: MenuOptions,
    setup: impl FnOnce(MenuState) -> F,
) -> Show
where
    F: Fn() -> Div + 'static,
{
    let dismiss: Rc<dyn Fn()> = Rc::new(on_dismiss);
    let state = MenuState::new(memo(open), dismiss.clone(), dismiss, false);
    let state = MenuState {
        kind: options.kind,
        ..state
    };
    build(state, options, setup)
}

pub(crate) fn surface_sheet(t: &Theme) -> Sheet {
    Sheet::new(|s| {
        s.bg(t.overlay)
            .border_color(t.border)
            .text_color(t.text)
            .caret_color(t.text)
    })
}

fn row_sheet(t: &Theme) -> Sheet {
    Sheet::new(|s| s.cursor(CursorIcon::Pointer).text_color(t.text))
        .disabled(|s| s.text_color(t.text_disabled).cursor(CursorIcon::Default))
}

fn lit_row_sheet(t: &Theme) -> Sheet {
    Sheet::new(|s| {
        s.bg(t.accent)
            .text_color(t.on_accent)
            .cursor(CursorIcon::Pointer)
    })
    .disabled(|s| {
        s.bg(Color::TRANSPARENT)
            .text_color(t.text_disabled)
            .cursor(CursorIcon::Default)
    })
    .accent_background()
}

type SurfacePart = Rc<dyn Fn(Div, MenuState) -> Div>;
type RowPart = Rc<dyn Fn(Div, MenuState, usize) -> Div>;

/// Changes to the parts of the standard menu, and of its submenus. Each gets the part with its
/// standard styles on and returns it; what it says goes above the theme in every state.
#[derive(Clone, Default)]
pub struct MenuParts {
    surface: Option<SurfacePart>,
    row: Option<RowPart>,
}

impl MenuParts {
    pub fn new() -> Self {
        Self::default()
    }

    /// The box with the rows in it.
    pub fn surface(mut self, f: impl Fn(Div, MenuState) -> Div + 'static) -> Self {
        self.surface = Some(Rc::new(f));
        self
    }

    /// Each row that can be chosen or opens a submenu, with its number.
    pub fn row(mut self, f: impl Fn(Div, MenuState, usize) -> Div + 'static) -> Self {
        self.row = Some(Rc::new(f));
        self
    }
}

/// A row without what it does.
fn row_frame(s: MenuState, i: usize) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(6.0)
        .min_h(26.0)
        .px(8.0)
        .rounded(4.0)
        .sheet(move || {
            themed(if s.is_lit(i) {
                lit_row_sheet
            } else {
                row_sheet
            })
        })
}

fn check_slot(checked: Option<Rc<dyn Fn() -> bool>>) -> Div {
    // What it says is in the state of the row.
    div()
        .flex_none()
        .w(14.0)
        .access(|a| a.hidden())
        .child(text(move || {
            if checked.as_ref().is_some_and(|f| f()) {
                "✓"
            } else {
                ""
            }
            .to_owned()
        }))
}

/// The standard look of a menu of `entries`, for [`menu_with`].
fn standard(
    entries: Rc<Vec<MenuEntry>>,
    parts: MenuParts,
) -> impl FnOnce(MenuState) -> Box<dyn Fn() -> Div> {
    move |s| {
        // It fades in, and goes at once.
        let shown = crate::theme::fade(move || s.open());
        Box::new(move || {
            let rows = entries
                .iter()
                .enumerate()
                .map(|(i, entry)| standard_row(s, i, entry, &parts))
                .collect::<Vec<_>>();
            let surface = div()
                .flex()
                .flex_col()
                .min_w(160.0)
                .max_w(360.0)
                .max_h(480.0)
                .p(4.0)
                .rounded(8.0)
                .border(1.0)
                .shadow_lg()
                .overflow_y_scroll()
                .sheet(|| themed(surface_sheet))
                .dyn_style(move |st| st.opacity(shown.get().clamp(0.0, 1.0)))
                .children(rows);
            match &parts.surface {
                Some(f) => f(surface, s),
                None => surface,
            }
        })
    }
}

fn standard_row(s: MenuState, i: usize, entry: &MenuEntry, parts: &MenuParts) -> View {
    let row = |el: Div| match &parts.row {
        Some(f) => f(el, s, i),
        None => el,
    };
    match entry {
        MenuEntry::Separator => div()
            .h(1.0)
            .my(4.0)
            .mx(4.0)
            .flex_none()
            .dyn_style(|s| s.bg(theme().border))
            .into_view(),
        MenuEntry::Item(item) => {
            let checked = item.checked.clone();
            let el = row_frame(s, i)
                .access(move |a| match &checked {
                    Some(f) => a.role(Role::MenuItemCheckbox).checked(f()),
                    None => a,
                })
                .child(check_slot(item.checked.clone()))
                .child(div().flex_1().pr(16.0).child(text(item.label.clone())))
                .children(item.shortcut.clone().map(|keys| {
                    text(keys).text_size(12.0).dyn_style(move |st| {
                        if s.is_lit(i) {
                            st
                        } else {
                            st.text_color(theme().text_muted)
                        }
                    })
                }));
            let choice = MenuChoice {
                run: item.on_click.clone(),
                disabled: item.disabled.clone(),
            };
            s.item(i, row(el), choice).into_view()
        }
        MenuEntry::Submenu(label, inner) => {
            let el = row_frame(s, i)
                .child(check_slot(None))
                .child(div().flex_1().pr(16.0).child(text(label.clone())))
                .child(
                    div()
                        .access(|a| a.hidden())
                        .child(text("▸").text_size(10.0)),
                );
            s.submenu(i, row(el), standard(inner.clone(), parts.clone()))
                .into_view()
        }
    }
}

/// A menu that hangs from what it is a child of, while `open` says so. Pressing outside of it and
/// Escape call `on_dismiss`, as does choosing a row, which is the place to change `open`.
pub fn menu(
    open: impl Fn() -> bool + 'static,
    on_dismiss: impl Fn() + 'static,
    entries: Vec<MenuEntry>,
) -> Show {
    styled_menu(open, on_dismiss, entries, MenuParts::new())
}

/// A menu with the standard look, with its parts changed by `parts`.
pub fn styled_menu(
    open: impl Fn() -> bool + 'static,
    on_dismiss: impl Fn() + 'static,
    entries: Vec<MenuEntry>,
    parts: MenuParts,
) -> Show {
    menu_with(
        open,
        on_dismiss,
        MenuOptions::new(),
        standard(Rc::new(entries), parts),
    )
}

/// A button that opens a menu.
pub fn dropdown_button(label: impl IntoTextSource, entries: Vec<MenuEntry>) -> Div {
    let open = signal(false);
    crate::button(label)
        .on_click(move |_| open.update(|v| *v = !*v))
        .on_key_down(move |e| {
            if matches!(e.key, Key::Named(NamedKey::ArrowDown)) && !open.get_untracked() {
                e.prevent_default();
                open.set(true);
            }
        })
        .child(text("▾").dyn_style(|s| s.text_color(theme().text_muted)))
        .child(menu(move || open.get(), move || open.set(false), entries))
}

/// Opens a menu where `target` is pressed with the secondary button. `target` is what comes back,
/// and can be styled further.
pub fn context_menu(target: Div, entries: Vec<MenuEntry>) -> Div {
    let at = signal(None::<RectI>);
    target
        .on_pointer_down(move |e| {
            if e.button == Some(PointerButton::Secondary) {
                let (x, y) = (e.position.x as i32, e.position.y as i32);
                at.set(Some(RectI::new(x, y, 1, 1)));
            }
        })
        .child(menu_with(
            move || at.with(Option::is_some),
            move || at.set(None),
            MenuOptions::new().at(at),
            standard(Rc::new(entries), MenuParts::new()),
        ))
}
