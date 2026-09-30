//! A panel taken out of its area, and the window that can host it.
//!
//! Detaching is a move, not a close: the panel's view stays live and it is
//! never told [`Panel::on_removed`], the same contract a drag between groups
//! honors. What comes back is a [`DetachedPanel`] — the view handle, the
//! [`PanelState`] it dumped at detach time, and the region it came from —
//! which any [`DockArea`] can take back through
//! [`DockArea::adopt_detached`]. That the panel entity is app-global rather
//! than window-bound is what makes the move work *across* windows: a
//! `DetachedPanel` handed to a `DockArea` in another window renders there
//! unchanged.
//!
//! [`DetachedDock`] is the window half. It is a root view holding one
//! `DockArea` — the area the panel was adopted into — plus the home area it
//! returns its panels to when the window closes. [`DockArea::pop_out_panel`]
//! wires all of it: detach, open the window, adopt, and re-dock on close.
//!
//! [`Panel::on_removed`]: super::Panel::on_removed

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, BorrowAppContext, Context, Entity, FocusHandle, Global, Hsla, InteractiveElement,
    IntoElement, Modifiers, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, WeakEntity, Window, actions, div, px, svg,
};

use super::{DockArea, DockPlacement, PanelId, PanelState, PanelView};

actions!(dock, [CloseDetached]);

/// A panel taken out of a [`DockArea`], still live and still owning its
/// entity, together with everything a host needs to persist or re-home it.
///
/// Produced by [`DockArea::detach_panel`] and consumed by
/// [`DockArea::adopt_detached`] — or unpacked through [`Self::into_view`] for
/// a host that wants one of the area's `*_view` entry points instead.
pub struct DetachedPanel {
    id: PanelId,
    view: Arc<dyn PanelView>,
    state: PanelState,
    from: DockPlacement,
}

impl DetachedPanel {
    pub(crate) fn new(
        id: PanelId,
        view: Arc<dyn PanelView>,
        state: PanelState,
        from: DockPlacement,
    ) -> Self {
        Self {
            id,
            view,
            state,
            from,
        }
    }

    /// The panel's stable id — its entity id — unchanged by the move.
    pub fn panel_id(&self) -> PanelId {
        self.id
    }

    /// The live view handle. Borrowed form; take ownership with
    /// [`Self::into_view`].
    pub fn view(&self) -> &Arc<dyn PanelView> {
        &self.view
    }

    /// The view, owned. For a host that adopts through
    /// [`DockArea::add_panel_view`] or [`DockArea::add_tile_view`] rather
    /// than [`DockArea::adopt_detached`].
    pub fn into_view(self) -> Arc<dyn PanelView> {
        self.view
    }

    /// What [`Panel::dump`](super::Panel::dump) reported at detach time, so a
    /// host persisting a multi-window layout can write the panel out without
    /// owning an area to dump it through.
    pub fn state(&self) -> &PanelState {
        &self.state
    }

    /// The region the panel was detached from. A pop-out window re-docks
    /// into the same region it left; a host moving the panel somewhere else
    /// names its own placement and ignores this.
    pub fn from(&self) -> DockPlacement {
        self.from
    }
}

/// The root view of a pop-out window: one [`DockArea`] holding detached
/// panels, plus the area they return to when the window closes.
///
/// `DockArea::pop_out_panel` builds this; nothing else needs to. The inner
/// area is a full `DockArea` — drags, splits and tabs all work inside the
/// pop-out — so a host can also move panels *into* a detached window by
/// detaching them from home and adopting them here.
pub struct DetachedDock {
    area: Entity<DockArea>,
    home: WeakEntity<DockArea>,
    /// Painted behind the panels — the host's window background, projected
    /// down through [`DockArea::set_window_background`] at pop-out time.
    ///
    /// [`DockArea::set_window_background`]: super::DockArea::set_window_background
    background: Option<Hsla>,
    /// The label the header row leads with. `pop_out_panel` projects the
    /// window title the host chose; when no title was set the header falls
    /// back to the hosted panel's own name, which is the most specific label
    /// this layer can know.
    title: Option<SharedString>,
    /// The home region each panel originally came from. A panel hosted here
    /// sits at `Center` in the inner area, so a detach at close time would
    /// report `Center` — this map remembers the region it actually left.
    origins: HashMap<PanelId, DockPlacement>,
    /// Focused on the root div. An unfocused window's dispatch path is just
    /// the root view node, so nothing this view paints — key contexts, key
    /// and action listeners — would ever be consulted without it.
    focus_handle: FocusHandle,
}

impl DetachedDock {
    pub(crate) fn new(
        area: Entity<DockArea>,
        home: WeakEntity<DockArea>,
        background: Option<Hsla>,
        title: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            area,
            home,
            background,
            title,
            origins: HashMap::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Record the home region a panel was detached from before it was hosted
    /// here. `pop_out_panel` calls this for its panel; a host that moves more
    /// panels into the window should do the same or they re-dock to `Center`.
    pub fn remember_origin(&mut self, panel: PanelId, from: DockPlacement) {
        self.origins.insert(panel, from);
    }

    /// The area this window hosts.
    pub fn dock_area(&self) -> &Entity<DockArea> {
        &self.area
    }

    /// The area this window's panels re-dock into on close, if it is still
    /// alive.
    pub fn home(&self) -> &WeakEntity<DockArea> {
        &self.home
    }

    /// Whether this window currently hosts a panel whose registry name is
    /// `name`.
    ///
    /// This is the question a host's "summon" has to ask before building a
    /// panel: `DockArea` lookups only see their own window, so a panel
    /// sitting in a pop-out is invisible to them and the summon would
    /// duplicate it. Naming it rather than the panel id matters because the
    /// panel a layout restore rebuilds gets a fresh id, while its registry
    /// name is the stable identity the summon means.
    pub fn hosts_panel_named(&self, name: &str, cx: &App) -> bool {
        let area = self.area.read(cx);
        area.placed_panels().iter().any(|(id, _)| {
            area.panel(*id)
                .is_some_and(|view| view.panel_name(cx) == name)
        })
    }

    /// The title the header row leads with: the host's window title when one
    /// was projected at pop-out time, else the hosted panel's own name, else
    /// nothing — an empty pop-out draws an unlabeled header.
    fn header_title(&self, cx: &App) -> Option<SharedString> {
        self.title.clone().or_else(|| {
            let area = self.area.read(cx);
            let id = area
                .active_panel_at(DockPlacement::Center)
                .or_else(|| area.placed_panels().first().map(|(id, _)| *id))?;
            area.panel(id)
                .map(|view| SharedString::from(view.panel_name(cx)))
        })
    }

    /// Move every panel the inner area still holds back into `home`, each at
    /// the region it was originally detached from.
    ///
    /// `pop_out_panel` installs this as the window's should-close handler; a
    /// host offering an explicit "re-dock" affordance invokes the same path.
    /// Returns `true` when every panel the area held was adopted home — or
    /// the area held none. `false` means some panel stayed put: a home that
    /// is already gone leaves the panels where they are rather than dropping
    /// them mid-flight, and a host that asked because it is about to
    /// overwrite the layout — a reset or load — should read that as a
    /// refusal and leave the layout alone.
    pub fn redock_home(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.redock_home_inner(window, None, cx)
    }

    /// [`Self::redock_home`], with a window to adopt through when the home
    /// area's own window cannot be reached for update — the one case being
    /// the caller holding it. [`DetachedWindows::redock_all`] is invoked
    /// from inside the window the home area lives in, which puts it on the
    /// update stack, so `update_in` cannot borrow it; the caller lends its
    /// own `&mut Window`, which *is* that window.
    fn redock_home_inner(
        &mut self,
        window: &mut Window,
        mut home_window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut all_home = true;
        let panels = self.area.read(cx).placed_panels();
        for (panel, _) in panels {
            // A dead home cannot take anything: leave the panel hosted here,
            // where a caller that closes anyway drops it deliberately.
            if self.home.upgrade().is_none() {
                all_home = false;
                continue;
            }
            let Some(detached) = self
                .area
                .update(cx, |area, cx| area.detach_panel(panel, window, cx))
            else {
                continue;
            };
            // The recorded origin beats `from` here — inside this window the
            // panel sits at `Center`, which is where it is hosted, not where
            // it belongs at home.
            let from = self
                .origins
                .get(&panel)
                .copied()
                .unwrap_or_else(|| detached.from());
            // `update_in` delivers the adoption against the home area's own
            // window — the one it was last rendered in — rather than this
            // closing one, so `on_added_to`/`set_active` reach the panel with
            // the window it actually lives in now. Shared through a cell so
            // that a handoff that never ran still has the panel to put back
            // here, rather than letting the view drop out of every tree.
            let detached = Rc::new(RefCell::new(Some(detached)));
            let pending = detached.clone();
            let mut adopted = self
                .home
                .update_in(cx, |home, window, cx| {
                    if let Some(detached) = pending.borrow_mut().take() {
                        home.adopt_detached(detached, from, None, window, cx);
                    }
                })
                .is_ok();
            if !adopted
                && let (Some(detached), Some(window)) =
                    (pending.borrow_mut().take(), home_window.as_deref_mut())
            {
                adopted = self
                    .home
                    .update(cx, |home, cx| {
                        home.adopt_detached(detached, from, None, window, cx)
                    })
                    .is_ok();
            }
            if !adopted {
                if let Some(detached) = detached.borrow_mut().take() {
                    self.area.update(cx, |area, cx| {
                        area.adopt_detached(detached, DockPlacement::Center, None, window, cx)
                    });
                }
                all_home = false;
            }
        }
        all_home
    }
}

/// Every pop-out window currently open.
///
/// [`DockArea::pop_out_panel`] registers each [`DetachedDock`] root as its
/// window comes up, so the inventory needs no cooperation from the host that
/// asked for the window — and because the handles are weak, a window that
/// closed by any path drops out of [`Self::live`] without being unregistered.
/// That makes this the one place a host can ask "is that panel already out
/// there?" before building a second copy, and the one list a wholesale layout
/// change — reset or load — has to drain first: the docked trees cannot see
/// panels hosted in other windows, so replacing them while a pop-out is open
/// would strand the workspace's own handles on live panels.
#[derive(Default)]
pub struct DetachedWindows {
    roots: Vec<WeakEntity<DetachedDock>>,
}

impl Global for DetachedWindows {}

impl DetachedWindows {
    /// Note a pop-out that just came up. `pop_out_panel` calls this; a host
    /// building a `DetachedDock` by hand should do the same or its window is
    /// invisible to [`Self::live`] and [`Self::redock_all`].
    pub(crate) fn register(root: WeakEntity<DetachedDock>, cx: &mut App) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        cx.update_global(|windows: &mut Self, _| {
            windows.roots.retain(|root| root.upgrade().is_some());
            windows.roots.push(root);
        });
    }

    /// The roots of the pop-out windows still open, dead entries filtered
    /// out. Order is registration order.
    pub fn live(cx: &App) -> Vec<WeakEntity<DetachedDock>> {
        cx.try_global::<Self>()
            .map(|windows| {
                windows
                    .roots
                    .iter()
                    .filter(|root| root.upgrade().is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Re-dock every live pop-out's panels into its home area and close its
    /// window.
    ///
    /// `window` is the window the caller is updating in. It is lent to the
    /// adoption when a home area's own window cannot be reached because the
    /// caller is the one holding it — the common case, since a host drains
    /// the list from inside the window the pop-outs came from.
    ///
    /// Returns the number of windows that did not go home — each logged
    /// `false` from [`DetachedDock::redock_home`] or could not be reached at
    /// all — which stay open with their panels intact. A host draining the
    /// list before a reset should treat a nonzero count as a refusal: the
    /// layout it is about to apply would still orphan what is hosted there.
    pub fn redock_all(window: &mut Window, cx: &mut App) -> usize {
        Self::live(cx)
            .into_iter()
            .filter(|root| {
                !root
                    .update_in(cx, |detached, pop_window, cx| {
                        let home = detached.redock_home_inner(pop_window, Some(&mut *window), cx);
                        if home {
                            pop_window.remove_window();
                        }
                        home
                    })
                    // An error means the window could not be reached at all
                    // — same refusal, from further upstream.
                    .unwrap_or(false)
            })
            .count()
    }
}

impl Render for DetachedDock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `secondary-w` (⌘W) reaches this root through the "DetachedDock"
        // context the host binds — the platform should-close path covers the
        // red button, so close-by-key has to re-dock before removing the
        // window or the hosted panels would drop with it.
        //
        // The focus matters as much as the context: with nothing focused the
        // dispatch path is only the root view node, which is a parent of this
        // div — the context and listener below would sit on a node no key
        // event ever visits. Focus the root whenever focus has escaped the
        // tree (unset, or a handle whose node left the frame); a focused
        // field inside a hosted panel stays focused, and its dispatch path
        // still runs through here.
        if !self.focus_handle.contains_focused(window, cx) {
            self.focus_handle.focus(window, cx);
        }
        let colors = crate::Theme::global(cx).tokens.colors;
        let title = self.header_title(cx).unwrap_or_default();
        let close = cx.weak_entity();
        let escape = cx.weak_entity();
        let redock = cx.weak_entity();
        let mut view = div()
            .flex()
            .flex_col()
            .key_context("DetachedDock")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(move |_: &CloseDetached, window, cx| {
                _ = close.update(cx, |detached, cx| detached.redock_home(window, cx));
                window.remove_window();
            })
            // Escape is the same act as the Re-dock button below: re-dock,
            // then close. A hosted panel that consumed the keystroke stops
            // propagation, so it never reaches this listener.
            .on_key_down(move |event, window, cx| {
                if event.keystroke.key == "escape" && event.keystroke.modifiers == Modifiers::none()
                {
                    _ = escape.update(cx, |detached, cx| detached.redock_home(window, cx));
                    window.remove_window();
                }
            });
        if let Some(background) = self.background {
            view = view.bg(background);
        }
        // The titlebar already names the window; the in-window row stays a
        // thin affordance strip — just the way home, right-aligned.
        let label = if title.is_empty() {
            "Re-dock — return this panel to the main window".to_string()
        } else {
            format!("Re-dock {title} — return this panel to the main window")
        };
        view
            // The affordance the platform chrome cannot offer: a window
            // decoration has a close button, not "put the panel back". The
            // native titlebar already names the panel, so the strip carries
            // only the one action — a quiet ghost riding right; a filled
            // button outshouts the panel it serves.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .id("redock")
                            .role(gpui::Role::Button)
                            .aria_label(label)
                            .cursor_pointer()
                            .flex()
                            .flex_row()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            // Ghost, not filled: muted until hovered, when it
                            // lifts to the accent wash like a tab affordance.
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .hover(|style| style.bg(colors.accent).text_color(colors.foreground))
                            .child(
                                svg()
                                    .path("icons/panel-right.svg")
                                    .size(px(12.))
                                    .text_color(colors.muted_foreground),
                            )
                            .child("Re-dock")
                            .on_click(move |_, window, cx| {
                                _ = redock
                                    .update(cx, |detached, cx| detached.redock_home(window, cx));
                                window.remove_window();
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.area.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext, WindowOptions};

    use super::*;
    use crate::dock::test_support::{Log, PanelSignal, TestPanel, drain, log_of};
    use crate::dock::{DockLayout, DockPlacement as Placement};

    fn setup(cx: &mut TestAppContext) -> (Entity<DockArea>, &mut VisualTestContext) {
        cx.update(|cx| {
            let _ = crate::Theme::global_mut(cx);
        });
        cx.add_window_view(|window, cx| DockArea::new("test-dock", None, window, cx))
    }

    /// Two center tab groups side by side, one logging panel each.
    fn two_groups<'a>(
        log: &Log,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<DockArea>,
        Entity<TestPanel>,
        Entity<TestPanel>,
        &'a mut VisualTestContext,
    ) {
        let (area, cx) = setup(cx);
        let log = log.clone();
        let (alpha, beta) = cx.update(|window, cx| {
            let alpha = TestPanel::logging("Alpha", &log, cx);
            let beta = TestPanel::logging("Beta", &log, cx);
            area.update(cx, |area, cx| {
                area.set_center(
                    DockLayout::h_split()
                        .child(DockLayout::tabs().panel(alpha.clone()), None)
                        .child(DockLayout::tabs().panel(beta.clone()), None),
                    window,
                    cx,
                );
            });
            (alpha, beta)
        });
        (area, alpha, beta, cx)
    }

    fn id_of(panel: &Entity<TestPanel>) -> PanelId {
        PanelId::from(panel.entity_id())
    }

    fn panel_is_in(
        area: &Entity<DockArea>,
        panel: PanelId,
        placement: Placement,
        cx: &VisualTestContext,
    ) -> bool {
        cx.read(|cx| {
            area.read(cx)
                .layout(placement)
                .is_some_and(|tree| tree.contains_panel(panel))
        })
    }

    #[gpui::test]
    fn detaching_a_panel_keeps_it_live_and_never_says_removed(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let detached = cx.update(|window, cx| {
            area.update(cx, |area, cx| area.detach_panel(alpha_id, window, cx))
        });
        cx.run_until_parked();

        let detached = detached.expect("a panel in the area detaches");
        assert_eq!(detached.panel_id(), alpha_id);
        assert_eq!(detached.from(), Placement::Center);
        assert_eq!(
            cx.read(|cx| detached.view().panel_name(cx)),
            "Alpha",
            "the view came with it, still live"
        );
        assert_eq!(detached.state().panel_name, "Alpha");

        assert!(
            !panel_is_in(&area, alpha_id, Placement::Center, cx),
            "the panel left the tree"
        );
        assert!(
            cx.read(|cx| area.read(cx).panel(alpha_id).is_none()),
            "and left the view map"
        );
        assert!(
            !drain(&log).contains(&("Alpha", PanelSignal::Removed)),
            "detaching is a move, not a close: on_removed does not fire"
        );
    }

    #[gpui::test]
    fn an_adopted_panel_reenters_its_own_area_without_a_removed_edge(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let detached = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| area.detach_panel(alpha_id, window, cx))
            })
            .unwrap();
        cx.update(|window, cx| {
            area.update(cx, |area, cx| {
                area.adopt_detached(detached, Placement::Center, None, window, cx)
            })
        });
        cx.run_until_parked();

        assert!(
            panel_is_in(&area, alpha_id, Placement::Center, cx),
            "the adopted panel is back in the tree"
        );
        let signals = drain(&log);
        assert!(
            !signals.contains(&("Alpha", PanelSignal::Removed)),
            "the round trip never told the panel it was removed"
        );
        assert!(
            signals.contains(&("Alpha", PanelSignal::Added)),
            "and it was told it joined a group again"
        );
    }

    #[gpui::test]
    fn a_panel_moves_between_areas_in_different_windows(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        // A second area in a second window — the shape a pop-out takes when
        // a host builds the window itself.
        let (other, _other_cx) =
            cx.add_window_view(|window, cx| DockArea::new("other-dock", None, window, cx));
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let detached = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| area.detach_panel(alpha_id, window, cx))
            })
            .unwrap();
        cx.update(|window, cx| {
            other.update(cx, |other, cx| {
                other.adopt_detached(detached, Placement::Center, None, window, cx)
            })
        });
        cx.run_until_parked();

        assert!(
            panel_is_in(&other, alpha_id, Placement::Center, cx),
            "the panel landed in the other window's area"
        );
        assert!(
            !panel_is_in(&area, alpha_id, Placement::Center, cx),
            "and left the source's"
        );
        assert!(
            !drain(&log).contains(&("Alpha", PanelSignal::Removed)),
            "a cross-area move is still a move: no on_removed"
        );
    }

    #[gpui::test]
    fn popping_out_hosts_the_panel_in_a_new_window(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let pop_window = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(alpha_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the window opened");
        cx.run_until_parked();

        assert!(
            !panel_is_in(&area, alpha_id, Placement::Center, cx),
            "the panel left the source area"
        );
        let hosted = cx
            .update(|_window, cx| {
                pop_window.update(cx, |detached, _window, cx| {
                    detached
                        .dock_area()
                        .read(cx)
                        .layout(Placement::Center)
                        .is_some_and(|tree| tree.contains_panel(alpha_id))
                })
            })
            .unwrap();
        assert!(hosted, "the pop-out window's area holds the panel");
        assert!(
            !drain(&log).contains(&("Alpha", PanelSignal::Removed)),
            "popping out is a move, not a close"
        );
    }

    #[gpui::test]
    fn closing_the_pop_out_window_re_docks_the_panel_home(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let pop_window = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(alpha_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the window opened");
        cx.run_until_parked();
        drain(&log);

        let mut pop = VisualTestContext::from_window(*pop_window, cx);
        assert!(
            pop.simulate_close(),
            "the window's should-close handler ran and allowed the close"
        );
        cx.run_until_parked();

        assert!(
            panel_is_in(&area, alpha_id, Placement::Center, cx),
            "the panel re-docked into the region it was detached from"
        );
        let signals = drain(&log);
        assert!(
            !signals.contains(&("Alpha", PanelSignal::Removed)),
            "re-docking is a move back, not a close"
        );
        assert!(
            signals.contains(&("Alpha", PanelSignal::Added)),
            "and the panel heard it joined a group at home"
        );
    }

    #[gpui::test]
    fn closing_the_pop_out_re_docks_to_the_region_the_panel_left(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, _alpha, _beta, cx) = two_groups(&log, cx);
        let gamma = cx.update(|window, cx| {
            let gamma = TestPanel::logging("Gamma", &log, cx);
            area.update(cx, |area, cx| {
                area.add_panel(gamma.clone(), Placement::Right, None, window, cx)
            });
            gamma
        });
        cx.run_until_parked();

        let gamma_id = id_of(&gamma);
        let pop_window = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(gamma_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the window opened");
        cx.run_until_parked();

        let mut pop = VisualTestContext::from_window(*pop_window, cx);
        assert!(pop.simulate_close());
        cx.run_until_parked();

        assert!(
            panel_is_in(&area, gamma_id, Placement::Right, cx),
            "a side-docked panel re-docks to the dock it left, not Center"
        );
        assert!(
            !panel_is_in(&area, gamma_id, Placement::Center, cx),
            "and does not land as a center tab"
        );
    }

    #[gpui::test]
    fn detaching_a_panel_the_area_does_not_hold_is_none(cx: &mut TestAppContext) {
        let (area, cx) = setup(cx);
        let stray = cx.update(|_, cx| TestPanel::new("Stray", cx));
        let stray_id = id_of(&stray);

        let detached = cx.update(|window, cx| {
            area.update(cx, |area, cx| area.detach_panel(stray_id, window, cx))
        });
        assert!(detached.is_none());
    }

    #[gpui::test]
    fn placed_panels_reports_every_panel_with_its_region(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, beta, cx) = two_groups(&log, cx);
        let gamma = cx.update(|window, cx| {
            let gamma = TestPanel::logging("Gamma", &log, cx);
            area.update(cx, |area, cx| {
                area.add_panel(gamma.clone(), Placement::Right, None, window, cx)
            });
            gamma
        });
        cx.run_until_parked();

        let placed = cx.read(|cx| area.read(cx).placed_panels());
        assert_eq!(placed.len(), 3);
        assert!(placed.contains(&(id_of(&alpha), Placement::Center)));
        assert!(placed.contains(&(id_of(&beta), Placement::Center)));
        assert!(placed.contains(&(id_of(&gamma), Placement::Right)));
    }

    #[gpui::test]
    fn a_pop_out_registers_until_its_window_is_gone(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();

        let alpha_id = id_of(&alpha);
        let pop_window = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(alpha_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the window opened");
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_window, cx| DetachedWindows::live(cx)).len(),
            1,
            "the pop-out registered itself when it opened"
        );

        let mut pop = VisualTestContext::from_window(*pop_window, cx);
        pop.update(|window, _cx| window.remove_window());
        cx.run_until_parked();

        assert!(
            cx.update(|_window, cx| DetachedWindows::live(cx))
                .is_empty(),
            "a closed window drops out of the inventory without an unregister"
        );
        assert!(
            pop_window.update(&mut cx.cx, |_, _, _| ()).is_err(),
            "and the window is really gone"
        );
    }

    #[gpui::test]
    fn redock_all_brings_every_pop_out_home(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let beta_id = id_of(&beta);
        let first = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(alpha_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the first window opened");
        let second = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(beta_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the second window opened");
        cx.run_until_parked();
        assert_eq!(cx.update(|_window, cx| DetachedWindows::live(cx)).len(), 2);

        let hosts_alpha = cx
            .update(|_window, cx| {
                first.update(cx, |detached, _window, cx| {
                    detached.hosts_panel_named("Alpha", cx)
                })
            })
            .expect("the first pop-out is live");
        assert!(hosts_alpha, "a summon-side lookup sees the hosted panel");

        // Called from inside the home window's own update — the window every
        // pop-out wants to adopt into — so `update_in` cannot borrow it and
        // the adoption runs through the window the caller lent instead.
        let refused = cx.update(|window, cx| DetachedWindows::redock_all(window, cx));
        cx.run_until_parked();

        assert_eq!(refused, 0, "every pop-out went home");
        assert!(panel_is_in(&area, alpha_id, Placement::Center, cx));
        assert!(panel_is_in(&area, beta_id, Placement::Center, cx));
        assert!(
            cx.update(|_window, cx| DetachedWindows::live(cx))
                .is_empty(),
            "the inventory is empty once the windows are gone"
        );
        assert!(
            first.update(&mut cx.cx, |_, _, _| ()).is_err()
                && second.update(&mut cx.cx, |_, _, _| ()).is_err(),
            "both windows were removed"
        );
        assert!(
            !drain(&log).contains(&("Alpha", PanelSignal::Removed)),
            "re-docking all is still a move, not a close"
        );
    }

    #[gpui::test]
    fn redock_all_refuses_a_pop_out_whose_home_is_gone(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();

        let alpha_id = id_of(&alpha);
        let beta_id = id_of(&beta);
        let first = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(alpha_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the first window opened");
        let second = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(beta_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the second window opened");
        cx.run_until_parked();

        // Kill the home outright: its window goes, then the last entity
        // handle the test held, so `home` is a dead weak that nothing can
        // adopt into — not merely a window that is out of reach.
        cx.update(|window, _cx| window.remove_window());
        drop(area);
        cx.run_until_parked();

        // `first` runs the drain from inside its own window, which leaves
        // its own root mid-update and unreachable; `second` is reached but
        // its home is dead. Each refusal keeps its panel hosted.
        let refused = first
            .update(&mut cx.cx, |_, window, cx| {
                DetachedWindows::redock_all(window, cx)
            })
            .expect("the first pop-out is live");
        assert_eq!(refused, 2, "both pop-outs refused");

        cx.run_until_parked();
        for (window, name) in [(&first, "Alpha"), (&second, "Beta")] {
            let still_hosted = window
                .update(&mut cx.cx, |detached, _window, cx| {
                    detached.hosts_panel_named(name, cx)
                })
                .expect("the refused window stayed open");
            assert!(
                still_hosted,
                "the panel stayed hosted rather than orphaned mid-flight"
            );
        }
        assert_eq!(
            cx.cx.update(|cx| DetachedWindows::live(cx)).len(),
            2,
            "and both windows are still registered"
        );
    }

    #[gpui::test]
    fn escape_re_docks_the_panel_and_closes_the_window(cx: &mut TestAppContext) {
        let log = log_of();
        let (area, alpha, _beta, cx) = two_groups(&log, cx);
        cx.run_until_parked();
        drain(&log);

        let alpha_id = id_of(&alpha);
        let pop_window = cx
            .update(|window, cx| {
                area.update(cx, |area, cx| {
                    area.pop_out_panel(alpha_id, WindowOptions::default(), window, cx)
                })
            })
            .expect("the window opened");
        cx.run_until_parked();

        let mut pop = VisualTestContext::from_window(*pop_window, cx);
        pop.simulate_keystrokes("escape");
        cx.run_until_parked();

        assert!(
            panel_is_in(&area, alpha_id, Placement::Center, cx),
            "Escape re-docked the panel home"
        );
        assert!(
            pop_window.update(&mut cx.cx, |_, _, _| ()).is_err(),
            "and closed the pop-out window"
        );
        assert!(
            !drain(&log).contains(&("Alpha", PanelSignal::Removed)),
            "Escape is a move back, not a close"
        );
    }
}
