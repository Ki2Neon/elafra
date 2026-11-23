use std::collections::HashMap;

use gpui_kit::{
    Axis, Pixels, Size,
    base::PANEL_MIN_SIZE,
    component::dock::{NodeId, PaneNode, PaneRef, PanelId, PanelInfo, PanelState},
    px, size,
};

#[derive(Default)]
pub(super) struct DockProportions {
    splits: HashMap<NodeId, Split>,
}

struct Split {
    children: Vec<NodeId>,
    preferred: Vec<f64>,
    observed: Vec<Option<Pixels>>,
}

impl DockProportions {
    pub(super) fn capture(&mut self, root: &PaneNode, visible: &impl Fn(PanelId) -> bool) {
        let mut previous = std::mem::take(&mut self.splits);
        root.walk(&mut |node| {
            let PaneRef::Split {
                children, sizes, ..
            } = node.kind()
            else {
                return;
            };
            let child_ids: Vec<_> = children.iter().map(PaneNode::id).collect();
            let prior = previous
                .remove(&node.id())
                .filter(|split| split.children == child_ids);
            let mut preferred = prior
                .as_ref()
                .map_or_else(|| weights(sizes), |split| split.preferred.clone());
            if let Some(prior) = prior.filter(|prior| prior.observed != sizes) {
                // A divider change sets new shares for the currently visible panes.
                // Closed panes keep the share they will take on reopening.
                let shown: Vec<_> = children
                    .iter()
                    .map(|child| is_visible(child, visible))
                    .collect();
                let recorded = weights(sizes);
                let prior_visible: f64 = prior
                    .preferred
                    .iter()
                    .zip(&shown)
                    .filter_map(|(&share, &shown)| shown.then_some(share))
                    .sum();
                let recorded_visible: f64 = recorded
                    .iter()
                    .zip(&shown)
                    .filter_map(|(&share, &shown)| shown.then_some(share))
                    .sum();
                for (ix, share) in preferred.iter_mut().enumerate() {
                    if shown[ix] {
                        *share = recorded[ix] * prior_visible / recorded_visible;
                    }
                }
            }
            self.splits.insert(
                node.id(),
                Split {
                    children: child_ids,
                    preferred,
                    observed: sizes.to_vec(),
                },
            );
        });
    }

    pub(super) fn resize(
        &mut self,
        root: &PaneNode,
        viewport: Size<Pixels>,
        visible: &impl Fn(PanelId) -> bool,
    ) -> Vec<(NodeId, Vec<Pixels>)> {
        let mut changes = Vec::new();
        self.resize_node(root, viewport, visible, &mut changes);
        changes
    }

    fn resize_node(
        &mut self,
        node: &PaneNode,
        viewport: Size<Pixels>,
        visible: &impl Fn(PanelId) -> bool,
        changes: &mut Vec<(NodeId, Vec<Pixels>)>,
    ) {
        let PaneRef::Split { axis, children, .. } = node.kind() else {
            return;
        };
        let along = |size: Size<Pixels>| match axis {
            Axis::Horizontal => size.width,
            Axis::Vertical => size.height,
        };
        let minimums: Vec<_> = children
            .iter()
            .map(|child| f64::from(along(minimum_size(child, visible)).as_f32()))
            .collect();
        if minimums.iter().all(|&minimum| minimum == 0.) {
            return;
        }
        let split = self.splits.get_mut(&node.id()).expect("captured split");
        let extent = f64::from(along(viewport).as_f32());
        let sizes: Vec<_> = allocate(extent, &split.preferred, &minimums)
            .into_iter()
            .map(|size| px(size as f32))
            .collect();
        if !split
            .observed
            .iter()
            .copied()
            .eq(sizes.iter().copied().map(Some))
        {
            // Record our write before DockArea emits LayoutChanged so it cannot
            // be mistaken for the user's latest divider position.
            split.observed = sizes.iter().copied().map(Some).collect();
            changes.push((node.id(), sizes.clone()));
        }
        for (child, extent) in children.iter().zip(sizes) {
            if extent > Pixels::ZERO {
                let child_viewport = match axis {
                    Axis::Horizontal => size(extent, viewport.height),
                    Axis::Vertical => size(viewport.width, extent),
                };
                self.resize_node(child, child_viewport, visible, changes);
            }
        }
    }

    pub(super) fn save(&self, node: &PaneNode, state: &mut PanelState) {
        let (PaneRef::Split { children, .. }, PanelInfo::Stack { sizes, .. }) =
            (node.kind(), &mut state.info)
        else {
            return;
        };
        let split = self.splits.get(&node.id()).expect("captured split");
        // The saved schema takes pixel sizes. Only their relative proportions
        // matter when restoring, so use a common scale across window sizes.
        *sizes = split
            .preferred
            .iter()
            .map(|share| px((1000. * share) as f32))
            .collect();
        for (child, state) in children.iter().zip(&mut state.children) {
            self.save(child, state);
        }
    }
}

fn weights(sizes: &[Option<Pixels>]) -> Vec<f64> {
    let mut weights: Vec<_> = sizes
        .iter()
        .map(|size| {
            f64::from(
                size.map(Pixels::as_f32)
                    .filter(|size| size.is_finite() && *size > 0.)
                    .unwrap_or(PANEL_MIN_SIZE.as_f32()),
            )
        })
        .collect();
    let total: f64 = weights.iter().sum();
    for weight in &mut weights {
        *weight /= total;
    }
    weights
}

fn is_visible(node: &PaneNode, visible: &impl Fn(PanelId) -> bool) -> bool {
    match node.kind() {
        PaneRef::Split { children, .. } => children.iter().any(|child| is_visible(child, visible)),
        PaneRef::Tabs { panels, .. } => panels.iter().copied().any(visible),
    }
}

fn minimum_size(node: &PaneNode, visible: &impl Fn(PanelId) -> bool) -> Size<Pixels> {
    match node.kind() {
        PaneRef::Split { axis, children, .. } => {
            children
                .iter()
                .fold(Size::<Pixels>::default(), |total, child| {
                    let child = minimum_size(child, visible);
                    match axis {
                        Axis::Horizontal => {
                            size(total.width + child.width, total.height.max(child.height))
                        }
                        Axis::Vertical => {
                            size(total.width.max(child.width), total.height + child.height)
                        }
                    }
                })
        }
        PaneRef::Tabs { .. } if is_visible(node, visible) => size(PANEL_MIN_SIZE, PANEL_MIN_SIZE),
        PaneRef::Tabs { .. } => Size::default(),
    }
}

fn allocate(extent: f64, weights: &[f64], minimums: &[f64]) -> Vec<f64> {
    let minimum_total: f64 = minimums.iter().sum();
    if extent <= minimum_total {
        return minimums
            .iter()
            .map(|min| min * extent / minimum_total)
            .collect();
    }
    let mut sizes = vec![0.; weights.len()];
    let mut pending: Vec<_> = minimums
        .iter()
        .enumerate()
        .filter_map(|(index, &min)| (min > 0.).then_some(index))
        .collect();
    let mut remaining = extent;
    while !pending.is_empty() {
        let total: f64 = pending.iter().map(|&index| weights[index]).sum();
        let share = |index| remaining * weights[index] / total;
        if let Some(position) = pending
            .iter()
            .position(|&index| share(index) < minimums[index])
        {
            let index = pending.remove(position);
            sizes[index] = minimums[index];
            remaining -= sizes[index];
        } else {
            for index in pending {
                sizes[index] = share(index);
            }
            break;
        }
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::DockProportions;
    use gpui_kit::{
        Axis, Pixels, Size,
        component::dock::{
            NodeId, PaneRef, PaneTree, PanelBuilder, PanelId, PanelInfo, PanelState, RootKind,
        },
        px, size,
    };

    struct TestPanels;

    impl PanelBuilder for TestPanels {
        fn build(&mut self, state: &PanelState, _: &PanelInfo) -> PanelId {
            PanelId::from_u64(state.panel_name.parse().unwrap())
        }
    }

    fn tabs(panel: u64) -> PanelState {
        PanelState {
            panel_name: "TabPanel".into(),
            info: PanelInfo::tabs(0),
            children: vec![PanelState::new(panel.to_string())],
        }
    }

    fn stack(axis: Axis, extents: Vec<f32>, children: Vec<PanelState>) -> PanelState {
        PanelState {
            panel_name: "StackPanel".into(),
            info: PanelInfo::stack(extents.into_iter().map(px).collect(), axis),
            children,
        }
    }

    fn resize(
        proportions: &mut DockProportions,
        tree: &mut PaneTree,
        viewport: Size<Pixels>,
        visible: &impl Fn(PanelId) -> bool,
    ) {
        proportions.capture(tree.root(), visible);
        for (node, sizes) in proportions.resize(tree.root(), viewport, visible) {
            tree.set_sizes(node, sizes.into_iter().map(Some).collect());
        }
    }

    fn assert_extents(tree: &PaneTree, split: NodeId, expected: &[f32]) {
        let node = tree.find_node(split).unwrap();
        let PaneRef::Split { sizes, .. } = node.kind() else {
            panic!("expected split");
        };
        assert_eq!(sizes.len(), expected.len());
        for (size, expected) in sizes.iter().zip(expected) {
            assert!(
                (size.unwrap().as_f32() - expected).abs() < 0.01,
                "{sizes:?}"
            );
        }
    }

    #[test]
    fn temporary_minimums_never_become_the_saved_proportions() {
        let mut state = stack(
            Axis::Horizontal,
            vec![208., 812., 220.],
            vec![tabs(1), tabs(2), tabs(3)],
        );
        let mut tree = PaneTree::from_state(&state, RootKind::Split, &mut TestPanels);
        let mut proportions = DockProportions::default();
        let root = tree.root().id();
        let visible = |_| true;
        resize(
            &mut proportions,
            &mut tree,
            size(px(560.), px(400.)),
            &visible,
        );
        assert_extents(&tree, root, &[100., 360., 100.]);

        proportions.capture(tree.root(), &visible);
        proportions.save(tree.root(), &mut state);
        let mut restored = PaneTree::from_state(&state, RootKind::Split, &mut TestPanels);
        let restored_root = restored.root().id();
        for (proportions, tree, node) in [
            (&mut proportions, &mut tree, root),
            (
                &mut DockProportions::default(),
                &mut restored,
                restored_root,
            ),
        ] {
            resize(proportions, tree, size(px(1240.), px(800.)), &visible);
            assert_extents(tree, node, &[208., 812., 220.]);
        }
    }

    #[test]
    fn a_nested_split_reflows_on_its_own_axis_and_hidden_panes_recover() {
        let state = stack(
            Axis::Horizontal,
            vec![400., 200.],
            vec![
                tabs(1),
                stack(Axis::Vertical, vec![100., 300.], vec![tabs(2), tabs(3)]),
            ],
        );
        let mut tree = PaneTree::from_state(&state, RootKind::Split, &mut TestPanels);
        let mut proportions = DockProportions::default();
        let root = tree.root().id();
        let PaneRef::Split { children, .. } = tree.root().kind() else {
            panic!("expected split")
        };
        let nested = children[1].id();
        let viewport = size(px(900.), px(800.));
        resize(&mut proportions, &mut tree, viewport, &|id| {
            id.as_u64() != 2
        });
        assert_extents(&tree, root, &[600., 300.]);
        assert_extents(&tree, nested, &[0., 800.]);
        resize(&mut proportions, &mut tree, viewport, &|_| true);
        assert_extents(&tree, nested, &[200., 600.]);

        tree.set_sizes(nested, vec![Some(px(300.)), Some(px(500.))]);
        resize(
            &mut proportions,
            &mut tree,
            size(px(900.), px(400.)),
            &|_| true,
        );
        assert_extents(&tree, nested, &[150., 250.]);
    }
}
