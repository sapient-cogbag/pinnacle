// SPDX-License-Identifier: GPL-3.0-or-later

pub mod transaction;
pub mod tree;

use std::collections::{hash_map::Entry, HashMap};

use indexmap::IndexSet;
use smithay::{
    desktop::{layer_map_for_output, WindowSurface},
    output::Output,
    utils::{Logical, Rectangle, Serial},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;
use tree::{LayoutNode, LayoutTree};

use crate::{
    output::OutputName,
    state::{Pinnacle, State, WithState},
    tag::TagId,
    window::{window_state::LayoutModeKind, WindowElement},
};

use self::transaction::LayoutTransaction;

impl Pinnacle {
    // FIXME: make layout calls use f64 loc
    fn update_windows_with_geometries(
        &mut self,
        output: &Output,
        geometries: Vec<Rectangle<i32, Logical>>,
    ) -> Vec<(WindowElement, Serial)> {
        let (windows_on_foc_tags, to_unmap) = output.with_state(|state| {
            let focused_tags = state.focused_tags().cloned().collect::<IndexSet<_>>();
            self.windows
                .iter()
                .filter(|win| win.output(self).as_ref() == Some(output))
                .cloned()
                .partition::<Vec<_>, _>(|win| {
                    win.with_state(|state| state.tags.intersection(&focused_tags).next().is_some())
                })
        });

        for win in to_unmap {
            if win.with_state(|state| state.layout_mode.is_floating()) {
                if let Some(loc) = self.space.element_location(&win) {
                    win.with_state_mut(|state| state.floating_loc = Some(loc.to_f64()));
                }
            }
            let to_schedule = self.space.outputs_for_element(&win);
            self.space.unmap_elem(&win);
            self.loop_handle.insert_idle(move |state| {
                for output in to_schedule {
                    state.schedule_render(&output);
                }
            });
        }

        let tiled_windows = windows_on_foc_tags
            .iter()
            .filter(|win| !win.is_x11_override_redirect())
            .filter(|win| win.with_state(|state| state.layout_mode.is_tiled()))
            .cloned();

        let output_geo = self.space.output_geometry(output).expect("no output geo");

        let non_exclusive_geo = {
            let map = layer_map_for_output(output);
            map.non_exclusive_zone()
        };

        let mut zipped = tiled_windows.zip(geometries.into_iter().map(|mut geo| {
            geo.loc += output_geo.loc + non_exclusive_geo.loc;
            geo
        }));

        for (win, geo) in zipped.by_ref() {
            win.set_tiled_states();
            match win.underlying_surface() {
                WindowSurface::Wayland(toplevel) => {
                    toplevel.with_pending_state(|state| {
                        state.size = Some(geo.size);
                    });
                }
                WindowSurface::X11(surface) => {
                    let _ = surface.configure(geo);
                }
            }
            self.space.map_element(win, geo.loc, false);
        }

        let (remaining_wins, _remaining_geos) = zipped.unzip::<_, _, Vec<_>, Vec<_>>();

        for win in remaining_wins {
            let to_schedule = self.space.outputs_for_element(&win);
            self.space.unmap_elem(&win);
            self.loop_handle.insert_idle(move |state| {
                for output in to_schedule {
                    state.schedule_render(&output);
                }
            });
        }

        let mut pending_wins = Vec::<(WindowElement, Serial)>::new();

        for win in windows_on_foc_tags.iter() {
            if let WindowSurface::Wayland(toplevel) = win.underlying_surface() {
                if let Some(serial) = toplevel.send_pending_configure() {
                    pending_wins.push((win.clone(), serial));
                }
            }

            match win.with_state(|state| state.layout_mode.current()) {
                LayoutModeKind::Tiled => (),
                LayoutModeKind::Floating => {
                    let floating_loc = win.with_state(|state| state.floating_loc);
                    if let Some(loc) = floating_loc {
                        self.space
                            .map_element(win.clone(), loc.to_i32_round(), false);
                    }
                }
                LayoutModeKind::Maximized => {
                    let loc = output_geo.loc + non_exclusive_geo.loc;
                    self.space.map_element(win.clone(), loc, false);
                }
                LayoutModeKind::Fullscreen => {
                    self.space.map_element(win.clone(), output_geo.loc, false);
                }
            }
        }

        self.fixup_z_layering();

        pending_wins
    }

    pub fn swap_window_positions(&mut self, win1: &WindowElement, win2: &WindowElement) {
        let win1_index = self.windows.iter().position(|win| win == win1);
        let win2_index = self.windows.iter().position(|win| win == win2);

        if let (Some(first), Some(second)) = (win1_index, win2_index) {
            self.windows.swap(first, second);
        }
    }
}

/// A monotonically increasing identifier for layout requests.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct LayoutRequestId(u32);

impl LayoutRequestId {
    pub fn to_inner(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Default)]
pub struct LayoutState {
    pub layout_request_sender: Option<UnboundedSender<LayoutInfo>>,
    pub pending_swap: bool,
    // TODO: make these outputs weak or something
    pending_requests: HashMap<Output, LayoutRequestId>,
    fulfilled_requests: HashMap<Output, LayoutRequestId>,
    current_id: LayoutRequestId,
    pub layout_trees: HashMap<u32, LayoutTree>,
}

impl LayoutState {
    fn next_id(&mut self) -> LayoutRequestId {
        self.current_id.0 += 1;
        self.current_id
    }
}

#[derive(Debug)]
pub struct LayoutInfo {
    pub request_id: LayoutRequestId,
    pub output_name: OutputName,
    pub window_count: u32,
    pub tag_ids: Vec<TagId>,
}

impl Pinnacle {
    pub fn request_layout(&mut self, output: &Output) {
        if self
            .outputs
            .get(output)
            .is_some_and(|global| global.is_none())
        {
            return;
        }

        let id = self.layout_state.next_id();
        let Some(sender) = self.layout_state.layout_request_sender.as_ref() else {
            warn!("Layout requested but no client has connected to the layout service");
            return;
        };

        let windows_on_foc_tags = output.with_state(|state| {
            let focused_tags = state.focused_tags().cloned().collect::<IndexSet<_>>();
            self.windows
                .iter()
                .filter(|win| !win.is_x11_override_redirect())
                .filter(|win| {
                    win.with_state(|state| state.tags.intersection(&focused_tags).next().is_some())
                })
                .cloned()
                .collect::<Vec<_>>()
        });

        let window_count = windows_on_foc_tags
            .iter()
            .filter(|win| win.with_state(|state| state.layout_mode.is_tiled()))
            .count();

        let tag_ids = output.with_state(|state| state.focused_tags().map(|tag| tag.id()).collect());

        self.layout_state
            .pending_requests
            .insert(output.clone(), id);

        let _ = sender.send(LayoutInfo {
            request_id: id,
            output_name: OutputName(output.name()),
            window_count: window_count as u32,
            tag_ids,
        });
    }
}

impl State {
    pub fn apply_layout_tree(
        &mut self,
        tree_id: u32,
        root_node: LayoutNode,
        request_id: u32,
        output_name: String,
    ) -> anyhow::Result<()> {
        let Some(output) = OutputName(output_name).output(&self.pinnacle) else {
            anyhow::bail!("Output was invalid");
        };

        let tree_entry = self.pinnacle.layout_state.layout_trees.entry(tree_id);
        let tree = match tree_entry {
            Entry::Occupied(occupied_entry) => {
                let tree = occupied_entry.into_mut();
                tree.diff(root_node);
                tree
            }
            Entry::Vacant(vacant_entry) => vacant_entry.insert(LayoutTree::new(root_node)),
        };

        let request_id = LayoutRequestId(request_id);

        let Some(current_pending) = self
            .pinnacle
            .layout_state
            .pending_requests
            .get(&output)
            .copied()
        else {
            anyhow::bail!("attempted to layout without request");
        };

        if current_pending > request_id {
            anyhow::bail!("Attempted to layout but a new request came in");
        }
        if current_pending < request_id {
            anyhow::bail!("Attempted to layout but request is newer");
        }

        let (output_width, output_height) = {
            let map = layer_map_for_output(&output);
            let zone = map.non_exclusive_zone();
            (zone.size.w, zone.size.h)
        };

        let geometries = tree.compute_geos(output_width as u32, output_height as u32);

        self.pinnacle.layout_state.pending_requests.remove(&output);
        self.pinnacle
            .layout_state
            .fulfilled_requests
            .insert(output.clone(), current_pending);

        let pending_windows = self
            .pinnacle
            .update_windows_with_geometries(&output, geometries);

        output.with_state_mut(|state| {
            if let Some(ts) = state.layout_transaction.as_mut() {
                ts.update_pending(pending_windows);
            } else {
                state.layout_transaction = Some(LayoutTransaction::new(
                    self.pinnacle.loop_handle.clone(),
                    std::mem::take(&mut state.snapshots.fullscreen_and_above),
                    std::mem::take(&mut state.snapshots.under_fullscreen),
                    pending_windows,
                ));
            }
        });

        self.schedule_render(&output);

        self.pinnacle.layout_state.pending_swap = false;

        Ok(())
    }
}
