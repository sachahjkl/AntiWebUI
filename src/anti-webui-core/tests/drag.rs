//! Grabbing and dragging. See `docs/design/23-drag-and-drop.md`.

use anti_webui_core::Ui;
use anti_webui_core::event::*;
use anti_webui_core::geometry::{PointF, Scale, SizeI};
use anti_webui_core::platform::{CursorIcon, DragOffer, PlatformRequest};
use anti_webui_core::prelude::*;
use anti_webui_core::scene::{Primitive, Scene};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn mount_at<V: IntoView>(scale: Scale, f: impl FnOnce() -> V) -> Ui {
    let mut ui = Ui::new(f);
    ui.handle_event(WindowEvent::Resized {
        size: SizeI::new(800, 600),
        scale,
    });
    ui.handle_event(WindowEvent::Focused(true));
    ui.update();
    ui
}

fn mount<V: IntoView>(f: impl FnOnce() -> V) -> Ui {
    mount_at(Scale::ONE, f)
}

fn at(x: f32, y: f32) -> PointF {
    PointF::new(x, y)
}

fn moved(ui: &mut Ui, x: f32, y: f32) {
    ui.handle_event(WindowEvent::PointerMoved {
        position: at(x, y),
        modifiers: Modifiers::default(),
    });
    ui.update();
}

fn button(ui: &mut Ui, x: f32, y: f32, pressed: bool) {
    ui.handle_event(WindowEvent::PointerButton {
        position: at(x, y),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::default(),
        time: 0,
    });
    ui.update();
}

fn press(ui: &mut Ui, x: f32, y: f32) {
    moved(ui, x, y);
    button(ui, x, y, true);
}

fn release(ui: &mut Ui, x: f32, y: f32) {
    button(ui, x, y, false);
}

fn key(ui: &mut Ui, key: Key, modifiers: Modifiers) {
    ui.handle_event(WindowEvent::Key(KeyEvent {
        key,
        text: None,
        modifiers,
        pressed: true,
        repeat: false,
    }));
    ui.update();
}

fn escape(ui: &mut Ui) {
    key(ui, Key::Named(NamedKey::Escape), Modifiers::default());
}

fn drag_layer(scene: &Scene) -> Option<&anti_webui_core::scene::Layer> {
    scene.primitives.iter().find_map(|p| match p {
        Primitive::Layer(l) if l.key == u64::MAX - 1 => Some(l),
        _ => None,
    })
}

fn boxed() -> Div {
    div()
        .absolute()
        .left(0.0)
        .top(0.0)
        .w(100.0)
        .h(100.0)
        .bg(rgb(0x808080))
}

type Log = Rc<RefCell<Vec<GrabPhase>>>;

fn grab_box(log: &Log, last: &Rc<Cell<(f32, f32)>>) -> Div {
    let (log, last) = (log.clone(), last.clone());
    boxed().on_grab(move |g| {
        log.borrow_mut().push(g.phase);
        last.set((g.total.x, g.total.y));
    })
}

#[test]
fn a_grab_starts_after_the_threshold_and_ends_when_released() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| grab_box(&log, &last));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 12.0, 10.0);
    assert!(log.borrow().is_empty(), "within the threshold");
    moved(&mut ui, 20.0, 10.0);
    assert_eq!(*log.borrow(), [GrabPhase::Start, GrabPhase::Move]);
    // From where it was pressed, so what the threshold ate isn't lost.
    assert_eq!(last.get(), (10.0, 0.0));
    release(&mut ui, 30.0, 40.0);
    assert_eq!(log.borrow().last(), Some(&GrabPhase::End));
    assert_eq!(last.get(), (20.0, 30.0));
}

#[test]
fn a_grab_follows_the_pointer_out_of_the_node_and_the_window() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| grab_box(&log, &last));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 500.0, -50.0);
    assert_eq!(last.get(), (490.0, -60.0));
}

#[test]
fn totals_and_speeds_are_logical_pixels() {
    let totals = Rc::new(Cell::new((0.0f32, 0.0f32)));
    let deltas = Rc::new(Cell::new((0.0f32, 0.0f32)));
    let (t, d) = (totals.clone(), deltas.clone());
    let mut ui = mount_at(Scale::from_f64(2.0), || {
        boxed().on_grab(move |g| {
            t.set((g.total.x, g.total.y));
            d.set((g.delta.x, g.delta.y));
        })
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    assert_eq!(totals.get(), (20.0, 0.0));
    moved(&mut ui, 60.0, 30.0);
    assert_eq!(totals.get(), (25.0, 10.0));
    assert_eq!(deltas.get(), (5.0, 10.0));
}

#[test]
fn with_no_threshold_it_starts_when_pressed() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| grab_box(&log, &last).grab_threshold(0.0));
    press(&mut ui, 10.0, 10.0);
    assert_eq!(*log.borrow(), [GrabPhase::Start], "it hasn't moved");
    moved(&mut ui, 11.0, 10.0);
    assert_eq!(*log.borrow(), [GrabPhase::Start, GrabPhase::Move]);
    release(&mut ui, 11.0, 10.0);
    assert_eq!(log.borrow().last(), Some(&GrabPhase::End));
}

#[test]
fn clicks_stay_clicks_until_it_moves() {
    let clicks = Rc::new(Cell::new(0));
    let c = clicks.clone();
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| grab_box(&log, &last).on_click(move |_| c.set(c.get() + 1)));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 12.0, 10.0);
    release(&mut ui, 12.0, 10.0);
    assert_eq!(clicks.get(), 1, "a little move is still a click");
    assert!(log.borrow().is_empty());

    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    release(&mut ui, 40.0, 10.0);
    assert_eq!(clicks.get(), 1, "it was grabbed");
}

#[test]
fn no_threshold_also_keeps_a_click_that_did_not_move() {
    let clicks = Rc::new(Cell::new(0));
    let c = clicks.clone();
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| {
        grab_box(&log, &last)
            .grab_threshold(0.0)
            .on_click(move |_| c.set(c.get() + 1))
    });
    press(&mut ui, 10.0, 10.0);
    release(&mut ui, 10.0, 10.0);
    assert_eq!(clicks.get(), 1);
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    release(&mut ui, 40.0, 10.0);
    assert_eq!(clicks.get(), 1);
}

#[test]
fn escape_cancels_once_and_nothing_follows() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let keys = Rc::new(Cell::new(0));
    let k = keys.clone();
    let clicks = Rc::new(Cell::new(0));
    let c = clicks.clone();
    let mut ui = mount(|| {
        grab_box(&log, &last)
            .focusable()
            .on_key_down(move |_| k.set(k.get() + 1))
            .on_click(move |_| c.set(c.get() + 1))
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    escape(&mut ui);
    assert_eq!(log.borrow().last(), Some(&GrabPhase::Cancel));
    assert_eq!(keys.get(), 0, "it was for what is held");
    let so_far = log.borrow().len();
    moved(&mut ui, 60.0, 10.0);
    release(&mut ui, 60.0, 10.0);
    assert_eq!(log.borrow().len(), so_far);
    assert_eq!(clicks.get(), 0);
    escape(&mut ui);
    assert_eq!(keys.get(), 1, "nothing is held any more");
}

#[test]
fn escape_before_it_starts_goes_to_the_node() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let keys = Rc::new(Cell::new(0));
    let k = keys.clone();
    let mut ui = mount(|| {
        grab_box(&log, &last)
            .focusable()
            .on_key_down(move |_| k.set(k.get() + 1))
    });
    press(&mut ui, 10.0, 10.0);
    escape(&mut ui);
    assert_eq!(keys.get(), 1);
    assert!(log.borrow().is_empty());
}

#[test]
fn losing_the_focus_cancels_it() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| grab_box(&log, &last));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    ui.handle_event(WindowEvent::Focused(false));
    assert_eq!(log.borrow().last(), Some(&GrabPhase::Cancel));
    assert!(!log.borrow().contains(&GrabPhase::End));
}

#[test]
fn the_innermost_one_takes_it_and_other_buttons_do_not() {
    let (outer, inner) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (o, i) = (outer.clone(), inner.clone());
    let mut ui = mount(|| {
        boxed()
            .on_grab(move |_| o.set(o.get() + 1))
            .child(div().w(50.0).h(50.0).on_grab(move |_| i.set(i.get() + 1)))
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    release(&mut ui, 40.0, 10.0);
    assert!(inner.get() > 0);
    assert_eq!(outer.get(), 0);

    let before = inner.get();
    ui.handle_event(WindowEvent::PointerButton {
        position: at(10.0, 10.0),
        button: PointerButton::Secondary,
        pressed: true,
        modifiers: Modifiers::default(),
        time: 0,
    });
    moved(&mut ui, 40.0, 10.0);
    assert_eq!(inner.get(), before);
}

#[test]
fn what_can_be_grabbed_does_not_start_selecting_text() {
    let mut ui = mount(|| {
        boxed()
            .on_grab(|_| {})
            .child(text("Some text that could be selected"))
    });
    press(&mut ui, 5.0, 8.0);
    moved(&mut ui, 60.0, 8.0);
    release(&mut ui, 60.0, 8.0);
    let selected = anti_webui_core::tree::with_doc(|d| d.state.selection.is_some());
    assert!(!selected);
}

#[test]
fn text_inputs_inside_of_it_keep_their_own_drag() {
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let mut ui = mount(|| grab_box(&log, &last).child(text_input().w(100.0).h(40.0)));
    press(&mut ui, 5.0, 8.0);
    moved(&mut ui, 60.0, 8.0);
    release(&mut ui, 60.0, 8.0);
    assert!(log.borrow().is_empty());
}

#[test]
fn a_node_that_goes_is_not_called_again() {
    let shown = signal(true);
    let (log, last) = (Log::default(), Rc::new(Cell::new((0.0, 0.0))));
    let (l, la) = (log.clone(), last.clone());
    let mut ui = mount(|| show(move || shown.get(), move || grab_box(&l, &la)));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    shown.set(false);
    ui.update();
    let so_far = log.borrow().len();
    moved(&mut ui, 60.0, 10.0);
    release(&mut ui, 60.0, 10.0);
    assert_eq!(log.borrow().len(), so_far);
}

#[test]
fn the_cursor_shows_it_being_held() {
    let mut ui = mount(|| boxed().cursor(CursorIcon::Grab).on_grab(|_| {}));
    moved(&mut ui, 10.0, 10.0);
    ui.take_requests();
    button(&mut ui, 10.0, 10.0, true);
    assert!(
        ui.take_requests()
            .contains(&PlatformRequest::SetCursor(CursorIcon::Grabbing))
    );
    release(&mut ui, 10.0, 10.0);
    assert!(
        ui.take_requests()
            .contains(&PlatformRequest::SetCursor(CursorIcon::Grab))
    );
}

#[derive(Clone, Debug, PartialEq)]
struct Card(u32);
#[derive(Clone, Debug, PartialEq)]
struct Other;

/// Something to drag at the top left, and something to take it to the right of it.
fn board(taken: &Rc<RefCell<Vec<(u32, DragAction)>>>) -> Div {
    let taken = taken.clone();
    div().child(boxed().draggable(|| Card(7))).child(
        div()
            .absolute()
            .left(200.0)
            .top(0.0)
            .w(100.0)
            .h(100.0)
            .drop_target(move |c: &Card, e: &DropEvent| {
                taken.borrow_mut().push((c.0, e.action));
            }),
    )
}

type Taken = Rc<RefCell<Vec<(u32, DragAction)>>>;

#[test]
fn a_value_goes_to_what_takes_its_type() {
    let taken = Taken::default();
    let mut ui = mount(|| board(&taken));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 40.0, 10.0);
    assert!(taken.borrow().is_empty());
    moved(&mut ui, 250.0, 50.0);
    release(&mut ui, 250.0, 50.0);
    assert_eq!(*taken.borrow(), [(7, DragAction::Move)]);
}

#[test]
fn dropping_elsewhere_does_nothing_and_says_so() {
    let taken = Taken::default();
    let result = Rc::new(Cell::new(None));
    let r = result.clone();
    let t = taken.clone();
    let mut ui = mount(|| {
        board(&t).child(
            div()
                .absolute()
                .left(0.0)
                .top(300.0)
                .w(100.0)
                .h(100.0)
                .draggable(|| Card(1))
                .on_drag_end(move |x| r.set(Some(x))),
        )
    });
    press(&mut ui, 10.0, 310.0);
    moved(&mut ui, 500.0, 500.0);
    release(&mut ui, 500.0, 500.0);
    assert!(taken.borrow().is_empty());
    assert_eq!(result.get(), Some(DragResult::Cancelled));
}

#[test]
fn it_says_what_became_of_it() {
    let result = Rc::new(Cell::new(None));
    let r = result.clone();
    let mut ui = mount(|| {
        div()
            .child(
                boxed()
                    .draggable(|| Card(1))
                    .on_drag_end(move |x| r.set(Some(x))),
            )
            .child(
                div()
                    .absolute()
                    .left(200.0)
                    .w(100.0)
                    .h(100.0)
                    .drop_target(|_: &Card, _| {}),
            )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 250.0, 50.0);
    release(&mut ui, 250.0, 50.0);
    assert_eq!(result.get(), Some(DragResult::Moved));
}

#[test]
fn other_types_are_not_taken_and_the_closest_that_takes_it_does() {
    let (outer, inner) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (o, i) = (outer.clone(), inner.clone());
    let mut ui = mount(|| {
        div().child(boxed().draggable(|| Card(1))).child(
            div()
                .absolute()
                .left(200.0)
                .w(100.0)
                .h(100.0)
                .drop_target(move |_: &Card, _| o.set(o.get() + 1))
                .child(
                    div()
                        .w(100.0)
                        .h(50.0)
                        .drop_target(move |_: &Other, _| i.set(i.get() + 1)),
                ),
        )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 250.0, 20.0);
    release(&mut ui, 250.0, 20.0);
    assert_eq!(outer.get(), 1, "the inner one doesn't take a Card");
    assert_eq!(inner.get(), 0);
}

#[test]
fn accepts_narrows_what_is_taken() {
    let taken = Rc::new(Cell::new(0));
    let t = taken.clone();
    let hovering = Rc::new(Cell::new(false));
    let h = hovering.clone();
    let mut ui = mount(|| {
        div().child(boxed().draggable(|| Card(1))).child(
            div()
                .absolute()
                .left(200.0)
                .w(100.0)
                .h(100.0)
                .drop_accepts(|c: &Card| c.0 != 1)
                .on_drag_hover(move |on| h.set(on))
                .drop_target(move |_: &Card, _| t.set(t.get() + 1)),
        )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 250.0, 20.0);
    assert!(!hovering.get());
    release(&mut ui, 250.0, 20.0);
    assert_eq!(taken.get(), 0);
}

#[test]
fn hover_stays_put_going_over_children_and_ends_with_the_drop() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let l = log.clone();
    let mut ui = mount(|| {
        div().child(boxed().draggable(|| Card(1))).child(
            div()
                .absolute()
                .left(200.0)
                .w(100.0)
                .h(100.0)
                .on_drag_hover(move |on| l.borrow_mut().push(on))
                .drop_target(|_: &Card, _| {})
                .child(div().w(50.0).h(50.0))
                .child(div().w(50.0).h(50.0)),
        )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 210.0, 10.0);
    moved(&mut ui, 210.0, 60.0);
    moved(&mut ui, 210.0, 10.0);
    assert_eq!(*log.borrow(), [true], "it doesn't flicker on its children");
    moved(&mut ui, 500.0, 500.0);
    moved(&mut ui, 210.0, 10.0);
    release(&mut ui, 210.0, 10.0);
    assert_eq!(*log.borrow(), [true, false, true, false]);
}

#[test]
fn over_says_where_it_is() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let mut ui = mount(|| {
        div().child(boxed().draggable(|| Card(3))).child(
            div()
                .absolute()
                .left(200.0)
                .w(100.0)
                .h(100.0)
                .drop_target(|_: &Card, _| {})
                .on_drag_over(move |c: &Card, e: &DropEvent| {
                    s.borrow_mut().push((c.0, e.local.x, e.fraction().y));
                }),
        )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 225.0, 25.0);
    assert_eq!(*seen.borrow(), [(3, 25.0, 0.25)]);
}

#[test]
fn dragged_tells_everyone_while_it_goes_on() {
    let current = signal(None::<Card>);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    let mut ui = mount(|| {
        effect(move || {
            current.set(dragged::<Card>());
            s.borrow_mut().push(dragged::<Other>().is_some());
        });
        boxed().draggable(|| Card(9))
    });
    press(&mut ui, 10.0, 10.0);
    assert_eq!(current.get_untracked(), None);
    moved(&mut ui, 50.0, 10.0);
    assert_eq!(current.get_untracked(), Some(Card(9)));
    assert!(!seen.borrow().contains(&true), "it isn't that type");
    release(&mut ui, 50.0, 10.0);
    assert_eq!(current.get_untracked(), None);
}

#[test]
fn control_copies_unless_it_may_not() {
    let taken = Taken::default();
    let t = taken.clone();
    let mut ui = mount(|| {
        let t2 = t.clone();
        div()
            .child(boxed().draggable(|| Card(1)))
            .child(
                div()
                    .absolute()
                    .left(0.0)
                    .top(200.0)
                    .w(100.0)
                    .h(100.0)
                    .draggable(|| DragData::new(Card(2)).actions(DragActions::Move)),
            )
            .child(div().absolute().left(200.0).w(100.0).h(100.0).drop_target(
                move |c: &Card, e: &DropEvent| {
                    t2.borrow_mut().push((c.0, e.action));
                },
            ))
    });
    let ctrl = Modifiers::COPY_DRAG;
    key(&mut ui, Key::Named(NamedKey::Control), ctrl);
    press(&mut ui, 10.0, 10.0);
    ui.handle_event(WindowEvent::PointerMoved {
        position: at(250.0, 50.0),
        modifiers: ctrl,
    });
    ui.handle_event(WindowEvent::PointerButton {
        position: at(250.0, 50.0),
        button: PointerButton::Primary,
        pressed: false,
        modifiers: ctrl,
        time: 0,
    });
    ui.update();
    ui.handle_event(WindowEvent::PointerMoved {
        position: at(10.0, 210.0),
        modifiers: ctrl,
    });
    ui.handle_event(WindowEvent::PointerButton {
        position: at(10.0, 210.0),
        button: PointerButton::Primary,
        pressed: true,
        modifiers: ctrl,
        time: 0,
    });
    ui.handle_event(WindowEvent::PointerMoved {
        position: at(250.0, 50.0),
        modifiers: ctrl,
    });
    ui.handle_event(WindowEvent::PointerButton {
        position: at(250.0, 50.0),
        button: PointerButton::Primary,
        pressed: false,
        modifiers: ctrl,
        time: 0,
    });
    assert_eq!(
        *taken.borrow(),
        [(1, DragAction::Copy), (2, DragAction::Move)]
    );
}

#[test]
fn escape_and_losing_the_focus_call_it_off() {
    let taken = Taken::default();
    for by_focus in [false, true] {
        let t = taken.clone();
        let mut ui = mount(|| board(&t));
        press(&mut ui, 10.0, 10.0);
        moved(&mut ui, 250.0, 50.0);
        if by_focus {
            ui.handle_event(WindowEvent::Focused(false));
        } else {
            escape(&mut ui);
        }
        release(&mut ui, 250.0, 50.0);
        assert!(taken.borrow().is_empty());
        let scene = ui.paint(Instant::now());
        assert!(drag_layer(&scene).is_none());
    }
}

#[test]
fn it_ends_being_pressed_and_hovered_when_it_starts_and_is_told_nothing_else() {
    let ups = Rc::new(Cell::new(0));
    let u = ups.clone();
    let log = Rc::new(RefCell::new(Vec::new()));
    let l = log.clone();
    let clicks = Rc::new(Cell::new(0));
    let c = clicks.clone();
    let mut ui = mount(|| {
        boxed()
            .draggable(|| Card(1))
            .on_pointer_up(move |_| u.set(u.get() + 1))
            .on_active(move |on| l.borrow_mut().push(on))
            .on_click(move |_| c.set(c.get() + 1))
    });
    press(&mut ui, 10.0, 10.0);
    assert_eq!(*log.borrow(), [true]);
    moved(&mut ui, 50.0, 10.0);
    assert_eq!(*log.borrow(), [true, false]);
    release(&mut ui, 50.0, 10.0);
    assert_eq!(ups.get(), 0);
    assert_eq!(clicks.get(), 0);
    let hovered = anti_webui_core::tree::with_doc(|d| !d.state.hovered.is_empty());
    assert!(hovered, "it is known what is under the pointer again");
}

#[test]
fn styles_follow_it() {
    let mut ui = mount(|| {
        div()
            .child(boxed().dragging(|s| s.opacity(0.25)).draggable(|| Card(1)))
            .child(
                div()
                    .absolute()
                    .left(200.0)
                    .w(100.0)
                    .h(100.0)
                    .drag_over(|s| s.bg(rgb(0xff0000)))
                    .drop_target(|_: &Card, _| {}),
            )
    });
    let red_shown = |ui: &mut Ui| {
        let scene = ui.paint(Instant::now());
        scene.primitives.iter().any(|p| match p {
            Primitive::Quad(q) => q.background == rgb(0xff0000),
            _ => false,
        })
    };
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    assert!(!red_shown(&mut ui));
    moved(&mut ui, 250.0, 10.0);
    assert!(red_shown(&mut ui));
    let source = anti_webui_core::tree::with_doc(|d| {
        d.state
            .dragging
            .and_then(|id| d.get(id))
            .map(|n| n.computed.opacity)
    });
    assert_eq!(source, Some(0.25));
    release(&mut ui, 250.0, 10.0);
    assert!(!red_shown(&mut ui));
    let still = anti_webui_core::tree::with_doc(|d| d.state.dragging.is_some());
    assert!(!still);
}

#[test]
fn the_preview_moves_without_painting_it_again() {
    let mut ui = mount(|| boxed().draggable(|| Card(1)));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 30.0);
    let first = ui.paint(Instant::now());
    let a = drag_layer(&first).expect("it shows").clone();
    assert_eq!((a.origin.x, a.origin.y), (40, 20), "the same place in it");
    assert_eq!(a.opacity, (0.75f32 * 255.0).round() / 255.0);
    moved(&mut ui, 71.0, 33.0);
    let second = ui.paint(Instant::now());
    let b = drag_layer(&second).expect("it shows");
    assert_eq!((b.origin.x, b.origin.y), (61, 23));
    assert!(std::sync::Arc::ptr_eq(&a.content, &b.content));
    assert_eq!(a.bounds, b.bounds);
}

#[test]
fn the_preview_goes_on_whole_pixels_at_any_scale() {
    let mut ui = mount_at(Scale::from_f64(1.8), || boxed().draggable(|| Card(1)));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.4, 30.6);
    let scene = ui.paint(Instant::now());
    let layer = drag_layer(&scene).expect("it shows");
    assert!(layer.is_pixel_aligned());
}

#[test]
fn the_preview_shows_what_was_there_when_the_node_goes() {
    let shown = signal(true);
    let taken = Rc::new(Cell::new(0));
    let t = taken.clone();
    let mut ui = mount(|| {
        div()
            .child(show(move || shown.get(), || boxed().draggable(|| Card(1))))
            .child(
                div()
                    .absolute()
                    .left(200.0)
                    .w(100.0)
                    .h(100.0)
                    .drop_target(move |_: &Card, _| t.set(t.get() + 1)),
            )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    shown.set(false);
    ui.update();
    let scene = ui.paint(Instant::now());
    assert!(drag_layer(&scene).is_some());
    moved(&mut ui, 250.0, 10.0);
    release(&mut ui, 250.0, 10.0);
    assert_eq!(taken.get(), 1, "it still gets where it goes");
}

#[test]
fn a_preview_of_your_own_shows_and_goes() {
    let live = Rc::new(Cell::new(false));
    let l = live.clone();
    let mut ui = mount(|| {
        boxed().draggable(|| Card(1)).drag_preview(move |c: &Card| {
            l.set(true);
            let l = l.clone();
            on_cleanup(move || l.set(false));
            div()
                .w(40.0)
                .h(20.0)
                .bg(rgb(0x00ff00))
                .child(text(format!("{}", c.0)))
        })
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    let scene = ui.paint(Instant::now());
    let layer = drag_layer(&scene).expect("it shows");
    assert!(live.get());
    assert_eq!(layer.opacity, 1.0);
    // In the box the pointer has its place as in the node: 10% across.
    assert_eq!(layer.origin.x, 50 - 4);
    assert_eq!(layer.origin.y, 10 - 2);
    assert!(
        layer
            .content
            .primitives
            .iter()
            .any(|p| matches!(p, Primitive::Quad(q) if q.background == rgb(0x00ff00)))
    );
    // Not as something of its own in the window as well.
    assert!(
        !scene
            .primitives
            .iter()
            .any(|p| matches!(p, Primitive::Quad(q) if q.background == rgb(0x00ff00)))
    );
    release(&mut ui, 50.0, 10.0);
    let scene = ui.paint(Instant::now());
    assert!(drag_layer(&scene).is_none());
    assert!(!live.get(), "it went away with it");
    let portals = anti_webui_core::tree::with_doc(|d| d.popups.len());
    assert_eq!(portals, 0);
}

#[test]
fn a_preview_of_your_own_inherits_from_what_is_dragged() {
    let mut ui = mount(|| {
        div().text_color(rgb(0x123456)).child(
            boxed()
                .draggable(|| Card(1))
                .drag_preview(|_: &Card| text("x")),
        )
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    let scene = ui.paint(Instant::now());
    let layer = drag_layer(&scene).expect("it shows");
    let color = layer.content.primitives.iter().find_map(|p| match p {
        Primitive::Glyphs(g) => Some(g.color),
        _ => None,
    });
    assert_eq!(color, Some(rgb(0x123456)));
}

#[test]
fn the_preview_keeps_nested_layers_apart_from_the_real_ones() {
    let mut ui = mount(|| {
        boxed()
            .draggable(|| Card(1))
            .child(div().w(50.0).h(50.0).translate(3.0, 3.0).bg(rgb(0x00ff00)))
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    let scene = ui.paint(Instant::now());
    let layer = drag_layer(&scene).expect("it shows");
    let inner: Vec<u64> = layer
        .content
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Layer(l) => Some(l.key),
            _ => None,
        })
        .collect();
    let outer: Vec<u64> = scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Layer(l) => Some(l.key),
            _ => None,
        })
        .collect();
    assert_eq!(inner.len(), 1);
    assert!(!outer.contains(&inner[0]));
}

/// Something tall in what scrolls, with a target at the bottom of the window.
fn scroller(top: Signal<Option<f32>>, r: NodeRef) -> Div {
    div()
        .absolute()
        .left(0.0)
        .top(0.0)
        .w(200.0)
        .h(200.0)
        .overflow_y_scroll()
        .node_ref(r)
        .on_scrolled(move |at| top.set(Some(at.y)))
        .child(
            div()
                .w(200.0)
                .h(1000.0)
                .draggable(|| Card(1))
                .drop_target(|_: &Card, _| {}),
        )
}

fn frames(ui: &mut Ui, from: Instant, seconds: f32) {
    let mut t = 0.0;
    while t < seconds {
        t += 0.016;
        ui.animate(from + Duration::from_secs_f32(t));
        ui.update();
    }
}

#[test]
fn it_scrolls_when_held_near_the_edge_but_not_at_once() {
    let top = signal(None);
    let r = node_ref();
    let mut ui = mount(|| scroller(top, r));
    press(&mut ui, 100.0, 100.0);
    moved(&mut ui, 100.0, 195.0);
    let now = Instant::now();
    assert!(ui.animating(), "it wants the frames to do so");
    frames(&mut ui, now, 0.1);
    assert_eq!(r.scroll_offset().y, 0.0, "it may have been grabbed there");
    frames(&mut ui, now + Duration::from_secs_f32(0.1), 0.5);
    assert!(r.scroll_offset().y > 50.0, "{}", r.scroll_offset().y);
}

#[test]
fn it_stops_scrolling_away_from_the_edge_and_at_the_end() {
    let top = signal(None);
    let r = node_ref();
    let mut ui = mount(|| scroller(top, r));
    press(&mut ui, 100.0, 100.0);
    moved(&mut ui, 100.0, 195.0);
    let now = Instant::now();
    frames(&mut ui, now, 0.4);
    moved(&mut ui, 100.0, 100.0);
    assert!(!ui.animating());
    let at = r.scroll_offset().y;
    frames(&mut ui, now + Duration::from_secs_f32(0.4), 0.5);
    assert_eq!(r.scroll_offset().y, at);

    moved(&mut ui, 100.0, 199.0);
    let later = now + Duration::from_secs_f32(1.0);
    frames(&mut ui, later, 6.0);
    assert_eq!(r.scroll_offset().y, 800.0);
    assert!(!ui.animating(), "there is nowhere to go");
}

#[test]
fn a_grab_can_ask_for_it_and_hears_about_it() {
    let r = node_ref();
    let moves = Rc::new(Cell::new(0));
    let m = moves.clone();
    let mut ui = mount(|| {
        div()
            .absolute()
            .left(0.0)
            .top(0.0)
            .w(200.0)
            .h(200.0)
            .overflow_y_scroll()
            .node_ref(r)
            .child(
                div()
                    .w(200.0)
                    .h(1000.0)
                    .grab_autoscroll()
                    .on_grab(move |g| {
                        if g.phase == GrabPhase::Move {
                            m.set(m.get() + 1);
                        }
                    }),
            )
    });
    press(&mut ui, 100.0, 100.0);
    moved(&mut ui, 100.0, 500.0);
    let before = moves.get();
    frames(&mut ui, Instant::now(), 0.5);
    assert!(r.scroll_offset().y > 0.0);
    assert!(moves.get() > before);
    release(&mut ui, 100.0, 500.0);
    assert!(!ui.animating());
}

#[test]
fn selecting_text_scrolls_when_the_pointer_has_left() {
    let r = node_ref();
    let mut ui = mount(|| {
        div()
            .absolute()
            .left(0.0)
            .top(0.0)
            .w(200.0)
            .h(100.0)
            .overflow_y_scroll()
            .node_ref(r)
            .child(text("word ".repeat(600)))
    });
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 10.0, 90.0);
    assert!(!ui.animating(), "it hasn't left");
    moved(&mut ui, 10.0, 150.0);
    assert!(ui.animating());
    frames(&mut ui, Instant::now(), 0.5);
    assert!(r.scroll_offset().y > 0.0);
    release(&mut ui, 10.0, 150.0);
    assert!(!ui.animating());
}

fn entered(ui: &mut Ui, x: f32, y: f32, mimes: &[&str]) {
    ui.handle_event(WindowEvent::DragEntered {
        position: at(x, y),
        mimes: mimes.iter().map(|m| m.to_string()).collect(),
        modifiers: Modifiers::default(),
    });
    ui.update();
}

fn requests_of(ui: &mut Ui) -> Vec<PlatformRequest> {
    ui.take_requests()
        .into_iter()
        .filter(|r| {
            matches!(
                r,
                PlatformRequest::StartDrag(_)
                    | PlatformRequest::AcceptDrag(_)
                    | PlatformRequest::ReceiveDrop(_)
                    | PlatformRequest::FinishDrop
                    | PlatformRequest::CancelDrag
                    | PlatformRequest::CloseDragIcon
            )
        })
        .collect()
}

#[test]
fn files_from_elsewhere_are_read_after_the_drop_and_then_told() {
    let got = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let g = got.clone();
    let hovering = Rc::new(Cell::new(false));
    let h = hovering.clone();
    let seen = signal(None::<ExternalDrag>);
    let mut ui = mount(|| {
        effect(move || seen.set(dragged::<ExternalDrag>()));
        boxed()
            .on_drag_hover(move |on| h.set(on))
            .on_drop_files(move |files, _| g.borrow_mut().extend_from_slice(files))
    });
    ui.set_platform_drag(true);
    entered(&mut ui, 500.0, 500.0, &["text/uri-list"]);
    assert!(seen.get_untracked().is_some_and(|x| x.has_files()));
    assert!(requests_of(&mut ui).is_empty(), "not over what takes it");
    ui.handle_event(WindowEvent::DragMoved {
        position: at(10.0, 10.0),
        modifiers: Modifiers::default(),
    });
    assert!(hovering.get());
    assert_eq!(
        requests_of(&mut ui),
        [PlatformRequest::AcceptDrag(Some((
            "text/uri-list".into(),
            DragAction::Move
        )))]
    );
    ui.handle_event(WindowEvent::DragDropped {
        position: at(10.0, 10.0),
        action: DragAction::Copy,
    });
    assert_eq!(
        requests_of(&mut ui),
        [PlatformRequest::ReceiveDrop("text/uri-list".into())]
    );
    assert!(got.borrow().is_empty(), "it hasn't been read");
    ui.handle_event(WindowEvent::DropData {
        mime: "text/uri-list".into(),
        bytes: b"file:///tmp/a%20b.png\r\n".to_vec(),
    });
    assert_eq!(*got.borrow(), [PathBuf::from("/tmp/a b.png")]);
    assert!(seen.get_untracked().is_none());
    assert!(!hovering.get());
}

#[test]
fn what_is_not_taken_from_elsewhere_is_refused() {
    let mut ui = mount(|| boxed().on_drop_files(|_, _| {}));
    ui.set_platform_drag(true);
    entered(&mut ui, 10.0, 10.0, &["image/png"]);
    assert!(requests_of(&mut ui).is_empty());
    ui.handle_event(WindowEvent::DragLeft);
    assert!(dragged::<ExternalDrag>().is_none());
}

#[test]
fn leaving_takes_back_what_was_accepted() {
    let mut ui = mount(|| boxed().on_drop_text(|_, _| {}));
    ui.set_platform_drag(true);
    entered(&mut ui, 10.0, 10.0, &["text/plain"]);
    assert_eq!(requests_of(&mut ui).len(), 1);
    ui.handle_event(WindowEvent::DragMoved {
        position: at(500.0, 500.0),
        modifiers: Modifiers::default(),
    });
    assert_eq!(requests_of(&mut ui), [PlatformRequest::AcceptDrag(None)]);
}

#[test]
fn text_arrives_without_carriage_returns() {
    let got = Rc::new(RefCell::new(String::new()));
    let g = got.clone();
    let mut ui = mount(|| boxed().on_drop_text(move |s, _| *g.borrow_mut() = s.to_owned()));
    ui.set_platform_drag(true);
    entered(&mut ui, 10.0, 10.0, &["UTF8_STRING"]);
    ui.handle_event(WindowEvent::DragDropped {
        position: at(10.0, 10.0),
        action: DragAction::Copy,
    });
    assert_eq!(
        requests_of(&mut ui).last(),
        Some(&PlatformRequest::ReceiveDrop("UTF8_STRING".into()))
    );
    ui.handle_event(WindowEvent::DropData {
        mime: "UTF8_STRING".into(),
        bytes: b"a\r\nb".to_vec(),
    });
    assert_eq!(*got.borrow(), "a\nb");
}

#[test]
fn what_failed_to_read_is_not_told() {
    let got = Rc::new(Cell::new(0));
    let g = got.clone();
    let mut ui = mount(|| boxed().on_drop_text(move |_, _| g.set(g.get() + 1)));
    ui.set_platform_drag(true);
    entered(&mut ui, 10.0, 10.0, &["text/plain"]);
    ui.handle_event(WindowEvent::DragDropped {
        position: at(10.0, 10.0),
        action: DragAction::Copy,
    });
    ui.handle_event(WindowEvent::DropData {
        mime: "text/plain".into(),
        bytes: Vec::new(),
    });
    assert_eq!(got.get(), 0);
    assert!(dragged::<ExternalDrag>().is_none());
}

#[test]
fn a_target_of_external_drag_itself_needs_nothing_read() {
    let got = Rc::new(Cell::new(0));
    let g = got.clone();
    let mut ui = mount(|| boxed().drop_target(move |_: &ExternalDrag, _| g.set(g.get() + 1)));
    ui.set_platform_drag(true);
    entered(&mut ui, 10.0, 10.0, &["image/png"]);
    ui.handle_event(WindowEvent::DragDropped {
        position: at(10.0, 10.0),
        action: DragAction::Copy,
    });
    assert_eq!(got.get(), 1);
    assert!(requests_of(&mut ui).contains(&PlatformRequest::FinishDrop));
}

/// What can leave the program on the left, and things to take it on the right.
fn offering(taken: &Rc<RefCell<Vec<String>>>, result: &Rc<Cell<Option<DragResult>>>) -> Div {
    let (a, b, r) = (taken.clone(), taken.clone(), result.clone());
    div()
        .child(
            boxed()
                .draggable(|| DragData::new(Card(1)).text("the card"))
                .on_drag_end(move |x| r.set(Some(x))),
        )
        .child(
            div()
                .absolute()
                .left(200.0)
                .w(100.0)
                .h(100.0)
                .drop_target(move |c: &Card, _| a.borrow_mut().push(format!("card {}", c.0))),
        )
        .child(
            div()
                .absolute()
                .left(400.0)
                .w(100.0)
                .h(100.0)
                .on_drop_text(move |s, _| b.borrow_mut().push(s.to_owned())),
        )
}

#[test]
fn what_can_leave_is_handed_to_the_platform() {
    let (taken, result) = Default::default();
    let mut ui = mount(|| offering(&taken, &result));
    ui.set_platform_drag(true);
    press(&mut ui, 10.0, 10.0);
    requests_of(&mut ui);
    moved(&mut ui, 50.0, 10.0);
    let requests = requests_of(&mut ui);
    let [PlatformRequest::StartDrag(DragOffer { items, actions })] = &requests[..] else {
        panic!("{requests:?}");
    };
    assert_eq!(*actions, DragActions::Both);
    assert!(
        items
            .iter()
            .any(|(m, d)| m == "text/plain" && &**d == b"the card")
    );

    // It shows on a surface of its own, not in the window.
    let frame = ui.paint_frame(Instant::now());
    assert!(drag_layer(&frame.window).is_none());
    let icon = frame.drag_icon.expect("a surface");
    assert_eq!((icon.scene.width, icon.scene.height), (100, 100));
    assert_eq!((icon.hotspot.x, icon.hotspot.y), (10, 10));
    assert!(drag_layer(&icon.scene).is_some());
}

#[test]
fn a_platform_drag_over_this_window_goes_to_what_takes_its_value() {
    let (taken, result): (Rc<RefCell<Vec<String>>>, _) = Default::default();
    let mut ui = mount(|| offering(&taken, &result));
    ui.set_platform_drag(true);
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    ui.handle_event(WindowEvent::PointerLeft);
    requests_of(&mut ui);
    entered(&mut ui, 250.0, 10.0, &["text/plain"]);
    assert_eq!(
        requests_of(&mut ui),
        // Any format of it will do: it isn't read.
        [PlatformRequest::AcceptDrag(Some((
            "text/plain;charset=utf-8".into(),
            DragAction::Move
        )))]
    );
    ui.handle_event(WindowEvent::DragDropped {
        position: at(250.0, 10.0),
        action: DragAction::Move,
    });
    assert_eq!(*taken.borrow(), ["card 1"]);
    assert_eq!(requests_of(&mut ui), [PlatformRequest::FinishDrop]);
    assert_eq!(result.get(), None, "it isn't over until the platform says");
    ui.handle_event(WindowEvent::DragSourceEnded(DragResult::Moved));
    assert_eq!(result.get(), Some(DragResult::Moved));
    assert_eq!(requests_of(&mut ui), [PlatformRequest::CloseDragIcon]);
    assert!(ui.paint_frame(Instant::now()).drag_icon.is_none());
}

#[test]
fn a_platform_drag_of_another_window_of_the_thread_keeps_its_value() {
    // A panel on a surface of its own and the bar it drops on: two windows, one thread.
    let (taken, result): (Rc<RefCell<Vec<String>>>, _) = Default::default();
    let mut source = mount(|| offering(&taken, &result));
    source.set_platform_drag(true);
    let (got, unused): (Rc<RefCell<Vec<String>>>, Rc<Cell<Option<DragResult>>>) =
        Default::default();
    let mut target = mount(|| offering(&got, &unused));
    target.set_platform_drag(true);

    press(&mut source, 10.0, 10.0);
    requests_of(&mut source);
    moved(&mut source, 50.0, 10.0);
    let requests = requests_of(&mut source);
    let [PlatformRequest::StartDrag(DragOffer { items, .. })] = &requests[..] else {
        panic!("{requests:?}");
    };
    let offered: Vec<&str> = items.iter().map(|(m, _)| m.as_str()).collect();
    source.handle_event(WindowEvent::PointerLeft);
    requests_of(&mut source);

    // The platform brings it over the other window, in the formats it was given.
    entered(&mut target, 250.0, 10.0, &offered);
    assert_eq!(
        requests_of(&mut target),
        // Taken by its value: no format is read.
        [PlatformRequest::AcceptDrag(Some((
            "text/plain;charset=utf-8".into(),
            DragAction::Move
        )))]
    );
    target.handle_event(WindowEvent::DragDropped {
        position: at(250.0, 10.0),
        action: DragAction::Move,
    });
    assert_eq!(*got.borrow(), ["card 1"], "the drop target has the card itself");
    assert_eq!(requests_of(&mut target), [PlatformRequest::FinishDrop]);

    // The window that started it ends it when the platform says.
    source.handle_event(WindowEvent::DragSourceEnded(DragResult::Moved));
    assert_eq!(result.get(), Some(DragResult::Moved));
}

#[test]
fn a_platform_drag_gives_its_own_text_without_reading() {
    let (taken, result): (Rc<RefCell<Vec<String>>>, _) = Default::default();
    let mut ui = mount(|| offering(&taken, &result));
    ui.set_platform_drag(true);
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    requests_of(&mut ui);
    entered(&mut ui, 450.0, 10.0, &["text/plain"]);
    ui.handle_event(WindowEvent::DragDropped {
        position: at(450.0, 10.0),
        action: DragAction::Copy,
    });
    assert_eq!(*taken.borrow(), ["the card"]);
    let requests = requests_of(&mut ui);
    assert!(
        !requests
            .iter()
            .any(|r| matches!(r, PlatformRequest::ReceiveDrop(_)))
    );
    ui.handle_event(WindowEvent::DragSourceEnded(DragResult::Copied));
    assert_eq!(result.get(), Some(DragResult::Copied));
}

#[test]
fn escape_asks_the_platform_to_end_it_and_losing_the_focus_does_not() {
    let (taken, result): (Rc<RefCell<Vec<String>>>, _) = Default::default();
    let mut ui = mount(|| offering(&taken, &result));
    ui.set_platform_drag(true);
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    requests_of(&mut ui);
    ui.handle_event(WindowEvent::Focused(false));
    assert!(requests_of(&mut ui).is_empty());
    assert_eq!(result.get(), None);
    escape(&mut ui);
    assert_eq!(requests_of(&mut ui), [PlatformRequest::CancelDrag]);
    ui.handle_event(WindowEvent::DragSourceEnded(DragResult::Cancelled));
    assert_eq!(result.get(), Some(DragResult::Cancelled));
}

#[test]
fn without_the_platform_it_stays_in_the_window_and_its_text_still_arrives() {
    let (taken, result): (Rc<RefCell<Vec<String>>>, _) = Default::default();
    let mut ui = mount(|| offering(&taken, &result));
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 450.0, 10.0);
    assert!(requests_of(&mut ui).is_empty());
    release(&mut ui, 450.0, 10.0);
    assert_eq!(*taken.borrow(), ["the card"]);
    assert_eq!(result.get(), Some(DragResult::Moved));
}

#[test]
fn a_type_only_drag_stays_in_the_window_even_with_the_platform() {
    let taken = Taken::default();
    let mut ui = mount(|| board(&taken));
    ui.set_platform_drag(true);
    press(&mut ui, 10.0, 10.0);
    moved(&mut ui, 50.0, 10.0);
    assert!(requests_of(&mut ui).is_empty());
    let scene = ui.paint(Instant::now());
    assert!(drag_layer(&scene).is_some());
}

/// Something that says when to start dragging by itself, once the pointer gets far enough.
fn starts_by_itself(data: fn() -> DragData) -> Div {
    boxed().on_pointer_move(move |e| {
        if e.position.x > 150.0 {
            start_drag(data(), (0.5, 0.5), || {
                div().w(20.0).h(10.0).bg(Color::from_rgb(0xff0000))
            });
        }
    })
}

#[test]
fn start_drag_carries_what_it_is_given_from_when_it_says_so() {
    let mut ui = mount(|| starts_by_itself(|| DragData::new(5u8).text("carried")));
    ui.set_platform_drag(true);
    press(&mut ui, 50.0, 30.0);
    moved(&mut ui, 60.0, 30.0);
    assert!(
        !ui.take_requests()
            .iter()
            .any(|r| matches!(r, PlatformRequest::StartDrag(_)))
    );
    moved(&mut ui, 160.0, 30.0);
    let offered = ui.take_requests().into_iter().find_map(|r| match r {
        PlatformRequest::StartDrag(DragOffer { items, .. }) => Some(items),
        _ => None,
    });
    let items = offered.expect("it started");
    assert!(
        items
            .iter()
            .any(|(mime, bytes)| mime == "text/plain" && &bytes[..] == b"carried")
    );
    assert_eq!(untrack(dragged::<u8>), Some(5));
    ui.handle_event(WindowEvent::DragSourceEnded(DragResult::Copied));
    ui.update();
    assert_eq!(untrack(dragged::<u8>), None);
}

#[test]
fn start_drag_shows_what_it_is_given_where_it_says() {
    let mut ui = mount(|| starts_by_itself(|| DragData::new(5u8)));
    press(&mut ui, 50.0, 30.0);
    moved(&mut ui, 160.0, 30.0);
    ui.update();
    let scene = ui.paint(Instant::now());
    let layer = drag_layer(&scene).expect("it shows");
    // Half of 20 across and half of 10 down from where the pointer is.
    assert_eq!((layer.origin.x, layer.origin.y), (150, 25));
}

#[test]
fn start_drag_does_nothing_without_a_button_held() {
    let mut ui = mount(|| {
        boxed().on_pointer_move(|_| {
            start_drag(DragData::new(5u8), (0.0, 0.0), || div().w(20.0).h(10.0));
        })
    });
    moved(&mut ui, 60.0, 30.0);
    ui.update();
    assert_eq!(untrack(dragged::<u8>), None);
    assert!(drag_layer(&ui.paint(Instant::now())).is_none());
}

fn pinched(ui: &mut Ui, scale: f32, phase: PinchPhase) {
    ui.handle_event(WindowEvent::Pinch {
        position: at(50.0, 30.0),
        scale,
        modifiers: Modifiers::default(),
        phase,
    });
}

#[test]
fn a_pinch_goes_to_what_was_under_the_pointer_until_it_is_over() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let (inner, outer) = (log.clone(), log.clone());
    let mut ui = mount(move || {
        div()
            .w(400.0)
            .h(300.0)
            .on_pinch(move |e| outer.borrow_mut().push(("outer", e.phase, e.scale)))
            .child(boxed().on_pinch(move |e| {
                inner.borrow_mut().push(("inner", e.phase, e.scale));
                if e.scale != 2.0 {
                    e.stop_propagation();
                }
            }))
    });
    pinched(&mut ui, 1.0, PinchPhase::Began);
    pinched(&mut ui, 2.0, PinchPhase::Moved);
    pinched(&mut ui, 0.5, PinchPhase::Moved);
    pinched(&mut ui, 1.0, PinchPhase::Ended);
    // Nothing goes on without another one starting.
    pinched(&mut ui, 3.0, PinchPhase::Moved);
    assert_eq!(
        *log.borrow(),
        [
            ("inner", PinchPhase::Began, 1.0),
            ("inner", PinchPhase::Moved, 2.0),
            ("outer", PinchPhase::Moved, 2.0),
            ("inner", PinchPhase::Moved, 0.5),
            ("inner", PinchPhase::Ended, 1.0),
        ]
    );
}

#[test]
fn a_pinch_says_where_it_is_in_the_node() {
    let seen = Rc::new(Cell::new(PointF::new(0.0, 0.0)));
    let kept = seen.clone();
    let mut ui = mount(move || {
        div()
            .w(400.0)
            .h(300.0)
            .pl(20.0)
            .child(div().w(100.0).h(100.0).on_pinch(move |e| kept.set(e.local)))
    });
    pinched(&mut ui, 1.0, PinchPhase::Began);
    assert_eq!(seen.get(), PointF::new(30.0, 30.0));
}
