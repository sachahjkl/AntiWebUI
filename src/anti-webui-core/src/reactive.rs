//! Fine-grained reactivity in the style of SolidJS.
//!
//! Propagation uses the three-color (Clean / Check / Dirty) algorithm: writes push marks
//! downstream, and computations pull fresh values from upstream memos before deciding whether
//! they really need to re-run. This keeps updates glitch-free and minimal.
//!
//! The runtime is thread-local. Values are stored outside of the runtime borrow so user
//! closures can freely read and write other reactive values while running.

use slotmap::{SlotMap, new_key_type};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::marker::PhantomData;
use std::rc::Rc;

new_key_type! {
    /// Identifies a reactive node (signal, memo, effect or root owner).
    pub struct ReactiveId;
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum State {
    Clean,
    Check,
    Dirty,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Signal,
    Memo,
    Effect,
    Root,
}

type Value = Rc<RefCell<Option<Box<dyn Any>>>>;
/// Runs a computation. Returns whether its value changed.
type Compute = Rc<dyn Fn(&Value) -> bool>;

struct Node {
    kind: Kind,
    state: State,
    value: Value,
    compute: Option<Compute>,
    sources: Vec<ReactiveId>,
    /// While it runs: how many of `sources` it has read so far. Those after are stale unless read
    /// again.
    read: usize,
    observers: Vec<ReactiveId>,
    owner: Option<ReactiveId>,
    owned: Vec<ReactiveId>,
    cleanups: Vec<Box<dyn FnOnce()>>,
    depth: u32,
    /// The window it was made in, which it runs in (see `crate::window`).
    window: u64,
}

#[derive(Default)]
struct Runtime {
    nodes: SlotMap<ReactiveId, Node>,
    observer: Option<ReactiveId>,
    owner: Option<ReactiveId>,
    batch_depth: u32,
    /// Effects to run: the shallowest first, since re-running a parent disposes its children, and
    /// in the order they were marked among equals.
    pending: BinaryHeap<Reverse<(u32, u64, ReactiveId)>>,
    marked: u64,
    flushing: bool,
}

/// The runtime goes when its thread ends, after the windows and roots of the thread disposed of
/// what they owned. The nodes left are dropped with a panic contained: their values and cleanups
/// may reach thread-locals that are gone by then, or a connection that failed, and a panic out of
/// the destructor of a thread-local aborts the program.
impl Drop for Runtime {
    fn drop(&mut self) {
        let nodes = std::mem::take(&mut self.nodes);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(nodes)));
    }
}

thread_local! {
    static RUNTIME: RefCell<Runtime> = RefCell::new(Runtime::default());
    static UNTRACK: Cell<bool> = const { Cell::new(false) };
}

fn with_rt<R>(f: impl FnOnce(&mut Runtime) -> R) -> R {
    RUNTIME.with(|rt| f(&mut rt.borrow_mut()))
}

impl Runtime {
    fn create(
        &mut self,
        kind: Kind,
        value: Option<Box<dyn Any>>,
        compute: Option<Compute>,
    ) -> ReactiveId {
        let owner = self.owner;
        let (depth, window) = match owner.and_then(|o| self.nodes.get(o)) {
            Some(o) => (o.depth + 1, o.window),
            None => (0, crate::window::active_id()),
        };
        let id = self.nodes.insert(Node {
            kind,
            state: if compute.is_some() {
                State::Dirty
            } else {
                State::Clean
            },
            value: Rc::new(RefCell::new(value)),
            compute,
            sources: Vec::new(),
            read: 0,
            observers: Vec::new(),
            owner,
            owned: Vec::new(),
            cleanups: Vec::new(),
            depth,
            window,
        });
        if let Some(owner) = owner
            && let Some(o) = self.nodes.get_mut(owner)
        {
            o.owned.push(id);
        }
        id
    }

    fn track(&mut self, source: ReactiveId) {
        if UNTRACK.with(|u| u.get()) {
            return;
        }
        let Some(observer) = self.observer else {
            return;
        };
        if observer == source || !self.nodes.contains_key(observer) {
            return;
        }
        // An effect mostly reads what it read last time, in the same order: nothing to change then.
        let obs = &mut self.nodes[observer];
        let read = obs.read;
        if obs.sources.get(read) == Some(&source) {
            obs.read += 1;
            return;
        }
        if obs.sources[..read].contains(&source) {
            return;
        }
        obs.read += 1;
        if let Some(i) = obs.sources[read..].iter().position(|s| *s == source) {
            obs.sources.swap(read, read + i);
            return;
        }
        obs.sources.push(source);
        let last = obs.sources.len() - 1;
        obs.sources.swap(read, last);
        if let Some(src) = self.nodes.get_mut(source) {
            src.observers.push(observer);
        }
    }

    /// Marks the observers of `id` as `state`, and everything further downstream as `Check`.
    fn mark_observers(&mut self, id: ReactiveId, state: State) {
        let observers = match self.nodes.get(id) {
            Some(n) => n.observers.clone(),
            None => return,
        };
        for o in observers {
            let Some(node) = self.nodes.get_mut(o) else {
                continue;
            };
            if node.state < state {
                let was_clean = node.state == State::Clean;
                node.state = state;
                if node.kind == Kind::Effect {
                    let depth = node.depth;
                    self.marked += 1;
                    self.pending.push(Reverse((depth, self.marked, o)));
                }
                if was_clean {
                    self.mark_observers(o, State::Check);
                }
            }
        }
    }

    fn unlink_sources(&mut self, id: ReactiveId) {
        self.nodes[id].read = 0;
        self.unlink_unread_sources(id);
    }

    /// Forgets the sources that the computation didn't read in its last run.
    fn unlink_unread_sources(&mut self, id: ReactiveId) {
        let node = &mut self.nodes[id];
        let read = node.read;
        if read == node.sources.len() {
            return;
        }
        let stale = node.sources.split_off(read);
        for s in stale {
            if let Some(src) = self.nodes.get_mut(s) {
                src.observers.retain(|o| *o != id);
            }
        }
    }
}

/// Disposes the nodes owned by `id` and collects its cleanups (without running them).
fn take_owned(id: ReactiveId) -> Vec<Box<dyn FnOnce()>> {
    let mut cleanups = Vec::new();
    let owned = with_rt(|rt| {
        rt.nodes
            .get_mut(id)
            .map(|n| std::mem::take(&mut n.owned))
            .unwrap_or_default()
    });
    // Dispose children in reverse creation order so later nodes go first.
    for child in owned.into_iter().rev() {
        cleanups.extend(dispose_collect(child));
    }
    with_rt(|rt| {
        if let Some(n) = rt.nodes.get_mut(id) {
            cleanups.extend(n.cleanups.drain(..).rev());
        }
    });
    cleanups
}

fn dispose_collect(id: ReactiveId) -> Vec<Box<dyn FnOnce()>> {
    if !with_rt(|rt| rt.nodes.contains_key(id)) {
        return Vec::new();
    }
    let cleanups = take_owned(id);
    with_rt(|rt| {
        rt.unlink_sources(id);
        let node = rt.nodes.remove(id).unwrap();
        for o in node.observers {
            if let Some(obs) = rt.nodes.get_mut(o) {
                obs.sources.retain(|s| *s != id);
            }
        }
        if let Some(owner) = node.owner
            && let Some(o) = rt.nodes.get_mut(owner)
        {
            o.owned.retain(|c| *c != id);
        }
    });
    cleanups
}

fn dispose(id: ReactiveId) {
    let _window = crate::window::Enter::new(window_of(id));
    for cleanup in dispose_collect(id) {
        cleanup();
    }
}

/// The window a node runs in; 0 if it is gone.
fn window_of(id: ReactiveId) -> u64 {
    with_rt(|rt| rt.nodes.get(id).map_or(0, |n| n.window))
}

/// Whether something reading a value now would be subscribed to it.
pub(crate) fn is_tracking() -> bool {
    !UNTRACK.with(|u| u.get()) && with_rt(|rt| rt.observer.is_some())
}

/// Re-runs the computation `id`.
fn run(id: ReactiveId) {
    let _window = crate::window::Enter::new(window_of(id));
    for cleanup in take_owned(id) {
        cleanup();
    }
    let prepared = with_rt(|rt| {
        let node = rt.nodes.get_mut(id)?;
        node.read = 0;
        let compute = node.compute.clone()?;
        let value = node.value.clone();
        let prev = (rt.observer.replace(id), rt.owner.replace(id));
        Some((compute, value, prev))
    });
    let Some((compute, value, (prev_observer, prev_owner))) = prepared else {
        return;
    };
    let untrack_prev = UNTRACK.with(|u| u.replace(false));
    let changed = compute(&value);
    UNTRACK.with(|u| u.set(untrack_prev));
    with_rt(|rt| {
        rt.observer = prev_observer;
        rt.owner = prev_owner;
        if !rt.nodes.contains_key(id) {
            return;
        }
        rt.unlink_unread_sources(id);
        let node = &mut rt.nodes[id];
        node.state = State::Clean;
        if changed && node.kind == Kind::Memo {
            let observers = node.observers.clone();
            for o in observers {
                if let Some(obs) = rt.nodes.get_mut(o) {
                    obs.state = State::Dirty;
                }
            }
        }
    });
}

/// Brings `id` up to date, re-running it only if an upstream value really changed.
fn update_if_necessary(id: ReactiveId) {
    let (state, sources) =
        match with_rt(|rt| rt.nodes.get(id).map(|n| (n.state, n.sources.clone()))) {
            Some(v) => v,
            None => return,
        };
    if state == State::Check {
        for s in sources {
            let is_memo = with_rt(|rt| {
                rt.nodes
                    .get(s)
                    .map(|n| n.kind == Kind::Memo)
                    .unwrap_or(false)
            });
            if is_memo {
                update_if_necessary(s);
            }
            if with_rt(|rt| {
                rt.nodes
                    .get(id)
                    .map(|n| n.state == State::Dirty)
                    .unwrap_or(false)
            }) {
                break;
            }
        }
    }
    let state = with_rt(|rt| rt.nodes.get(id).map(|n| n.state));
    match state {
        Some(State::Dirty) => run(id),
        Some(_) => with_rt(|rt| rt.nodes[id].state = State::Clean),
        None => {}
    }
}

fn flush() {
    if with_rt(|rt| rt.flushing || rt.batch_depth > 0) {
        return;
    }
    with_rt(|rt| rt.flushing = true);
    let mut iterations = 0usize;
    loop {
        let next = with_rt(|rt| {
            while let Some(Reverse((_, _, id))) = rt.pending.pop() {
                if rt.nodes.contains_key(id) {
                    return Some(id);
                }
            }
            None
        });
        let Some(id) = next else { break };
        iterations += 1;
        assert!(
            iterations < 1_000_000,
            "reactive effects did not settle; an effect is probably writing a signal it reads"
        );
        update_if_necessary(id);
    }
    with_rt(|rt| {
        rt.pending.clear();
        rt.flushing = false;
    });
}

/// Runs `f`, deferring effect execution until it returns.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    with_rt(|rt| rt.batch_depth += 1);
    let r = f();
    with_rt(|rt| rt.batch_depth -= 1);
    flush();
    r
}

/// Runs `f` without tracking any reactive reads.
pub fn untrack<R>(f: impl FnOnce() -> R) -> R {
    let prev = UNTRACK.with(|u| u.replace(true));
    let r = f();
    UNTRACK.with(|u| u.set(prev));
    r
}

/// Registers `f` to run when the current owner is disposed or re-run.
pub fn on_cleanup(f: impl FnOnce() + 'static) {
    let leaked = with_rt(|rt| {
        if let Some(owner) = rt.owner
            && let Some(n) = rt.nodes.get_mut(owner)
        {
            n.cleanups.push(Box::new(f));
            return None;
        }
        Some(f)
    });
    // Without an owner nothing will ever run the cleanup; that is the caller's choice.
    drop(leaked);
}

/// A scope that owns reactive nodes, UI nodes and cleanups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Owner(ReactiveId);

impl Owner {
    /// The owner that is currently collecting new nodes, if any.
    pub fn current() -> Option<Owner> {
        with_rt(|rt| rt.owner.map(Owner))
    }

    /// Runs `f` with `self` as the current owner (and no tracking observer), in its window.
    pub fn with<R>(self, f: impl FnOnce() -> R) -> R {
        let _window = crate::window::Enter::new(window_of(self.0));
        let prev = with_rt(|rt| (rt.owner.replace(self.0), rt.observer.take()));
        let r = f();
        with_rt(|rt| {
            rt.owner = prev.0;
            rt.observer = prev.1;
        });
        r
    }

    /// Disposes the owner, everything it owns, and runs its cleanups.
    pub fn dispose(self) {
        dispose(self.0);
    }

    pub fn is_alive(self) -> bool {
        with_rt(|rt| rt.nodes.contains_key(self.0))
    }
}

/// Creates a new owner as a child of the current one and runs `f` inside it.
#[doc(hidden)]
pub fn create_child_owner<R>(f: impl FnOnce() -> R) -> (R, Owner) {
    let id = with_rt(|rt| rt.create(Kind::Root, None, None));
    let owner = Owner(id);
    (owner.with(f), owner)
}

/// Creates a detached root owner and runs `f` inside it.
pub fn create_root<R>(f: impl FnOnce() -> R) -> (R, Owner) {
    let id = with_rt(|rt| {
        let prev = rt.owner.take();
        let id = rt.create(Kind::Root, None, None);
        rt.owner = prev;
        id
    });
    let owner = Owner(id);
    (owner.with(f), owner)
}

/// A readable and writable reactive value. Cheap to copy.
pub struct Signal<T> {
    id: ReactiveId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Signal<T> {}

impl<T> std::fmt::Debug for Signal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signal({:?})", self.id)
    }
}

impl<T> PartialEq for Signal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<T> Eq for Signal<T> {}

/// Creates a signal owned by the current owner.
pub fn signal<T: 'static>(value: T) -> Signal<T> {
    let id = with_rt(|rt| rt.create(Kind::Signal, Some(Box::new(value)), None));
    Signal {
        id,
        _marker: PhantomData,
    }
}

fn value_of(id: ReactiveId) -> Value {
    with_rt(|rt| match rt.nodes.get(id) {
        Some(n) => n.value.clone(),
        None => panic!("accessed a reactive value after its owner was disposed"),
    })
}

impl<T: 'static> Signal<T> {
    /// Reads the value by reference, tracking the read.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        with_rt(|rt| rt.track(self.id));
        self.with_untracked(f)
    }

    pub fn with_untracked<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        let value = value_of(self.id);
        let value = value.borrow();
        f(value.as_ref().unwrap().downcast_ref::<T>().unwrap())
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }

    pub fn get_untracked(&self) -> T
    where
        T: Clone,
    {
        self.with_untracked(T::clone)
    }

    /// Replaces the value and notifies observers.
    pub fn set(&self, value: T) {
        self.update(|v| *v = value);
    }

    /// Replaces the value only if it differs, notifying observers only then.
    pub fn set_if_changed(&self, value: T)
    where
        T: PartialEq,
    {
        if self.with_untracked(|v| *v != value) {
            self.set(value);
        }
    }

    /// Mutates the value in place and notifies observers.
    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let r = self.update_untracked(f);
        self.notify();
        r
    }

    /// Mutates the value without notifying observers.
    pub fn update_untracked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let value = value_of(self.id);
        let mut value = value.borrow_mut();
        f(value.as_mut().unwrap().downcast_mut::<T>().unwrap())
    }

    /// Notifies observers as if the value had changed.
    pub fn notify(&self) {
        with_rt(|rt| rt.mark_observers(self.id, State::Dirty));
        flush();
    }

    pub fn is_alive(&self) -> bool {
        with_rt(|rt| rt.nodes.contains_key(self.id))
    }
}

/// A cached derived value. Downstream computations only re-run when the derived value changes.
pub struct Memo<T> {
    id: ReactiveId,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Memo<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Memo<T> {}

/// Creates a memo owned by the current owner.
pub fn memo<T: PartialEq + 'static>(f: impl Fn() -> T + 'static) -> Memo<T> {
    let compute: Compute = Rc::new(move |slot: &Value| {
        let new = f();
        let mut slot = slot.borrow_mut();
        let changed = match slot.as_ref().and_then(|v| v.downcast_ref::<T>()) {
            Some(old) => *old != new,
            None => true,
        };
        if changed {
            *slot = Some(Box::new(new));
        }
        changed
    });
    let id = with_rt(|rt| rt.create(Kind::Memo, None, Some(compute)));
    run(id);
    Memo {
        id,
        _marker: PhantomData,
    }
}

impl<T: 'static> Memo<T> {
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        update_if_necessary(self.id);
        with_rt(|rt| rt.track(self.id));
        let value = value_of(self.id);
        let value = value.borrow();
        f(value.as_ref().unwrap().downcast_ref::<T>().unwrap())
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }
}

/// A value that a control shows and changes: a [`Signal`], or a pair of functions for a value that
/// comes from somewhere else (a checkbox that stands for a group, say). Controls take
/// `impl Into<Binding<T>>`, so a signal can be passed as it is. See `docs/design/33-customization.md`.
pub struct Binding<T: 'static> {
    inner: BindingInner<T>,
}

enum BindingInner<T: 'static> {
    Signal(Signal<T>),
    Functions {
        get: Signal<Rc<dyn Fn() -> T>>,
        set: Signal<Rc<dyn Fn(T)>>,
    },
}

impl<T> Clone for BindingInner<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for BindingInner<T> {}

impl<T> Clone for Binding<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Binding<T> {}

impl<T: 'static> Binding<T> {
    /// A value that `get` reads and `set` is told to change. `get` is read where the control
    /// shows it, so what it reads is tracked.
    pub fn new(get: impl Fn() -> T + 'static, set: impl Fn(T) + 'static) -> Self {
        let get: Rc<dyn Fn() -> T> = Rc::new(get);
        let set: Rc<dyn Fn(T)> = Rc::new(set);
        Self {
            inner: BindingInner::Functions {
                get: signal(get),
                set: signal(set),
            },
        }
    }

    /// Reads the value, tracking the read.
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        match self.inner {
            BindingInner::Signal(s) => s.get(),
            BindingInner::Functions { get, .. } => get.get_untracked()(),
        }
    }

    pub fn get_untracked(&self) -> T
    where
        T: Clone,
    {
        untrack(|| self.get())
    }

    /// Asks for the value to become `value`.
    pub fn set(&self, value: T) {
        match self.inner {
            BindingInner::Signal(s) => s.set(value),
            BindingInner::Functions { set, .. } => set.get_untracked()(value),
        }
    }
}

impl<T: 'static> From<Signal<T>> for Binding<T> {
    fn from(signal: Signal<T>) -> Self {
        Self {
            inner: BindingInner::Signal(signal),
        }
    }
}

/// Runs `f` now and again whenever a reactive value it read changes.
pub fn effect(f: impl Fn() + 'static) {
    let compute: Compute = Rc::new(move |_| {
        f();
        false
    });
    let id = with_rt(|rt| rt.create(Kind::Effect, None, Some(compute)));
    run(id);
}

/// Like [`effect`], but passes the previous return value to the next run.
#[doc(hidden)]
pub fn effect_with<T: 'static>(f: impl Fn(Option<T>) -> T + 'static) {
    let prev: Rc<RefCell<Option<T>>> = Rc::new(RefCell::new(None));
    effect(move || {
        let p = prev.borrow_mut().take();
        let next = f(p);
        *prev.borrow_mut() = Some(next);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_reruns_on_change() {
        let (_, owner) = create_root(|| {
            let count = signal(0);
            let seen = Rc::new(RefCell::new(Vec::new()));
            let s = seen.clone();
            effect(move || s.borrow_mut().push(count.get()));
            count.set(1);
            count.set(2);
            assert_eq!(*seen.borrow(), vec![0, 1, 2]);
        });
        owner.dispose();
    }

    #[test]
    fn memo_stops_propagation_when_equal() {
        let (_, owner) = create_root(|| {
            let n = signal(1);
            let parity = memo(move || n.get() % 2);
            let runs = Rc::new(Cell::new(0));
            let r = runs.clone();
            effect(move || {
                parity.get();
                r.set(r.get() + 1);
            });
            n.set(3);
            assert_eq!(runs.get(), 1);
            n.set(4);
            assert_eq!(runs.get(), 2);
        });
        owner.dispose();
    }

    #[test]
    fn glitch_free_diamond() {
        let (_, owner) = create_root(|| {
            let a = signal(1);
            let b = memo(move || a.get() * 2);
            let c = memo(move || a.get() + 1);
            let seen = Rc::new(RefCell::new(Vec::new()));
            let s = seen.clone();
            effect(move || s.borrow_mut().push((b.get(), c.get())));
            a.set(5);
            assert_eq!(*seen.borrow(), vec![(2, 2), (10, 6)]);
        });
        owner.dispose();
    }

    #[test]
    fn batch_runs_once() {
        let (_, owner) = create_root(|| {
            let a = signal(0);
            let b = signal(0);
            let runs = Rc::new(Cell::new(0));
            let r = runs.clone();
            effect(move || {
                a.get();
                b.get();
                r.set(r.get() + 1);
            });
            batch(|| {
                a.set(1);
                b.set(1);
            });
            assert_eq!(runs.get(), 2);
        });
        owner.dispose();
    }

    #[test]
    fn nested_effects_are_disposed_on_rerun() {
        let (_, owner) = create_root(|| {
            let outer = signal(0);
            let inner = signal(0);
            let inner_runs = Rc::new(Cell::new(0));
            let cleanups = Rc::new(Cell::new(0));
            let (ir, cl) = (inner_runs.clone(), cleanups.clone());
            effect(move || {
                outer.get();
                let ir = ir.clone();
                effect(move || {
                    inner.get();
                    ir.set(ir.get() + 1);
                });
                let cl = cl.clone();
                on_cleanup(move || cl.set(cl.get() + 1));
            });
            assert_eq!(inner_runs.get(), 1);
            outer.set(1);
            assert_eq!(cleanups.get(), 1);
            assert_eq!(inner_runs.get(), 2);
            inner.set(1);
            // Only the live inner effect re-runs.
            assert_eq!(inner_runs.get(), 3);
        });
        owner.dispose();
    }

    #[test]
    fn a_source_read_no_more_is_forgotten_and_one_read_again_is_not_doubled() {
        let (_, owner) = create_root(|| {
            let step = signal(0);
            let (a, b, c) = (signal(0), signal(0), signal(0));
            let runs = Rc::new(Cell::new(0));
            let r = runs.clone();
            effect(move || {
                r.set(r.get() + 1);
                // Different sources, in a different order, some of them twice.
                match step.get() {
                    0 => {
                        a.get();
                        b.get();
                        c.get();
                    }
                    1 => {
                        c.get();
                        a.get();
                        c.get();
                    }
                    2 => {
                        b.get();
                    }
                    _ => {}
                }
            });
            let writes = |expected: [bool; 3]| {
                for (s, expected) in [a, b, c].iter().zip(expected) {
                    let before = runs.get();
                    s.update(|v| *v += 1);
                    assert_eq!(runs.get() - before, expected as i32);
                }
            };
            writes([true, true, true]);
            step.set(1);
            writes([true, false, true]);
            step.set(2);
            writes([false, true, false]);
            step.set(3);
            writes([false, false, false]);
            step.set(0);
            writes([true, true, true]);
        });
        owner.dispose();
    }

    #[test]
    fn a_source_read_twice_observes_once() {
        let (_, owner) = create_root(|| {
            let x = signal(0);
            effect(move || {
                x.get();
                x.get();
            });
            x.set(1);
            x.set(2);
            assert_eq!(with_rt(|rt| rt.nodes[x.id].observers.len()), 1);
        });
        owner.dispose();
    }

    #[test]
    fn a_parent_runs_before_the_child_it_replaces() {
        let (_, owner) = create_root(|| {
            let x = signal(0);
            let runs = Rc::new(Cell::new(0));
            let r = runs.clone();
            effect(move || {
                let r = r.clone();
                // The child gets to read it first, so comes first among equals.
                effect(move || {
                    x.get();
                    r.set(r.get() + 1);
                });
                x.get();
            });
            assert_eq!(runs.get(), 1);
            x.set(1);
            assert_eq!(
                runs.get(),
                2,
                "the old child must not run just before it goes"
            );
        });
        owner.dispose();
    }

    #[test]
    fn shallower_effects_run_first_and_equals_in_order() {
        let (_, owner) = create_root(|| {
            let x = signal(0);
            let order = Rc::new(RefCell::new(Vec::new()));
            for name in ["first", "second"] {
                let order = order.clone();
                effect(move || {
                    x.get();
                    order.borrow_mut().push(name);
                });
            }
            order.borrow_mut().clear();
            x.set(1);
            assert_eq!(*order.borrow(), ["first", "second"]);
        });
        owner.dispose();
    }

    #[test]
    fn dynamic_dependencies() {
        let (_, owner) = create_root(|| {
            let flag = signal(true);
            let a = signal(0);
            let b = signal(0);
            let runs = Rc::new(Cell::new(0));
            let r = runs.clone();
            effect(move || {
                if flag.get() {
                    a.get()
                } else {
                    b.get()
                };
                r.set(r.get() + 1);
            });
            b.set(1);
            assert_eq!(runs.get(), 1);
            flag.set(false);
            assert_eq!(runs.get(), 2);
            a.set(1);
            assert_eq!(runs.get(), 2);
            b.set(2);
            assert_eq!(runs.get(), 3);
        });
        owner.dispose();
    }
}
