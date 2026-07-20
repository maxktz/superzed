use crate::{
    AnyActiveCall, AppState, CollaboratorId, FollowerState, Pane, ParticipantLocation, Workspace,
    WorkspaceSettings,
    notifications::DetachAndPromptErr,
    pane_group::element::pane_axis,
    workspace_card_gap,
    workspace_settings::{PaneSplitDirectionHorizontal, PaneSplitDirectionVertical},
};
use anyhow::Result;
use collections::HashMap;
use gpui::{
    Along, AnyView, AnyWeakView, Axis, Bounds, Entity, Hsla, IntoElement, MouseButton, Pixels,
    Point, StyleRefinement, WeakEntity, Window, point, size,
};
use parking_lot::Mutex;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::Settings;
use std::{cmp::Ordering, mem, sync::Arc};
use ui::prelude::*;

pub const HANDLE_HITBOX_SIZE: f32 = 4.0;
const HORIZONTAL_MIN_SIZE: f32 = 80.;
const VERTICAL_MIN_SIZE: f32 = 100.;

#[derive(Clone, Copy, Debug)]
pub struct SplitSizeHint {
    inserted_size: Pixels,
    available_size: Option<Pixels>,
}

impl SplitSizeHint {
    pub fn inserted_size(inserted_size: Pixels) -> Self {
        Self {
            inserted_size,
            available_size: None,
        }
    }

    pub fn inserted_size_in_available_space(inserted_size: Pixels, available_size: Pixels) -> Self {
        Self {
            inserted_size,
            available_size: Some(available_size),
        }
    }
}

/// One or many panes, arranged in a horizontal or vertical axis due to a split.
/// Panes have all their tabs and capabilities preserved, and can be split again or resized.
/// Single-pane group is a regular pane.
#[derive(Clone)]
pub struct PaneGroup {
    pub root: Member,
    pub is_center: bool,
}

pub struct PaneRenderResult {
    pub element: gpui::AnyElement,
    pub contains_active_pane: bool,
    #[cfg(any(test, feature = "test-support"))]
    pub decorated_pane_ix: Option<usize>,
}

impl PaneGroup {
    pub fn with_root(mut root: Member) -> Self {
        root.normalize_same_axis();
        Self {
            root,
            is_center: false,
        }
    }

    pub fn new(pane: Entity<Pane>) -> Self {
        Self {
            root: Member::Pane(pane),
            is_center: false,
        }
    }

    pub fn set_is_center(&mut self, is_center: bool) {
        self.is_center = is_center;
    }

    pub fn split(
        &mut self,
        old_pane: &Entity<Pane>,
        new_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut App,
    ) {
        self.split_with_size_hint(old_pane, new_pane, direction, None, cx);
    }

    pub fn split_with_size_hint(
        &mut self,
        old_pane: &Entity<Pane>,
        new_pane: &Entity<Pane>,
        direction: SplitDirection,
        size_hint: Option<SplitSizeHint>,
        cx: &mut App,
    ) {
        let found = match &mut self.root {
            Member::Pane(pane) => {
                if pane == old_pane {
                    self.root = Member::new_axis_with_size_hint(
                        old_pane.clone(),
                        new_pane.clone(),
                        direction,
                        size_hint,
                        None,
                    );
                    true
                } else {
                    false
                }
            }
            Member::Axis(axis) => axis.split(old_pane, new_pane, direction, size_hint, cx),
        };

        // If the pane wasn't found, fall back to splitting the first pane in the tree.
        if !found {
            let first_pane = self.root.first_pane();
            match &mut self.root {
                Member::Pane(_) => {
                    self.root = Member::new_axis_with_size_hint(
                        first_pane,
                        new_pane.clone(),
                        direction,
                        size_hint,
                        None,
                    );
                }
                Member::Axis(axis) => {
                    let _ = axis.split(&first_pane, new_pane, direction, size_hint, cx);
                }
            }
        }

        self.mark_positions(cx);
    }

    pub fn bounding_box_for_pane(&self, pane: &Entity<Pane>) -> Option<Bounds<Pixels>> {
        match &self.root {
            Member::Pane(_) => None,
            Member::Axis(axis) => axis.bounding_box_for_pane(pane),
        }
    }

    pub fn horizontal_size_for_pane(&self, pane: &Entity<Pane>) -> Option<Pixels> {
        match &self.root {
            Member::Pane(_) => None,
            Member::Axis(axis) => axis.horizontal_size_for_pane(pane),
        }
    }

    pub fn full_height_column_count(&self) -> usize {
        self.root.full_height_column_count()
    }

    pub fn pane_at_pixel_position(&self, coordinate: Point<Pixels>) -> Option<&Entity<Pane>> {
        match &self.root {
            Member::Pane(pane) => Some(pane),
            Member::Axis(axis) => axis.pane_at_pixel_position(coordinate),
        }
    }

    /// Moves active pane to span the entire border in the given direction,
    /// similar to Vim ctrl+w shift-[hjkl] motion.
    ///
    /// Returns:
    /// - Ok(true) if it found and moved a pane
    /// - Ok(false) if it found but did not move the pane
    /// - Err(_) if it did not find the pane
    pub fn move_to_border(
        &mut self,
        active_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut App,
    ) -> Result<bool> {
        if let Some(pane) = self.find_pane_at_border(direction)
            && pane == active_pane
        {
            return Ok(false);
        }

        if !self.remove_internal(active_pane, cx)? {
            return Ok(false);
        }

        if let Member::Axis(root) = &mut self.root
            && direction.axis() == root.axis
        {
            let idx = if direction.increasing() {
                root.members.len()
            } else {
                0
            };
            root.insert_moved_pane(idx, active_pane);
            self.mark_positions(cx);
            return Ok(true);
        }

        let members = if direction.increasing() {
            vec![self.root.clone(), Member::Pane(active_pane.clone())]
        } else {
            vec![Member::Pane(active_pane.clone()), self.root.clone()]
        };
        self.root = Member::Axis(PaneAxis::new(direction.axis(), members));
        self.mark_positions(cx);
        Ok(true)
    }

    fn find_pane_at_border(&self, direction: SplitDirection) -> Option<&Entity<Pane>> {
        match &self.root {
            Member::Pane(pane) => Some(pane),
            Member::Axis(axis) => axis.find_pane_at_border(direction),
        }
    }

    /// Returns:
    /// - Ok(true) if it found and removed a pane
    /// - Ok(false) if it found but did not remove the pane
    /// - Err(_) if it did not find the pane
    pub fn remove(&mut self, pane: &Entity<Pane>, cx: &mut App) -> Result<bool> {
        let result = self.remove_internal(pane, cx);
        if let Ok(true) = result {
            self.mark_positions(cx);
        }
        result
    }

    fn remove_internal(&mut self, pane: &Entity<Pane>, cx: &App) -> Result<bool> {
        match &mut self.root {
            Member::Pane(_) => Ok(false),
            Member::Axis(axis) => {
                if let Some(last_pane) = axis.remove(pane, cx)? {
                    self.root = last_pane;
                }
                Ok(true)
            }
        }
    }

    pub fn resize(
        &mut self,
        pane: &Entity<Pane>,
        direction: Axis,
        amount: Pixels,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) {
        match &mut self.root {
            Member::Pane(_) => {}
            Member::Axis(axis) => {
                let _ = axis.resize(pane, direction, amount, bounds);
            }
        };
        self.mark_positions(cx);
    }

    pub fn reset_pane_sizes(&mut self, cx: &mut App) {
        match &mut self.root {
            Member::Pane(_) => {}
            Member::Axis(axis) => {
                let _ = axis.reset_pane_sizes();
            }
        };
        self.mark_positions(cx);
    }

    pub fn swap(&mut self, from: &Entity<Pane>, to: &Entity<Pane>, cx: &mut App) {
        match &mut self.root {
            Member::Pane(_) => {}
            Member::Axis(axis) => axis.swap(from, to),
        };
        self.mark_positions(cx);
    }

    pub fn mark_positions(&mut self, cx: &mut App) {
        self.root.normalize_same_axis();
        let top_left_pane = self
            .is_center
            .then(|| self.root.first_visible_pane(cx))
            .flatten();
        self.root
            .mark_positions(self.is_center, top_left_pane.as_ref(), cx);
    }

    pub fn render(
        &self,
        zoomed: Option<&AnyWeakView>,
        maximized: Option<&WeakEntity<Pane>>,
        render_cx: &dyn PaneLeaderDecorator,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        self.root
            .render(0, zoomed, maximized, render_cx, window, cx)
            .element
    }

    pub fn panes(&self) -> Vec<&Entity<Pane>> {
        let mut panes = Vec::new();
        self.root.collect_panes(&mut panes);
        panes
    }

    pub fn first_pane(&self) -> Entity<Pane> {
        self.root.first_pane()
    }

    pub fn last_pane(&self) -> Entity<Pane> {
        self.root.last_pane()
    }

    pub fn find_pane_in_direction(
        &self,
        active_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &App,
    ) -> Option<Entity<Pane>> {
        let bounding_box = self.bounding_box_for_pane(active_pane)?;
        let cursor = active_pane.read(cx).pixel_position_of_cursor(cx);
        let center = match cursor {
            Some(cursor) if bounding_box.contains(&cursor) => cursor,
            _ => bounding_box.center(),
        };

        let mut pane_bounds = Vec::new();
        self.root.collect_pane_bounds(&mut pane_bounds);
        pane_bounds
            .into_iter()
            .filter(|(pane, _)| pane != active_pane)
            .filter_map(|(pane, candidate_bounds)| {
                pane_distances_in_direction(bounding_box, candidate_bounds, center, direction)
                    .map(|distances| (pane, distances))
            })
            .min_by(|(_, left), (_, right)| compare_pane_distances(*left, *right))
            .map(|(pane, _)| pane)
    }

    pub fn invert_axies(&mut self, cx: &mut App) {
        self.root.invert_pane_axies();
        self.mark_positions(cx);
    }
}

pub(crate) fn render_pane_card(
    pane: Entity<Pane>,
    render_cx: &dyn PaneLeaderDecorator,
    cx: &mut App,
) -> AnyElement {
    let decoration = render_cx.decorate(&pane, cx);

    div()
        .relative()
        .flex_1()
        .size_full()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().colors().border)
        .child(AnyView::from(pane).cached(StyleRefinement::default().v_flex().size_full()))
        .when_some(decoration.border, |this, color| {
            this.child(
                div()
                    .absolute()
                    .size_full()
                    .left_0()
                    .top_0()
                    .border_2()
                    .border_color(color),
            )
        })
        .children(decoration.status_box)
        .into_any()
}

type PaneDistances = (f32, f32, f32);

fn pane_distances_in_direction(
    active: Bounds<Pixels>,
    candidate: Bounds<Pixels>,
    anchor: Point<Pixels>,
    direction: SplitDirection,
) -> Option<PaneDistances> {
    let (primary, cross, center) = match direction {
        SplitDirection::Left => {
            let primary = active.left() - candidate.right();
            if primary < Pixels::ZERO {
                return None;
            }
            let cross = distance_to_interval(anchor.y, candidate.top(), candidate.bottom());
            let center = (candidate.center().y - anchor.y).as_f32().abs();
            (primary, cross, center)
        }
        SplitDirection::Right => {
            let primary = candidate.left() - active.right();
            if primary < Pixels::ZERO {
                return None;
            }
            let cross = distance_to_interval(anchor.y, candidate.top(), candidate.bottom());
            let center = (candidate.center().y - anchor.y).as_f32().abs();
            (primary, cross, center)
        }
        SplitDirection::Up => {
            let primary = active.top() - candidate.bottom();
            if primary < Pixels::ZERO {
                return None;
            }
            let cross = distance_to_interval(anchor.x, candidate.left(), candidate.right());
            let center = (candidate.center().x - anchor.x).as_f32().abs();
            (primary, cross, center)
        }
        SplitDirection::Down => {
            let primary = candidate.top() - active.bottom();
            if primary < Pixels::ZERO {
                return None;
            }
            let cross = distance_to_interval(anchor.x, candidate.left(), candidate.right());
            let center = (candidate.center().x - anchor.x).as_f32().abs();
            (primary, cross, center)
        }
    };

    Some((primary.as_f32(), cross.as_f32(), center))
}

fn distance_to_interval(value: Pixels, start: Pixels, end: Pixels) -> Pixels {
    if value < start {
        start - value
    } else if value > end {
        value - end
    } else {
        Pixels::ZERO
    }
}

fn compare_pane_distances(left: PaneDistances, right: PaneDistances) -> Ordering {
    left.0
        .total_cmp(&right.0)
        .then_with(|| left.1.total_cmp(&right.1))
        .then_with(|| left.2.total_cmp(&right.2))
}

#[derive(Debug, Clone)]
pub enum Member {
    Axis(PaneAxis),
    Pane(Entity<Pane>),
}

impl Member {
    pub fn mark_positions(
        &mut self,
        in_center_group: bool,
        top_left_pane: Option<&Entity<Pane>>,
        cx: &mut App,
    ) {
        match self {
            Member::Axis(pane_axis) => {
                for member in pane_axis.members.iter_mut() {
                    member.mark_positions(in_center_group, top_left_pane, cx);
                }
            }
            Member::Pane(entity) => entity.update(cx, |pane, cx| {
                pane.in_center_group = in_center_group;
                pane.set_reserve_traffic_light_space(
                    in_center_group && top_left_pane.is_some_and(|pane| pane == entity),
                    cx,
                );
            }),
        }
    }

    fn full_height_column_count(&self) -> usize {
        match self {
            Member::Pane(_) => 1,
            Member::Axis(axis) => axis.full_height_column_count(),
        }
    }

    fn is_visible(&self, cx: &App) -> bool {
        match self {
            Member::Axis(axis) => axis.members.iter().any(|member| member.is_visible(cx)),
            Member::Pane(pane) => pane.read(cx).is_visible(),
        }
    }

    fn clear_bounding_boxes(&self) {
        match self {
            Member::Axis(axis) => {
                axis.bounding_boxes.lock().fill(None);
                for member in &axis.members {
                    member.clear_bounding_boxes();
                }
            }
            Member::Pane(_) => {}
        }
    }
}

#[derive(Clone, Copy)]
pub struct PaneRenderContext<'a> {
    pub project: &'a Entity<Project>,
    pub follower_states: &'a HashMap<CollaboratorId, FollowerState>,
    pub active_call: Option<&'a dyn AnyActiveCall>,
    pub active_pane: &'a Entity<Pane>,
    pub app_state: &'a Arc<AppState>,
    pub workspace: &'a WeakEntity<Workspace>,
}

#[derive(Default)]
pub struct LeaderDecoration {
    border: Option<Hsla>,
    status_box: Option<AnyElement>,
}

pub trait PaneLeaderDecorator {
    fn decorate(&self, pane: &Entity<Pane>, cx: &App) -> LeaderDecoration;
    fn active_pane(&self) -> &Entity<Pane>;
    fn workspace(&self) -> &WeakEntity<Workspace>;
}

pub struct ActivePaneDecorator<'a> {
    active_pane: &'a Entity<Pane>,
    workspace: &'a WeakEntity<Workspace>,
}

impl<'a> ActivePaneDecorator<'a> {
    pub fn new(active_pane: &'a Entity<Pane>, workspace: &'a WeakEntity<Workspace>) -> Self {
        Self {
            active_pane,
            workspace,
        }
    }
}

impl PaneLeaderDecorator for ActivePaneDecorator<'_> {
    fn decorate(&self, _: &Entity<Pane>, _: &App) -> LeaderDecoration {
        LeaderDecoration::default()
    }
    fn active_pane(&self) -> &Entity<Pane> {
        self.active_pane
    }

    fn workspace(&self) -> &WeakEntity<Workspace> {
        self.workspace
    }
}

impl PaneLeaderDecorator for PaneRenderContext<'_> {
    fn decorate(&self, pane: &Entity<Pane>, cx: &App) -> LeaderDecoration {
        let follower_state = self.follower_states.iter().find_map(|(leader_id, state)| {
            if state.center_pane == *pane {
                Some((*leader_id, state))
            } else {
                None
            }
        });
        let Some((leader_id, follower_state)) = follower_state else {
            return LeaderDecoration::default();
        };

        let mut leader_color;
        let status_box;
        match leader_id {
            CollaboratorId::PeerId(peer_id) => {
                let Some(leader) = self
                    .active_call
                    .as_ref()
                    .and_then(|call| call.remote_participant_for_peer_id(peer_id, cx))
                else {
                    return LeaderDecoration::default();
                };

                let is_in_unshared_view = follower_state.active_view_id.is_some_and(|view_id| {
                    !follower_state
                        .items_by_leader_view_id
                        .contains_key(&view_id)
                });

                let mut leader_join_data = None;
                let leader_status_box = match leader.location {
                    ParticipantLocation::SharedProject {
                        project_id: leader_project_id,
                    } => {
                        if Some(leader_project_id) == self.project.read(cx).remote_id() {
                            is_in_unshared_view.then(|| {
                                Label::new(format!(
                                    "{} is in an unshared pane",
                                    leader.user.username
                                ))
                            })
                        } else {
                            leader_join_data = Some((leader_project_id, leader.user.legacy_id));
                            Some(Label::new(format!(
                                "Follow {} to their active project",
                                leader.user.username,
                            )))
                        }
                    }
                    ParticipantLocation::UnsharedProject => Some(Label::new(format!(
                        "{} is viewing an unshared Zed project",
                        leader.user.username
                    ))),
                    ParticipantLocation::External => Some(Label::new(format!(
                        "{} is viewing a window outside of Zed",
                        leader.user.username
                    ))),
                };
                status_box = leader_status_box.map(|status| {
                    div()
                        .absolute()
                        .w_96()
                        .bottom_3()
                        .right_3()
                        .elevation_2(cx)
                        .p_1()
                        .child(status)
                        .when_some(
                            leader_join_data,
                            |this, (leader_project_id, leader_user_id)| {
                                let app_state = self.app_state.clone();
                                this.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    move |_, window, cx| {
                                        crate::join_in_room_project(
                                            leader_project_id,
                                            leader_user_id,
                                            app_state.clone(),
                                            cx,
                                        )
                                        .detach_and_prompt_err(
                                            "Failed to join project",
                                            window,
                                            cx,
                                            |error, _, _| Some(format!("{error:#}")),
                                        );
                                    },
                                )
                            },
                        )
                        .into_any_element()
                });
                leader_color = cx
                    .theme()
                    .players()
                    .color_for_participant(leader.participant_index.0)
                    .cursor;
            }
            CollaboratorId::Agent => {
                status_box = None;
                leader_color = cx.theme().players().agent().cursor;
            }
        }

        let is_in_panel = follower_state.dock_pane.is_some();
        if is_in_panel {
            leader_color.fade_out(0.75);
        } else {
            leader_color.fade_out(0.3);
        }

        LeaderDecoration {
            status_box,
            border: Some(leader_color),
        }
    }

    fn active_pane(&self) -> &Entity<Pane> {
        self.active_pane
    }

    fn workspace(&self) -> &WeakEntity<Workspace> {
        self.workspace
    }
}

impl Member {
    fn new_axis_with_size_hint(
        old_pane: Entity<Pane>,
        new_pane: Entity<Pane>,
        direction: SplitDirection,
        size_hint: Option<SplitSizeHint>,
        available_size: Option<Pixels>,
    ) -> Self {
        use Axis::*;
        use SplitDirection::*;

        let axis = match direction {
            Up | Down => Vertical,
            Left | Right => Horizontal,
        };

        let members = match direction {
            Up | Left => vec![Member::Pane(new_pane), Member::Pane(old_pane)],
            Down | Right => vec![Member::Pane(old_pane), Member::Pane(new_pane)],
        };

        let flexes = size_hint
            .filter(|_| axis == Axis::Horizontal)
            .and_then(|hint| Some((hint, hint.available_size.or(available_size)?)))
            .and_then(|(hint, available_size)| {
                split_flexes_for_inserted_size(
                    available_size,
                    hint.inserted_size,
                    direction.increasing(),
                )
            });

        match flexes {
            Some(flexes) => Member::Axis(PaneAxis::load(axis, members, Some(flexes))),
            None => Member::Axis(PaneAxis::new(axis, members)),
        }
    }

    fn first_pane(&self) -> Entity<Pane> {
        match self {
            Member::Axis(axis) => axis.members[0].first_pane(),
            Member::Pane(pane) => pane.clone(),
        }
    }

    fn first_visible_pane(&self, cx: &App) -> Option<Entity<Pane>> {
        match self {
            Member::Axis(axis) => axis
                .members
                .iter()
                .find_map(|member| member.first_visible_pane(cx)),
            Member::Pane(pane) => pane.read(cx).is_visible().then(|| pane.clone()),
        }
    }

    fn last_pane(&self) -> Entity<Pane> {
        match self {
            Member::Axis(axis) => axis.members.last().unwrap().last_pane(),
            Member::Pane(pane) => pane.clone(),
        }
    }

    pub fn render(
        &self,
        basis: usize,
        zoomed: Option<&AnyWeakView>,
        maximized: Option<&WeakEntity<Pane>>,
        render_cx: &dyn PaneLeaderDecorator,
        window: &mut Window,
        cx: &mut App,
    ) -> PaneRenderResult {
        match self {
            Member::Pane(pane) => {
                if !pane.read(cx).is_visible() {
                    return PaneRenderResult {
                        element: div().into_any(),
                        contains_active_pane: false,
                        #[cfg(any(test, feature = "test-support"))]
                        decorated_pane_ix: None,
                    };
                }

                if zoomed == Some(&pane.downgrade().into()) {
                    return PaneRenderResult {
                        element: div().into_any(),
                        contains_active_pane: false,
                        #[cfg(any(test, feature = "test-support"))]
                        decorated_pane_ix: None,
                    };
                }

                let is_maximized = if let Some(maximized) = maximized {
                    if maximized.upgrade().as_ref() != Some(pane) {
                        return PaneRenderResult {
                            element: div().into_any(),
                            contains_active_pane: false,
                            #[cfg(any(test, feature = "test-support"))]
                            decorated_pane_ix: None,
                        };
                    }
                    true
                } else {
                    false
                };

                let decoration = render_cx.decorate(pane, cx);
                let is_active = pane == render_cx.active_pane();

                let pane = div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .when(is_maximized, |this| {
                        this.bg(cx.theme().colors().background).shadow_lg()
                    })
                    .child(
                        AnyView::from(pane.clone())
                            .cached(StyleRefinement::default().v_flex().size_full()),
                    )
                    .when_some(decoration.border, |this, color| {
                        this.child(
                            div()
                                .absolute()
                                .size_full()
                                .left_0()
                                .top_0()
                                .border_2()
                                .border_color(color),
                        )
                    })
                    .children(decoration.status_box);

                PaneRenderResult {
                    element: div()
                        .relative()
                        .flex_1()
                        .size_full()
                        .when(is_maximized, |this| this.p_2())
                        .child(pane)
                        .into_any(),
                    contains_active_pane: is_active,
                    #[cfg(any(test, feature = "test-support"))]
                    decorated_pane_ix: None,
                }
            }
            Member::Axis(axis) => axis.render(basis + 1, zoomed, maximized, render_cx, window, cx),
        }
    }

    pub fn contains_pane(&self, needle: &Entity<Pane>) -> bool {
        match self {
            Member::Pane(pane) => pane == needle,
            Member::Axis(axis) => axis
                .members
                .iter()
                .any(|member| member.contains_pane(needle)),
        }
    }

    fn collect_panes<'a>(&'a self, panes: &mut Vec<&'a Entity<Pane>>) {
        match self {
            Member::Axis(axis) => {
                for member in &axis.members {
                    member.collect_panes(panes);
                }
            }
            Member::Pane(pane) => panes.push(pane),
        }
    }

    fn collect_pane_bounds(&self, panes: &mut Vec<(Entity<Pane>, Bounds<Pixels>)>) {
        match self {
            Member::Axis(axis) => axis.collect_pane_bounds(panes),
            Member::Pane(_) => {}
        }
    }

    fn invert_pane_axies(&mut self) {
        match self {
            Self::Axis(axis) => {
                axis.axis = axis.axis.invert();
                for member in axis.members.iter_mut() {
                    member.invert_pane_axies();
                }
            }
            Self::Pane(_) => {}
        }
    }

    fn normalize_same_axis(&mut self) {
        if let Self::Axis(axis) = self {
            axis.normalize_same_axis();
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaneAxis {
    pub axis: Axis,
    pub members: Vec<Member>,
    pub flexes: Arc<Mutex<Vec<f32>>>,
    pub bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
}

impl PaneAxis {
    pub fn new(axis: Axis, members: Vec<Member>) -> Self {
        let flexes = Arc::new(Mutex::new(vec![1.; members.len()]));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        }
    }

    pub fn load(axis: Axis, members: Vec<Member>, flexes: Option<Vec<f32>>) -> Self {
        let flexes = normalize_flexes(members.len(), flexes.unwrap_or_default());
        let flexes = Arc::new(Mutex::new(flexes));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        let mut axis = Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        };
        axis.normalize_same_axis();
        axis
    }

    fn split(
        &mut self,
        old_pane: &Entity<Pane>,
        new_pane: &Entity<Pane>,
        direction: SplitDirection,
        size_hint: Option<SplitSizeHint>,
        cx: &App,
    ) -> bool {
        let bounding_boxes = self.bounding_boxes.lock().clone();
        for (idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if axis.split(old_pane, new_pane, direction, size_hint, cx) {
                        return true;
                    }
                }
                Member::Pane(pane) => {
                    if pane == old_pane {
                        if direction.axis() == self.axis {
                            let insertion_ix = if direction.increasing() { idx + 1 } else { idx };
                            self.insert_pane(insertion_ix, idx, new_pane, size_hint, cx);
                        } else {
                            let available_size = bounding_boxes
                                .get(idx)
                                .and_then(|bounds| {
                                    bounds.map(|bounds| bounds.size.along(direction.axis()))
                                })
                                .map(|size| Pixels::max(size - workspace_card_gap(cx), px(0.)));
                            *member = Member::new_axis_with_size_hint(
                                old_pane.clone(),
                                new_pane.clone(),
                                direction,
                                size_hint,
                                available_size,
                            );
                        }
                        return true;
                    }
                }
            }
        }
        false
    }

    fn insert_pane(
        &mut self,
        idx: usize,
        split_ix: usize,
        new_pane: &Entity<Pane>,
        size_hint: Option<SplitSizeHint>,
        cx: &App,
    ) {
        let visible_members_are_distributed = self.visible_members_are_distributed(cx);
        let mut flexes = normalize_flexes(self.members.len(), self.flexes.lock().clone());

        self.members.insert(idx, Member::Pane(new_pane.clone()));

        if let Some((old_flex, inserted_flex)) =
            self.split_flex_for_inserted_size(split_ix, size_hint, &flexes, cx)
        {
            if let Some(flex) = flexes.get_mut(split_ix) {
                *flex = old_flex;
            }
            flexes.insert(idx, inserted_flex);
        } else if visible_members_are_distributed {
            flexes.insert(idx, 1.);
            for ix in self.visible_member_indices(cx) {
                flexes[ix] = 1.;
            }
        } else {
            let split_flex = flexes
                .get(split_ix)
                .copied()
                .unwrap_or(1.)
                .max(f32::EPSILON)
                / 2.;
            if let Some(flex) = flexes.get_mut(split_ix) {
                *flex = split_flex;
            }
            flexes.insert(idx, split_flex);
        }

        *self.flexes.lock() = flexes;
        *self.bounding_boxes.lock() = vec![None; self.members.len()];
    }

    fn split_flex_for_inserted_size(
        &self,
        split_ix: usize,
        size_hint: Option<SplitSizeHint>,
        flexes: &[f32],
        cx: &App,
    ) -> Option<(f32, f32)> {
        if self.axis != Axis::Horizontal {
            return None;
        }

        let hint = size_hint?;
        let split_size = hint.available_size.or_else(|| {
            self.bounding_boxes
                .lock()
                .get(split_ix)
                .and_then(|bounds| *bounds)
                .map(|bounds| bounds.size.width - workspace_card_gap(cx))
        })?;
        let (old_ratio, inserted_ratio) =
            split_ratios_for_inserted_size(split_size, hint.inserted_size)?;
        let split_flex = flexes.get(split_ix).copied()?.max(f32::EPSILON);
        Some((split_flex * old_ratio, split_flex * inserted_ratio))
    }

    fn insert_moved_pane(&mut self, idx: usize, pane: &Entity<Pane>) {
        self.members.insert(idx, Member::Pane(pane.clone()));
        let mut flexes = normalize_flexes(
            self.members.len().saturating_sub(1),
            self.flexes.lock().clone(),
        );
        flexes.insert(idx, 1.);
        *self.flexes.lock() = flexes;
        *self.bounding_boxes.lock() = vec![None; self.members.len()];
    }

    fn find_pane_at_border(&self, direction: SplitDirection) -> Option<&Entity<Pane>> {
        if self.axis != direction.axis() {
            return None;
        }
        let member = if direction.increasing() {
            self.members.last()
        } else {
            self.members.first()
        };
        member.and_then(|e| match e {
            Member::Pane(pane) => Some(pane),
            Member::Axis(_) => None,
        })
    }

    fn horizontal_size_for_pane(&self, pane: &Entity<Pane>) -> Option<Pixels> {
        let bounding_boxes = self.bounding_boxes.lock();

        for (idx, member) in self.members.iter().enumerate() {
            if self.axis == Axis::Horizontal && member.contains_pane(pane) {
                return bounding_boxes
                    .get(idx)
                    .and_then(|bounds| bounds.map(|bounds| bounds.size.width));
            }

            if let Member::Axis(axis) = member
                && let Some(size) = axis.horizontal_size_for_pane(pane)
            {
                return Some(size);
            }
        }

        None
    }

    fn remove(&mut self, pane_to_remove: &Entity<Pane>, cx: &App) -> Result<Option<Member>> {
        let mut found_pane = false;
        let mut remove_member = None;
        for (idx, member) in self.members.iter_mut().enumerate() {
            match member {
                Member::Axis(axis) => {
                    if let Ok(last_pane) = axis.remove(pane_to_remove, cx) {
                        if let Some(last_pane) = last_pane {
                            *member = last_pane;
                        }
                        found_pane = true;
                        break;
                    }
                }
                Member::Pane(pane) => {
                    if pane == pane_to_remove {
                        found_pane = true;
                        remove_member = Some(idx);
                        break;
                    }
                }
            }
        }

        if found_pane {
            if let Some(idx) = remove_member {
                let visible_members_are_distributed = self.visible_members_are_distributed(cx);
                let visible_indices = self.visible_member_indices(cx);
                let removed_was_visible = visible_indices.contains(&idx);
                let mut flexes = normalize_flexes(self.members.len(), self.flexes.lock().clone());
                let removed_flex = flexes.get(idx).copied().unwrap_or(1.);
                let recipient_ix = if removed_was_visible && !visible_members_are_distributed {
                    visible_indices
                        .iter()
                        .rev()
                        .copied()
                        .find(|visible_ix| *visible_ix < idx)
                        .or_else(|| {
                            visible_indices
                                .iter()
                                .copied()
                                .find(|visible_ix| *visible_ix > idx)
                        })
                } else {
                    None
                };

                self.members.remove(idx);
                if idx < flexes.len() {
                    flexes.remove(idx);
                }

                if visible_members_are_distributed {
                    for ix in self.visible_member_indices(cx) {
                        flexes[ix] = 1.;
                    }
                } else if let Some(recipient_ix) = recipient_ix {
                    let recipient_ix = if recipient_ix > idx {
                        recipient_ix - 1
                    } else {
                        recipient_ix
                    };
                    if let Some(flex) = flexes.get_mut(recipient_ix) {
                        *flex += removed_flex;
                    }
                }

                *self.flexes.lock() = flexes;
                *self.bounding_boxes.lock() = vec![None; self.members.len()];
            }

            if self.members.len() == 1 {
                let result = self.members.pop();
                *self.flexes.lock() = vec![1.; self.members.len()];
                Ok(result)
            } else {
                Ok(None)
            }
        } else {
            anyhow::bail!("Pane not found");
        }
    }

    fn reset_pane_sizes(&self) {
        *self.flexes.lock() = vec![1.; self.members.len()];
        for member in self.members.iter() {
            if let Member::Axis(axis) = member {
                axis.reset_pane_sizes();
            }
        }
    }

    fn visible_member_indices(&self, cx: &App) -> Vec<usize> {
        self.members
            .iter()
            .enumerate()
            .filter_map(|(ix, member)| member.is_visible(cx).then_some(ix))
            .collect()
    }

    fn visible_members_are_distributed(&self, cx: &App) -> bool {
        const DISTRIBUTED_SIZE_TOLERANCE: f32 = 2.;

        let visible_indices = self.visible_member_indices(cx);
        if visible_indices.len() <= 1 {
            return true;
        }

        let bounding_boxes = self.bounding_boxes.lock();
        let rendered_sizes = visible_indices
            .iter()
            .map(|ix| {
                bounding_boxes
                    .get(*ix)
                    .and_then(|bounds| bounds.map(|bounds| bounds.size.along(self.axis).as_f32()))
            })
            .collect::<Option<Vec<_>>>();

        if let Some(rendered_sizes) = rendered_sizes {
            let (min_size, max_size) = rendered_sizes
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), size| {
                    (min.min(*size), max.max(*size))
                });
            return max_size - min_size <= DISTRIBUTED_SIZE_TOLERANCE;
        }

        let flexes = normalize_flexes(self.members.len(), self.flexes.lock().clone());
        let (min_flex, max_flex) = visible_indices
            .iter()
            .filter_map(|ix| flexes.get(*ix))
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), flex| {
                (min.min(*flex), max.max(*flex))
            });

        (max_flex - min_flex).abs() <= f32::EPSILON
    }

    fn normalize_same_axis(&mut self) {
        for member in &mut self.members {
            member.normalize_same_axis();
        }

        if !self
            .members
            .iter()
            .any(|member| matches!(member, Member::Axis(axis) if axis.axis == self.axis))
        {
            let flexes = normalize_flexes(self.members.len(), self.flexes.lock().clone());
            *self.flexes.lock() = flexes;
            return;
        }

        let members = mem::take(&mut self.members);
        let flexes = normalize_flexes(members.len(), self.flexes.lock().clone());
        let mut flattened_members = Vec::with_capacity(members.len());
        let mut flattened_flexes = Vec::with_capacity(flexes.len());

        for (member, flex) in members.into_iter().zip(flexes) {
            match member {
                Member::Axis(axis) if axis.axis == self.axis => {
                    let child_flexes =
                        normalize_flexes(axis.members.len(), axis.flexes.lock().clone());
                    let child_total_flex = child_flexes.iter().sum::<f32>().max(0.001);
                    flattened_members.extend(axis.members);
                    flattened_flexes.extend(
                        child_flexes
                            .into_iter()
                            .map(|child_flex| flex * child_flex / child_total_flex),
                    );
                }
                member => {
                    flattened_members.push(member);
                    flattened_flexes.push(flex);
                }
            }
        }

        self.members = flattened_members;
        *self.flexes.lock() = normalize_flexes(self.members.len(), flattened_flexes);
        *self.bounding_boxes.lock() = vec![None; self.members.len()];
    }

    fn resize(
        &mut self,
        pane: &Entity<Pane>,
        axis: Axis,
        amount: Pixels,
        bounds: &Bounds<Pixels>,
    ) -> Option<bool> {
        let found_pane = self
            .members
            .iter()
            .any(|member| matches!(member, Member::Pane(p) if p == pane));

        if found_pane && self.axis != axis {
            return Some(false); // pane found but this is not the correct axis direction
        }
        let mut found_axis_index: Option<usize> = None;
        if !found_pane {
            for (i, pa) in self.members.iter_mut().enumerate() {
                if let Member::Axis(pa) = pa
                    && let Some(done) = pa.resize(pane, axis, amount, bounds)
                {
                    if done {
                        return Some(true); // pane found and operations already done
                    } else if self.axis != axis {
                        return Some(false); // pane found but this is not the correct axis direction
                    } else {
                        found_axis_index = Some(i); // pane found and this is correct direction
                    }
                }
            }
            found_axis_index?; // no pane found
        }

        let min_size = match axis {
            Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
            Axis::Vertical => px(VERTICAL_MIN_SIZE),
        };
        let mut flexes = self.flexes.lock();

        let ix = if found_pane {
            self.members.iter().position(|m| {
                if let Member::Pane(p) = m {
                    p == pane
                } else {
                    false
                }
            })
        } else {
            found_axis_index
        };

        if ix.is_none() {
            return Some(true);
        }

        let ix = ix.unwrap_or(0);

        let (visible_indices, available_size) = {
            let bounding_boxes = self.bounding_boxes.lock();
            let indices = bounding_boxes
                .iter()
                .enumerate()
                .filter_map(|(ix, bounds)| bounds.is_some().then_some(ix))
                .collect::<Vec<_>>();
            if indices.is_empty() {
                ((0..self.members.len()).collect(), bounds.size.along(axis))
            } else {
                let available_size = indices
                    .iter()
                    .filter_map(|ix| bounding_boxes[*ix].map(|bounds| bounds.size.along(axis)))
                    .sum();
                (indices, available_size)
            }
        };
        let Some(visible_ix) = visible_indices
            .iter()
            .position(|visible_member_ix| *visible_member_ix == ix)
        else {
            return Some(true);
        };

        if visible_ix + 1 == visible_indices.len() {
            if visible_ix == 0 {
                return Some(true);
            }
            resize_adjacent_visible_pair(
                flexes.as_mut_slice(),
                &visible_indices,
                visible_indices[visible_ix - 1],
                ix,
                -amount,
                available_size,
                min_size,
            );
        } else {
            resize_adjacent_visible_pair(
                flexes.as_mut_slice(),
                &visible_indices,
                ix,
                visible_indices[visible_ix + 1],
                amount,
                available_size,
                min_size,
            );
        }
        Some(true)
    }

    fn swap(&mut self, from: &Entity<Pane>, to: &Entity<Pane>) {
        for member in self.members.iter_mut() {
            match member {
                Member::Axis(axis) => axis.swap(from, to),
                Member::Pane(pane) => {
                    if pane == from {
                        *member = Member::Pane(to.clone());
                    } else if pane == to {
                        *member = Member::Pane(from.clone())
                    }
                }
            }
        }
    }

    fn bounding_box_for_pane(&self, pane: &Entity<Pane>) -> Option<Bounds<Pixels>> {
        debug_assert!(self.members.len() == self.bounding_boxes.lock().len());

        for (idx, member) in self.members.iter().enumerate() {
            match member {
                Member::Pane(found) => {
                    if pane == found {
                        return self.bounding_boxes.lock()[idx];
                    }
                }
                Member::Axis(axis) => {
                    if let Some(rect) = axis.bounding_box_for_pane(pane) {
                        return Some(rect);
                    }
                }
            }
        }
        None
    }

    fn pane_at_pixel_position(&self, coordinate: Point<Pixels>) -> Option<&Entity<Pane>> {
        debug_assert!(self.members.len() == self.bounding_boxes.lock().len());

        let bounding_boxes = self.bounding_boxes.lock();

        for (idx, member) in self.members.iter().enumerate() {
            if let Some(coordinates) = bounding_boxes[idx]
                && coordinates.contains(&coordinate)
            {
                return match member {
                    Member::Pane(found) => Some(found),
                    Member::Axis(axis) => axis.pane_at_pixel_position(coordinate),
                };
            }
        }
        None
    }

    fn collect_pane_bounds(&self, panes: &mut Vec<(Entity<Pane>, Bounds<Pixels>)>) {
        debug_assert!(self.members.len() == self.bounding_boxes.lock().len());

        let bounding_boxes = self.bounding_boxes.lock();
        for (idx, member) in self.members.iter().enumerate() {
            let Some(bounds) = bounding_boxes[idx] else {
                continue;
            };

            match member {
                Member::Pane(pane) => panes.push((pane.clone(), bounds)),
                Member::Axis(axis) => axis.collect_pane_bounds(panes),
            }
        }
    }

    fn full_height_column_count(&self) -> usize {
        match self.axis {
            Axis::Horizontal => self
                .members
                .iter()
                .map(Member::full_height_column_count)
                .sum::<usize>()
                .max(1),
            Axis::Vertical => self
                .members
                .iter()
                .map(Member::full_height_column_count)
                .max()
                .unwrap_or(1),
        }
    }

    fn render(
        &self,
        basis: usize,
        zoomed: Option<&AnyWeakView>,
        maximized: Option<&WeakEntity<Pane>>,
        render_cx: &dyn PaneLeaderDecorator,
        window: &mut Window,
        cx: &mut App,
    ) -> PaneRenderResult {
        if let Some(maximized) = maximized {
            if let Some(maximized_pane) = maximized.upgrade() {
                for member in &self.members {
                    if member.contains_pane(&maximized_pane) {
                        return member.render(
                            basis,
                            zoomed,
                            Some(maximized),
                            render_cx,
                            window,
                            cx,
                        );
                    }
                }
            }
        }

        debug_assert!(self.members.len() == self.flexes.lock().len());
        let mut active_pane_ix = None;
        let mut contains_active_pane = false;
        let mut is_leaf_pane = Vec::new();
        let mut visible_indices = Vec::new();

        let rendered_children = self
            .members
            .iter()
            .enumerate()
            .filter_map(|(ix, member)| {
                if !member.is_visible(cx) {
                    member.clear_bounding_boxes();
                    return None;
                }

                let visible_ix = visible_indices.len();
                visible_indices.push(ix);
                match member {
                    Member::Pane(pane) => {
                        is_leaf_pane.push(true);
                        if pane == render_cx.active_pane() {
                            active_pane_ix =
                                pane.read(cx).has_focus(window, cx).then_some(visible_ix);
                            contains_active_pane = true;
                        }
                    }
                    Member::Axis(_) => {
                        is_leaf_pane.push(false);
                    }
                }

                let result = member.render((basis + ix) * 10, zoomed, None, render_cx, window, cx);
                if result.contains_active_pane {
                    contains_active_pane = true;
                }
                Some(result.element.into_any_element())
            })
            .collect::<Vec<_>>();

        if rendered_children.is_empty() {
            return PaneRenderResult {
                element: div().into_any(),
                contains_active_pane: false,
                #[cfg(any(test, feature = "test-support"))]
                decorated_pane_ix: None,
            };
        }

        let element = pane_axis(
            self.axis,
            basis,
            self.flexes.clone(),
            self.bounding_boxes.clone(),
            render_cx.workspace().clone(),
        )
        .with_visible_indices(visible_indices)
        .with_is_leaf_pane_mask(is_leaf_pane)
        .children(rendered_children)
        .with_active_pane(active_pane_ix)
        .into_any_element();

        PaneRenderResult {
            element,
            contains_active_pane,
            #[cfg(any(test, feature = "test-support"))]
            decorated_pane_ix: active_pane_ix,
        }
    }
}

fn normalize_flexes(member_count: usize, flexes: Vec<f32>) -> Vec<f32> {
    if member_count == 0 {
        return Vec::new();
    }

    let total_flex = flexes.iter().copied().sum::<f32>();
    if flexes.len() != member_count
        || !total_flex.is_finite()
        || total_flex <= f32::EPSILON
        || flexes.iter().any(|flex| !flex.is_finite() || *flex <= 0.)
    {
        return vec![1.; member_count];
    }

    let scale = member_count as f32 / total_flex;
    flexes.into_iter().map(|flex| flex * scale).collect()
}

fn split_flexes_for_inserted_size(
    available_size: Pixels,
    inserted_size: Pixels,
    insert_after: bool,
) -> Option<Vec<f32>> {
    let (old_ratio, inserted_ratio) =
        split_ratios_for_inserted_size(available_size, inserted_size)?;
    let old_flex = old_ratio * 2.;
    let inserted_flex = inserted_ratio * 2.;
    Some(if insert_after {
        vec![old_flex, inserted_flex]
    } else {
        vec![inserted_flex, old_flex]
    })
}

fn split_ratios_for_inserted_size(
    available_size: Pixels,
    inserted_size: Pixels,
) -> Option<(f32, f32)> {
    let available_size = available_size.as_f32();
    if !available_size.is_finite() || available_size <= 0. {
        return None;
    }

    let min_size = HORIZONTAL_MIN_SIZE;
    let max_inserted_size = available_size - min_size;
    if max_inserted_size < min_size {
        return None;
    }

    let inserted_size = inserted_size.as_f32().clamp(min_size, max_inserted_size);
    let old_size = available_size - inserted_size;

    Some((old_size / available_size, inserted_size / available_size))
}

fn resize_adjacent_visible_pair(
    flexes: &mut [f32],
    visible_indices: &[usize],
    current_ix: usize,
    next_ix: usize,
    pixel_delta: Pixels,
    available_size: Pixels,
    min_size: Pixels,
) -> bool {
    let requested_delta = pixel_delta.as_f32();
    if requested_delta.abs() <= f32::EPSILON || available_size <= px(0.) {
        return false;
    }

    let Some(current_visible_ix) = visible_indices
        .iter()
        .position(|visible_ix| *visible_ix == current_ix)
    else {
        return false;
    };
    let Some(next_visible_ix) = visible_indices
        .iter()
        .position(|visible_ix| *visible_ix == next_ix)
    else {
        return false;
    };
    if next_visible_ix != current_visible_ix + 1 {
        return false;
    }

    let visible_total_flex = visible_indices.iter().map(|ix| flexes[*ix]).sum::<f32>();
    if !visible_total_flex.is_finite() || visible_total_flex <= f32::EPSILON {
        return false;
    }

    let current_size = available_size.as_f32() * flexes[current_ix] / visible_total_flex;
    let next_size = available_size.as_f32() * flexes[next_ix] / visible_total_flex;
    let min_size = min_size.as_f32();
    let min_delta = min_size - current_size;
    let max_delta = next_size - min_size;

    if min_delta > max_delta {
        return false;
    }

    let actual_delta = requested_delta.clamp(min_delta, max_delta);
    if actual_delta.abs() <= f32::EPSILON {
        return false;
    }

    let flex_delta = actual_delta * visible_total_flex / available_size.as_f32();
    flexes[current_ix] += flex_delta;
    flexes[next_ix] -= flex_delta;
    true
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Up,
    Down,
    Left,
    Right,
}

impl std::fmt::Display for SplitDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitDirection::Up => write!(f, "up"),
            SplitDirection::Down => write!(f, "down"),
            SplitDirection::Left => write!(f, "left"),
            SplitDirection::Right => write!(f, "right"),
        }
    }
}

impl SplitDirection {
    pub fn all() -> [Self; 4] {
        [Self::Up, Self::Down, Self::Left, Self::Right]
    }

    pub fn vertical(cx: &mut App) -> Self {
        match WorkspaceSettings::get_global(cx).pane_split_direction_vertical {
            PaneSplitDirectionVertical::Left => SplitDirection::Left,
            PaneSplitDirectionVertical::Right => SplitDirection::Right,
        }
    }

    pub fn horizontal(cx: &mut App) -> Self {
        match WorkspaceSettings::get_global(cx).pane_split_direction_horizontal {
            PaneSplitDirectionHorizontal::Down => SplitDirection::Down,
            PaneSplitDirectionHorizontal::Up => SplitDirection::Up,
        }
    }

    pub fn edge(&self, rect: Bounds<Pixels>) -> Pixels {
        match self {
            Self::Up => rect.origin.y,
            Self::Down => rect.bottom_left().y,
            Self::Left => rect.bottom_left().x,
            Self::Right => rect.bottom_right().x,
        }
    }

    pub fn along_edge(&self, bounds: Bounds<Pixels>, length: Pixels) -> Bounds<Pixels> {
        match self {
            Self::Up => Bounds {
                origin: bounds.origin,
                size: size(bounds.size.width, length),
            },
            Self::Down => Bounds {
                origin: point(bounds.bottom_left().x, bounds.bottom_left().y - length),
                size: size(bounds.size.width, length),
            },
            Self::Left => Bounds {
                origin: bounds.origin,
                size: size(length, bounds.size.height),
            },
            Self::Right => Bounds {
                origin: point(bounds.bottom_right().x - length, bounds.bottom_left().y),
                size: size(length, bounds.size.height),
            },
        }
    }

    pub fn axis(&self) -> Axis {
        match self {
            Self::Up | Self::Down => Axis::Vertical,
            Self::Left | Self::Right => Axis::Horizontal,
        }
    }

    pub fn increasing(&self) -> bool {
        match self {
            Self::Left | Self::Up => false,
            Self::Down | Self::Right => true,
        }
    }

    pub fn opposite(&self) -> SplitDirection {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

mod element {
    use std::mem;
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    use gpui::{
        Along, AnyElement, App, Axis, BorderStyle, Bounds, CursorStyle, Element, GlobalElementId,
        Hitbox, HitboxBehavior, IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
        ParentElement, Pixels, Point, Size, Style, WeakEntity, Window, px, relative, size,
    };
    use parking_lot::Mutex;
    use settings::Settings;
    use smallvec::SmallVec;
    use ui::prelude::*;
    use util::ResultExt;

    use crate::{Workspace, WorkspaceSettings, workspace_card_gap};

    use super::{
        HANDLE_HITBOX_SIZE, HORIZONTAL_MIN_SIZE, VERTICAL_MIN_SIZE, resize_adjacent_visible_pair,
    };

    pub(super) fn pane_axis(
        axis: Axis,
        basis: usize,
        flexes: Arc<Mutex<Vec<f32>>>,
        bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
        workspace: WeakEntity<Workspace>,
    ) -> PaneAxisElement {
        PaneAxisElement {
            axis,
            basis,
            flexes,
            bounding_boxes,
            children: SmallVec::new(),
            active_pane_ix: None,
            workspace,
            is_leaf_pane_mask: Vec::new(),
            visible_indices: Vec::new(),
        }
    }

    pub struct PaneAxisElement {
        axis: Axis,
        basis: usize,
        /// Equivalent to ColumnWidths (but in terms of flexes instead of percentages)
        /// For example, flexes "1.33, 1, 1", instead of "40%, 30%, 30%"
        flexes: Arc<Mutex<Vec<f32>>>,
        bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
        children: SmallVec<[AnyElement; 2]>,
        active_pane_ix: Option<usize>,
        workspace: WeakEntity<Workspace>,
        // Track which children are leaf panes (Member::Pane) vs axes (Member::Axis)
        is_leaf_pane_mask: Vec<bool>,
        visible_indices: Vec<usize>,
    }

    pub struct PaneAxisLayout {
        dragged_handle: Rc<RefCell<Option<usize>>>,
        children: Vec<PaneAxisChildLayout>,
        visible_indices: Vec<usize>,
    }

    struct PaneAxisChildLayout {
        bounds: Bounds<Pixels>,
        element: AnyElement,
        handle: Option<PaneAxisHandleLayout>,
        is_leaf_pane: bool,
        original_ix: usize,
    }

    struct PaneAxisHandleLayout {
        hitbox: Hitbox,
        current_ix: usize,
        next_ix: usize,
    }

    impl PaneAxisElement {
        pub fn with_active_pane(mut self, active_pane_ix: Option<usize>) -> Self {
            self.active_pane_ix = active_pane_ix;
            self
        }

        pub fn with_is_leaf_pane_mask(mut self, mask: Vec<bool>) -> Self {
            self.is_leaf_pane_mask = mask;
            self
        }

        pub fn with_visible_indices(mut self, indices: Vec<usize>) -> Self {
            self.visible_indices = indices;
            self
        }

        fn compute_resize(
            flexes: &Arc<Mutex<Vec<f32>>>,
            e: &MouseMoveEvent,
            current_ix: usize,
            next_ix: usize,
            visible_indices: &[usize],
            axis: Axis,
            child_start: Point<Pixels>,
            container_size: Size<Pixels>,
            workspace: WeakEntity<Workspace>,
            window: &mut Window,
            cx: &mut App,
        ) {
            let min_size = match axis {
                Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
                Axis::Vertical => px(VERTICAL_MIN_SIZE),
            };
            let mut flexes = flexes.lock();
            debug_assert!(flex_values_in_bounds(flexes.as_slice()));
            let visible_total_flex = visible_indices.iter().map(|ix| flexes[*ix]).sum::<f32>();
            if visible_total_flex <= 0. {
                return;
            }

            let available_size = Pixels::max(container_size.along(axis), px(0.001));
            let current_size = available_size * (flexes[current_ix] / visible_total_flex);

            let proposed_current_pixel_change =
                (e.position - child_start).along(axis) - current_size;
            if !resize_adjacent_visible_pair(
                flexes.as_mut_slice(),
                visible_indices,
                current_ix,
                next_ix,
                proposed_current_pixel_change,
                available_size,
                min_size,
            ) {
                return;
            }

            workspace
                .update(cx, |this, cx| this.serialize_workspace(window, cx))
                .log_err();
            cx.stop_propagation();
            window.refresh();
        }

        fn pixel_snap_bounds(window: &Window, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
            let origin = Point::new(
                window.pixel_snap(bounds.left()),
                window.pixel_snap(bounds.top()),
            );
            let corner = Point::new(
                window.pixel_snap(bounds.right()).max(origin.x),
                window.pixel_snap(bounds.bottom()).max(origin.y),
            );
            Bounds::from_corners(origin, corner)
        }

        fn layout_handle(
            axis: Axis,
            pane_bounds: Bounds<Pixels>,
            current_ix: usize,
            next_ix: usize,
            window: &mut Window,
            cx: &mut App,
        ) -> PaneAxisHandleLayout {
            let card_gap = workspace_card_gap(cx);
            let handle_bounds = Bounds {
                origin: pane_bounds.origin.apply_along(axis, |origin| {
                    origin + pane_bounds.size.along(axis) - px(HANDLE_HITBOX_SIZE / 2.)
                }),
                size: pane_bounds
                    .size
                    .apply_along(axis, |_| px(HANDLE_HITBOX_SIZE) + card_gap),
            };

            PaneAxisHandleLayout {
                hitbox: window.insert_hitbox(handle_bounds, HitboxBehavior::BlockMouse),
                current_ix,
                next_ix,
            }
        }
    }

    impl IntoElement for PaneAxisElement {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for PaneAxisElement {
        type RequestLayoutState = ();
        type PrepaintState = PaneAxisLayout;

        fn id(&self) -> Option<ElementId> {
            Some(self.basis.into())
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _global_id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (gpui::LayoutId, Self::RequestLayoutState) {
            let style = Style {
                flex_grow: 1.,
                flex_shrink: 1.,
                flex_basis: relative(0.).into(),
                size: size(relative(1.).into(), relative(1.).into()),
                ..Style::default()
            };
            (window.request_layout(style, None, cx), ())
        }

        fn prepaint(
            &mut self,
            global_id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            bounds: Bounds<Pixels>,
            _state: &mut Self::RequestLayoutState,
            window: &mut Window,
            cx: &mut App,
        ) -> PaneAxisLayout {
            let dragged_handle = window.with_element_state::<Rc<RefCell<Option<usize>>>, _>(
                global_id.unwrap(),
                |state, _cx| {
                    let state = state.unwrap_or_else(|| Rc::new(RefCell::new(None)));
                    (state.clone(), state)
                },
            );
            let flexes = self.flexes.lock().clone();
            let visible_indices = if self.visible_indices.is_empty() {
                (0..self.children.len()).collect::<Vec<_>>()
            } else {
                self.visible_indices.clone()
            };
            let len = visible_indices.len();
            debug_assert!(len == self.children.len());
            debug_assert!(visible_indices.iter().all(|ix| *ix < flexes.len()));
            debug_assert!(flex_values_in_bounds(flexes.as_slice()));

            let total_flex = visible_indices
                .iter()
                .map(|ix| flexes[*ix])
                .sum::<f32>()
                .max(0.001);

            let mut origin = bounds.origin;
            let card_gap = workspace_card_gap(cx);
            let gap_count = len.saturating_sub(1);
            let total_gap = card_gap * gap_count as f32;
            let available_size = Pixels::max(bounds.size.along(self.axis) - total_gap, px(0.0));
            let space_per_flex = available_size / total_flex;

            let mut bounding_boxes = self.bounding_boxes.lock();
            bounding_boxes.clear();
            bounding_boxes.resize(flexes.len(), None);

            let mut layout = PaneAxisLayout {
                dragged_handle,
                children: Vec::new(),
                visible_indices,
            };
            for (ix, mut child) in mem::take(&mut self.children).into_iter().enumerate() {
                let original_ix = layout.visible_indices[ix];
                let child_flex = flexes[original_ix];

                let raw_child_size = bounds
                    .size
                    .apply_along(self.axis, |_| space_per_flex * child_flex);
                let raw_child_bounds = Bounds {
                    origin,
                    size: raw_child_size,
                };
                // Pane axes bypass Taffy, so snap their child edges explicitly to avoid
                // 1dp border jitter when flex sizes or card gaps are fractional.
                let child_bounds = Self::pixel_snap_bounds(window, raw_child_bounds);

                bounding_boxes[original_ix] = Some(child_bounds);
                child.layout_as_root(child_bounds.size.into(), window, cx);
                child.prepaint_at(child_bounds.origin, window, cx);

                origin = origin.apply_along(self.axis, |val| {
                    val + raw_child_size.along(self.axis) + card_gap
                });

                let is_leaf_pane = self.is_leaf_pane_mask.get(ix).copied().unwrap_or(true);

                layout.children.push(PaneAxisChildLayout {
                    bounds: child_bounds,
                    element: child,
                    handle: None,
                    is_leaf_pane,
                    original_ix,
                })
            }

            for ix in 0..layout.children.len() {
                if ix < len - 1 {
                    let next_ix = layout.children[ix + 1].original_ix;
                    let child_layout = &mut layout.children[ix];
                    child_layout.handle = Some(Self::layout_handle(
                        self.axis,
                        child_layout.bounds,
                        child_layout.original_ix,
                        next_ix,
                        window,
                        cx,
                    ));
                }
            }

            layout
        }

        fn paint(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            bounds: gpui::Bounds<ui::prelude::Pixels>,
            _: &mut Self::RequestLayoutState,
            layout: &mut Self::PrepaintState,
            window: &mut Window,
            cx: &mut App,
        ) {
            for child in &mut layout.children {
                child.element.paint(window, cx);
            }

            let overlay_opacity = WorkspaceSettings::get(None, cx)
                .active_pane_modifiers
                .inactive_opacity
                .map(|val| val.0.clamp(0.0, 1.0))
                .and_then(|val| (val <= 1.).then_some(val));

            let mut overlay_background = cx.theme().colors().editor_background;
            if let Some(opacity) = overlay_opacity {
                overlay_background.fade_out(opacity);
            }

            let overlay_border = WorkspaceSettings::get(None, cx)
                .active_pane_modifiers
                .border_size
                .and_then(|val| (val >= 0.).then_some(val));

            let card_gap = workspace_card_gap(cx);

            for (ix, child) in &mut layout.children.iter_mut().enumerate() {
                if overlay_opacity.is_some() || overlay_border.is_some() {
                    // Keep active/inactive overlays inside the pane card border.
                    let overlay_bounds = Bounds {
                        origin: Point::new(
                            child.bounds.origin.x + px(1.),
                            child.bounds.origin.y + px(1.),
                        ),
                        size: Size {
                            width: Pixels::max(child.bounds.size.width - px(2.), px(0.)),
                            height: Pixels::max(child.bounds.size.height - px(2.), px(0.)),
                        },
                    };

                    if overlay_opacity.is_some()
                        && child.is_leaf_pane
                        && self.active_pane_ix != Some(ix)
                    {
                        window.paint_quad(gpui::fill(overlay_bounds, overlay_background));
                    }

                    if let Some(border) = overlay_border
                        && self.active_pane_ix == Some(ix)
                        && child.is_leaf_pane
                    {
                        window.paint_quad(gpui::quad(
                            overlay_bounds,
                            0.,
                            gpui::transparent_black(),
                            border,
                            cx.theme().colors().border_selected,
                            BorderStyle::Solid,
                        ));
                    }
                }

                if let Some(handle) = child.handle.as_mut() {
                    let cursor_style = match self.axis {
                        Axis::Vertical => CursorStyle::ResizeRow,
                        Axis::Horizontal => CursorStyle::ResizeColumn,
                    };

                    if layout
                        .dragged_handle
                        .borrow()
                        .is_some_and(|dragged_ix| dragged_ix == ix)
                    {
                        window.set_window_cursor_style(cursor_style);
                    } else {
                        window.set_cursor_style(cursor_style, &handle.hitbox);
                    }

                    window.on_mouse_event({
                        let dragged_handle = layout.dragged_handle.clone();
                        let flexes = self.flexes.clone();
                        let workspace = self.workspace.clone();
                        let handle_hitbox = handle.hitbox.clone();
                        move |e: &MouseDownEvent, phase, window, cx| {
                            if phase.bubble() && handle_hitbox.is_hovered(window) {
                                dragged_handle.replace(Some(ix));
                                if e.click_count >= 2 {
                                    let mut borrow = flexes.lock();
                                    *borrow = vec![1.; borrow.len()];
                                    workspace
                                        .update(cx, |this, cx| this.serialize_workspace(window, cx))
                                        .log_err();

                                    window.refresh();
                                }
                                cx.stop_propagation();
                            }
                        }
                    });
                    window.on_mouse_event({
                        let workspace = self.workspace.clone();
                        let dragged_handle = layout.dragged_handle.clone();
                        let flexes = self.flexes.clone();
                        let child_bounds = child.bounds;
                        let axis = self.axis;
                        let current_ix = handle.current_ix;
                        let next_ix = handle.next_ix;
                        let visible_indices = layout.visible_indices.clone();
                        move |e: &MouseMoveEvent, phase, window, cx| {
                            let dragged_handle = dragged_handle.borrow();
                            if phase.bubble() && *dragged_handle == Some(ix) {
                                Self::compute_resize(
                                    &flexes,
                                    e,
                                    current_ix,
                                    next_ix,
                                    &visible_indices,
                                    axis,
                                    child_bounds.origin,
                                    bounds.size.apply_along(axis, |size| {
                                        Pixels::max(
                                            size - card_gap
                                                * visible_indices.len().saturating_sub(1) as f32,
                                            px(0.0),
                                        )
                                    }),
                                    workspace.clone(),
                                    window,
                                    cx,
                                )
                            }
                        }
                    });
                }
            }

            window.on_mouse_event({
                let dragged_handle = layout.dragged_handle.clone();
                move |_: &MouseUpEvent, phase, _window, _cx| {
                    if phase.bubble() {
                        dragged_handle.replace(None);
                    }
                }
            });
        }
    }

    impl ParentElement for PaneAxisElement {
        fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
            self.children.extend(elements)
        }
    }

    fn flex_values_in_bounds(flexes: &[f32]) -> bool {
        (flexes.iter().copied().sum::<f32>() - flexes.len() as f32).abs() < 0.001
    }
}
