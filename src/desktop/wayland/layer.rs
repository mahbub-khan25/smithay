use crate::{
    backend::renderer::utils::RendererSurfaceStateUserData,
    desktop::{PopupManager, utils::*},
    output::{Output, WeakOutput},
    utils::{IsAlive, Logical, Point, Rectangle, Size, user_data::UserDataMap},
    wayland::{
        compositor::{SurfaceData, TraversalAction, with_states, with_surface_tree_downward},
        dmabuf::DmabufFeedback,
        seat::WaylandFocus,
        shell::wlr_layer::{
            Anchor, ExclusiveZone, KeyboardInteractivity, Layer as WlrLayer, LayerSurface as WlrLayerSurface,
            LayerSurfaceCachedState, LayerSurfaceData, Margins,
        },
    },
};
use indexmap::IndexSet;
use tracing::{debug_span, trace};
use wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use wayland_server::protocol::wl_surface::{self, WlSurface};

use std::{
    borrow::Cow,
    hash::{Hash, Hasher},
    num::Saturating,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use crate::desktop::WindowSurfaceType;

crate::utils::ids::id_gen!(layer_id);

/// Map of [`LayerSurface`]s on an [`Output`]
#[derive(Debug)]
pub struct LayerMap {
    layers: IndexSet<LayerSurface>,
    output: WeakOutput,
    zone: Rectangle<i32, Logical>,
}

/// Retrieve a [`LayerMap`] for a given [`Output`].
///
/// If none existed before a new empty [`LayerMap`] is attached
/// to the output and returned on subsequent calls.
///
/// Note: This function internally uses a [`Mutex`] per
/// [`Output`] as exposed by its return type. Therefore
/// trying to hold on to multiple references of a [`LayerMap`]
/// of the same output using this function *will* result in a deadlock.
pub fn layer_map_for_output(o: &Output) -> MutexGuard<'_, LayerMap> {
    let userdata = o.user_data();
    userdata.insert_if_missing_threadsafe(|| {
        Mutex::new(LayerMap {
            layers: IndexSet::new(),
            output: o.downgrade(),
            zone: Rectangle::from_size(output_logical_size(o)),
        })
    });
    userdata.get::<Mutex<LayerMap>>().unwrap().lock().unwrap()
}

/// The logical size of `o` that its layer surfaces are arranged in: its mode over its scale,
/// rounded up, then transformed. The same size as
/// [`Space::output_geometry`](crate::desktop::Space::output_geometry), so a bar anchored to the
/// right or bottom edge ends exactly at the output's edge in the space, and the zone it leaves
/// meets the windows with no overlap. (0, 0) without a mode.
fn output_logical_size(o: &Output) -> Size<i32, Logical> {
    o.current_mode()
        .map(|mode| {
            let logical_size = mode
                .size
                .to_f64()
                .to_logical(o.current_scale().fractional_scale())
                .to_i32_ceil();
            o.current_transform().transform_size(logical_size)
        })
        .unwrap_or_else(|| (0, 0).into())
}

#[derive(Debug, thiserror::Error)]
pub enum LayerError {
    #[error("Layer is already mapped to a different map")]
    AlreadyMapped,
}

impl LayerMap {
    /// Map a [`LayerSurface`] to this [`LayerMap`].
    pub fn map_layer(&mut self, layer: &LayerSurface) -> Result<(), LayerError> {
        if !self.layers.contains(layer) {
            if layer
                .0
                .userdata
                .get::<LayerUserdata>()
                .map(|s| s.lock().unwrap().location.is_some())
                .unwrap_or(false)
            {
                return Err(LayerError::AlreadyMapped);
            }

            self.layers.insert(layer.clone());
            self.arrange();
        }
        Ok(())
    }

    /// Remove a [`LayerSurface`] from this [`LayerMap`].
    pub fn unmap_layer(&mut self, layer: &LayerSurface) {
        if self.layers.shift_remove(layer) {
            let _ = layer
                .user_data()
                .get::<LayerUserdata>()
                .unwrap()
                .lock()
                .unwrap()
                .location
                .take();
            self.arrange();
        }
        if let (Some(output), surface) = (self.output(), layer.wl_surface()) {
            with_surface_tree_downward(
                surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |wl_surface, _, _| {
                    output.leave(wl_surface);
                },
                |_, _, _| true,
            );
            for (popup, _) in PopupManager::popups_for_surface(surface) {
                let surface = popup.wl_surface();
                with_surface_tree_downward(
                    surface,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |wl_surface, _, _| {
                        output.leave(wl_surface);
                    },
                    |_, _, _| true,
                )
            }
        }
    }

    /// Return the area of this output, that is not exclusive to any [`LayerSurface`]s.
    pub fn non_exclusive_zone(&self) -> Rectangle<i32, Logical> {
        self.zone
    }

    /// Returns the geometry of a given mapped [`LayerSurface`].
    ///
    /// If the surface was not previously mapped onto this layer map,
    /// this function return `None`.
    pub fn layer_geometry(&self, layer: &LayerSurface) -> Option<Rectangle<i32, Logical>> {
        if !self.layers.contains(layer) {
            return None;
        }
        let mut geo = layer.geometry();
        let state = layer_state(layer);
        geo.loc += state.location.unwrap_or_default();
        Some(geo)
    }

    /// Returns a [`LayerSurface`] under a given point and on a given layer, if any.
    pub fn layer_under<P: Into<Point<f64, Logical>>>(
        &self,
        layer: WlrLayer,
        point: P,
    ) -> Option<&LayerSurface> {
        let point = point.into();
        self.layers_on(layer).rev().find(|l| {
            let bbox_with_popups = {
                let mut bbox = l.bbox_with_popups();
                let state = layer_state(l);
                bbox.loc += state.location.unwrap_or_default();
                bbox
            };
            bbox_with_popups.to_f64().contains(point)
        })
    }

    /// Iterator over all [`LayerSurface`]s currently mapped.
    pub fn layers(&self) -> impl DoubleEndedIterator<Item = &LayerSurface> {
        self.layers.iter()
    }

    /// Iterator over all [`LayerSurface`]s currently mapped on a given layer.
    pub fn layers_on(&self, layer: WlrLayer) -> impl DoubleEndedIterator<Item = &LayerSurface> {
        self.layers.iter().filter(move |l| l.layer() == layer)
    }

    /// Returns the [`LayerSurface`] matching a given [`WlSurface`], if any.
    ///
    /// `surface_type` can be used to limit the types of surfaces queried for equality.
    pub fn layer_for_surface(
        &self,
        surface: &WlSurface,
        surface_type: WindowSurfaceType,
    ) -> Option<&LayerSurface> {
        use std::sync::atomic::{AtomicBool, Ordering};

        self.layers.iter().find(|layer| {
            if surface_type.contains(WindowSurfaceType::POPUP) {
                for (popup, _) in PopupManager::popups_for_surface(layer.wl_surface()) {
                    let toplevel = popup.wl_surface();
                    let found = AtomicBool::new(false);
                    with_surface_tree_downward(
                        toplevel,
                        surface,
                        |_, _, search| {
                            if surface_type.contains(WindowSurfaceType::SUBSURFACE) {
                                TraversalAction::DoChildren(search)
                            } else {
                                TraversalAction::SkipChildren
                            }
                        },
                        |s, _, search| {
                            found.fetch_or(s == *search, Ordering::SeqCst);
                        },
                        |_, _, _| !found.load(Ordering::SeqCst),
                    );
                    if found.load(Ordering::SeqCst) {
                        return true;
                    }
                }
            }

            if surface_type.contains(WindowSurfaceType::TOPLEVEL) {
                let toplevel = layer.wl_surface();
                let found = AtomicBool::new(false);
                with_surface_tree_downward(
                    toplevel,
                    surface,
                    |_, _, search| {
                        if surface_type.contains(WindowSurfaceType::SUBSURFACE) {
                            TraversalAction::DoChildren(search)
                        } else {
                            TraversalAction::SkipChildren
                        }
                    },
                    |s, _, search| {
                        found.fetch_or(s == *search, Ordering::SeqCst);
                    },
                    |_, _, _| !found.load(Ordering::SeqCst),
                );
                return found.load(Ordering::SeqCst);
            }

            false
        })
    }

    /// Force re-arranging the layer surfaces, e.g. when the output size changes.
    ///
    /// Note: Mapping or unmapping a layer surface will automatically cause a re-arrangement.
    ///
    /// Return whenever any position or size changes of existing surfaces were necessary.
    pub fn arrange(&mut self) -> bool {
        let mut changed = false;
        if let Some(output) = self.output() {
            let span = debug_span!("layer_map", output = output.name());
            let _guard = span.enter();

            let output_rect = Rectangle::from_size(output_logical_size(&output));
            let zone = output_rect;
            let mut zone: Rectangle<_, Logical> = Rectangle::new(
                Point::new(Saturating(zone.loc.x), Saturating(zone.loc.y)),
                Size::new(Saturating(zone.size.w), Saturating(zone.size.h)),
            );
            trace!("Arranging layers into {:?}", output_rect.size);

            let exclusive_surfaces = self
                .layers
                .iter()
                .filter(|l| matches!(l.effective_exclusive_zone(), ExclusiveZone::Exclusive(_)));
            let non_exclusive_surfaces = self
                .layers
                .iter()
                .filter(|l| !matches!(l.effective_exclusive_zone(), ExclusiveZone::Exclusive(_)));
            for layer in exclusive_surfaces.chain(non_exclusive_surfaces) {
                let surface = layer.wl_surface();

                with_surface_tree_downward(
                    surface,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |wl_surface, _, _| {
                        output.enter(wl_surface);
                    },
                    |_, _, _| true,
                );
                for (popup, _) in PopupManager::popups_for_surface(surface) {
                    let surface = popup.wl_surface();
                    with_surface_tree_downward(
                        surface,
                        (),
                        |_, _, _| TraversalAction::DoChildren(()),
                        |wl_surface, _, _| {
                            output.enter(wl_surface);
                        },
                        |_, _, _| true,
                    )
                }

                let data = layer.cached_state();
                let (location, size) = place_layer(output_rect, &mut zone, &data);
                trace!("Setting layer to pos {:?} and size {:?}", location, size);
                let size_changed = layer.0.surface.with_pending_state(|state| {
                    state.size.replace(size).map(|old| old != size).unwrap_or(true)
                });
                changed = changed || size_changed;
                let initial_configure_sent = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .map(|data| data.lock().unwrap().initial_configure_sent)
                })
                .unwrap_or_default();

                // arrange should never automatically send an configure
                // event if the surface has not been configured already.
                // The spec mandates that the initial configure has to be
                // send in response of the initial commit of the surface.
                // That also guarantees that the client is able set a size
                // before committing the surface. By not respecting that
                // we would send a wrong size to the client and also violate
                // the spec by sending a configure event before a prior commit.
                if size_changed && initial_configure_sent {
                    layer.0.surface.send_pending_configure();
                }

                {
                    let mut layer_state = layer_state(layer);
                    if layer_state.location != Some(location) {
                        layer_state.location = Some(location);
                        changed = true;
                    }
                }
            }

            let zone = Rectangle::new(
                Point::new(zone.loc.x.0, zone.loc.y.0),
                Size::new(zone.size.w.0.max(0), zone.size.h.0.max(0)),
            );
            trace!("Remaining zone {:?}", zone);
            self.zone = zone;
        }

        changed
    }

    fn output(&self) -> Option<Output> {
        self.output.upgrade()
    }

    /// Cleanup some internally used resources.
    ///
    /// This function needs to be called periodically (though not necessarily frequently)
    /// to be able cleanup internally used resources.
    pub fn cleanup(&mut self) {
        if self.layers.iter().any(|l| !l.alive()) {
            self.layers.retain(|layer| layer.alive());
            self.arrange();
        }
    }

    /// Returns layers count
    #[allow(clippy::len_without_is_empty)] //we don't need is_empty on that struct for now, mark as allow
    pub fn len(&self) -> usize {
        self.layers.len()
    }
}

/// Places one layer surface on an output of `output_rect`: its location and size, taking its
/// exclusive zone (if any) off `zone`, the area left for the surfaces arranged after it.
fn place_layer(
    output_rect: Rectangle<i32, Logical>,
    zone: &mut Rectangle<Saturating<i32>, Logical>,
    data: &LayerSurfaceCachedState,
) -> (Point<i32, Logical>, Size<i32, Logical>) {
    // The protocol puts no bounds on margins and exclusive zones, so a client could send
    // any `i32`. A margin is kept within the output's size on its axis: beyond that, the
    // surface is placed as if it were exactly that far, which is wholly off the output or
    // slid fully past its edge either way. An exclusive zone reserves at most what is left
    // of `zone` on its axis and never less than nothing, so no client can grow `zone` past
    // the output or move it off the output for the surfaces arranged after it.
    let (w, h) = (output_rect.size.w.max(0), output_rect.size.h.max(0));
    let margin = Margins {
        top: data.margin.top.clamp(-h, h),
        right: data.margin.right.clamp(-w, w),
        bottom: data.margin.bottom.clamp(-h, h),
        left: data.margin.left.clamp(-w, w),
    };

    let mut source = match data.exclusive_zone {
        ExclusiveZone::Exclusive(_) | ExclusiveZone::Neutral => *zone,
        ExclusiveZone::DontCare => Rectangle::new(
            Point::new(Saturating(output_rect.loc.x), Saturating(output_rect.loc.y)),
            Size::new(Saturating(output_rect.size.w), Saturating(output_rect.size.h)),
        ),
    };

    // adjust the copy rect to account for the margins
    if data.anchor.contains(Anchor::LEFT) {
        source.size.w -= margin.left
    }
    if data.anchor.contains(Anchor::RIGHT) {
        source.size.w -= margin.right
    }
    if data.anchor.contains(Anchor::TOP) {
        source.size.h -= margin.top
    }
    if data.anchor.contains(Anchor::BOTTOM) {
        source.size.h -= margin.bottom
    }

    let mut size: Size<_, Logical> = Size::new(Saturating(data.size.w), Saturating(data.size.h));
    size.w = size.w.min(source.size.w);
    size.h = size.h.min(source.size.h);
    if size.w.0 == 0 {
        size.w.0 = source.size.w.0 / 2;
    }
    if size.h.0 == 0 {
        size.h.0 = source.size.h.0 / 2;
    }
    if data.anchor.anchored_horizontally() {
        size.w = source.size.w;
    }
    if data.anchor.anchored_vertically() {
        size.h = source.size.h;
    }

    let x = if data.anchor.contains(Anchor::LEFT) {
        source.loc.x + Saturating(margin.left)
    } else if data.anchor.contains(Anchor::RIGHT) {
        source.loc.x + (source.size.w - size.w)
    } else {
        source.loc.x + Saturating((source.size.w.0 / 2) - (size.w.0 / 2))
    };

    let y = if data.anchor.contains(Anchor::TOP) {
        source.loc.y + Saturating(margin.top)
    } else if data.anchor.contains(Anchor::BOTTOM) {
        source.loc.y + (source.size.h - size.h)
    } else {
        source.loc.y + Saturating((source.size.h.0 / 2) - (size.h.0 / 2))
    };

    let location: Point<i32, Logical> = (x.0, y.0).into();

    if let ExclusiveZone::Exclusive(amount) = data.exclusive_zone {
        let amount = i32::try_from(amount).unwrap_or(i32::MAX);
        // What the zone and its margin take off what is left of `zone` on that axis.
        let reserve = |margin: i32, room: Saturating<i32>| {
            Saturating(
                (Saturating(amount) + Saturating(margin))
                    .0
                    .clamp(0, room.0.max(0)),
            )
        };

        match effective_exclusive_edge(data) {
            Some(Anchor::TOP) => {
                let sum = reserve(margin.top, zone.size.h);
                zone.loc.y += sum;
                zone.size.h -= sum;
            }
            Some(Anchor::BOTTOM) => {
                zone.size.h -= reserve(margin.bottom, zone.size.h);
            }
            Some(Anchor::LEFT) => {
                let sum = reserve(margin.left, zone.size.w);
                zone.loc.x += sum;
                zone.size.w -= sum;
            }
            Some(Anchor::RIGHT) => {
                zone.size.w -= reserve(margin.right, zone.size.w);
            }
            // Exclusive edge is always exactly one edge
            Some(_) => unreachable!(),
            None => {}
        }
    }

    let size = Size::new(size.w.0.max(0), size.h.0.max(0));
    (location, size)
}

fn effective_exclusive_edge(data: &LayerSurfaceCachedState) -> Option<Anchor> {
    data.exclusive_edge
        .or_else(|| implied_exclusive_edge_for_anchor(data.anchor))
}

fn implied_exclusive_edge_for_anchor(anchor: Anchor) -> Option<Anchor> {
    match anchor.bits().count_ones() {
        0 | 2 | 4 => None,
        1 => Some(anchor),
        3 => Some(match anchor.complement() {
            Anchor::TOP => Anchor::BOTTOM,
            Anchor::BOTTOM => Anchor::TOP,
            Anchor::LEFT => Anchor::RIGHT,
            Anchor::RIGHT => Anchor::LEFT,
            _ => unreachable!(),
        }),
        _ => unreachable!(),
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LayerState {
    pub location: Option<Point<i32, Logical>>,
}

type LayerUserdata = Mutex<LayerState>;
pub fn layer_state(layer: &LayerSurface) -> MutexGuard<'_, LayerState> {
    let userdata = layer.user_data();
    userdata.insert_if_missing_threadsafe(LayerUserdata::default);
    userdata.get::<LayerUserdata>().unwrap().lock().unwrap()
}

/// A [`LayerSurface`] represents a single layer surface as given by the wlr-layer-shell protocol.
#[derive(Debug, Clone)]
pub struct LayerSurface(pub(crate) Arc<LayerSurfaceInner>);

impl PartialEq for LayerSurface {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for LayerSurface {}

impl Hash for LayerSurface {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

#[derive(Debug)]
pub(crate) struct LayerSurfaceInner {
    pub(crate) id: usize,
    surface: WlrLayerSurface,
    namespace: String,
    userdata: UserDataMap,
}

impl Drop for LayerSurfaceInner {
    #[inline]
    fn drop(&mut self) {
        layer_id::remove(self.id);
    }
}

impl IsAlive for LayerSurface {
    #[inline]
    fn alive(&self) -> bool {
        self.0.surface.alive()
    }
}

impl LayerSurface {
    /// Create a new [`LayerSurface`] from a given [`WlrLayerSurface`] and its namespace.
    pub fn new(surface: WlrLayerSurface, namespace: String) -> LayerSurface {
        LayerSurface(Arc::new(LayerSurfaceInner {
            id: layer_id::next(),
            surface,
            namespace,
            userdata: UserDataMap::new(),
        }))
    }

    /// Returns the underlying [`WlrLayerSurface`]
    pub fn layer_surface(&self) -> &WlrLayerSurface {
        &self.0.surface
    }

    /// Returns the underlying [`WlSurface`]
    #[inline]
    pub fn wl_surface(&self) -> &WlSurface {
        self.0.surface.wl_surface()
    }

    /// Returns a clone of the cached protocol state
    pub fn cached_state(&self) -> LayerSurfaceCachedState {
        self.0.surface.with_cached_state(Clone::clone)
    }

    /// Returns true, if the surface has indicated, that it is able to process keyboard events.
    pub fn can_receive_keyboard_focus(&self) -> bool {
        self.0
            .surface
            .with_cached_state(|state| match state.keyboard_interactivity {
                KeyboardInteractivity::Exclusive | KeyboardInteractivity::OnDemand => true,
                KeyboardInteractivity::None => false,
            })
    }

    /// Returns the layer this surface resides on, if any yet.
    pub fn layer(&self) -> WlrLayer {
        self.0.surface.with_cached_state(|state| state.layer)
    }

    /// Returns the namespace of this surface
    pub fn namespace(&self) -> &str {
        &self.0.namespace
    }

    /// Returns the bounding box over this layer surface excluding its subsurfaces.
    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        with_states(self.0.surface.wl_surface(), |states| {
            let data = states.data_map.get::<RendererSurfaceStateUserData>();
            data.and_then(|d| d.lock().unwrap().view()).map(|view| Rectangle {
                loc: view.offset,
                size: view.dst,
            })
        })
        .unwrap_or_default()
    }

    /// Returns the bounding box over this layer surface and its subsurfaces.
    pub fn bbox(&self) -> Rectangle<i32, Logical> {
        bbox_from_surface_tree(self.0.surface.wl_surface(), (0, 0))
    }

    /// Returns the bounding box over this layer surface, it subsurfaces as well as any popups.
    ///
    /// Note: You need to use a [`PopupManager`] to track popups, otherwise the bounding box
    /// will not include the popups.
    pub fn bbox_with_popups(&self) -> Rectangle<i32, Logical> {
        let mut bounding_box = self.bbox();
        let surface = self.0.surface.wl_surface();
        for (popup, location) in PopupManager::popups_for_surface(surface) {
            let offset = location - popup.geometry().loc;
            bounding_box = bounding_box.merge(bbox_from_surface_tree(popup.wl_surface(), offset));
        }

        bounding_box
    }

    /// Finds the topmost surface under this point if any and returns it together with the location of this
    /// surface.
    ///
    /// - `point` needs to be relative to (0,0) of the layer surface.
    pub fn surface_under<P: Into<Point<f64, Logical>>>(
        &self,
        point: P,
        surface_type: WindowSurfaceType,
    ) -> Option<(WlSurface, Point<i32, Logical>)> {
        let point = point.into();
        let surface = self.wl_surface();
        if surface_type.contains(WindowSurfaceType::POPUP) {
            for (popup, location) in PopupManager::popups_for_surface(surface) {
                let surface = popup.wl_surface();
                let offset = location - popup.geometry().loc;
                if let Some(result) = under_from_surface_tree(surface, point, offset, surface_type) {
                    return Some(result);
                }
            }
        }

        if surface_type.contains(WindowSurfaceType::TOPLEVEL) {
            return under_from_surface_tree(surface, point, (0, 0), surface_type);
        }

        None
    }

    /// Sends the frame callback to all the subsurfaces in this layer that requested it
    ///
    /// See [`send_frames_surface_tree`] for more information
    pub fn send_frame<T, F>(
        &self,
        output: &Output,
        time: T,
        throttle: Option<Duration>,
        primary_scan_out_output: F,
    ) where
        T: Into<Duration>,
        F: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
    {
        let time = time.into();
        let surface = self.0.surface.wl_surface();

        send_frames_surface_tree(surface, output, time, throttle, primary_scan_out_output);
        for (popup, _) in PopupManager::popups_for_surface(surface) {
            let surface = popup.wl_surface();
            send_frames_surface_tree(surface, output, time, throttle, primary_scan_out_output);
        }
    }

    /// Sends the dmabuf feedback to all the subsurfaces in this window that requested it
    ///
    /// See [`send_dmabuf_feedback_surface_tree`] for more information
    pub fn send_dmabuf_feedback<'a, P, F>(
        &self,
        output: &Output,
        primary_scan_out_output: P,
        select_dmabuf_feedback: F,
    ) where
        P: FnMut(&wl_surface::WlSurface, &SurfaceData) -> Option<Output> + Copy,
        F: Fn(&wl_surface::WlSurface, &SurfaceData) -> &'a DmabufFeedback + Copy,
    {
        let surface = self.0.surface.wl_surface();
        send_dmabuf_feedback_surface_tree(surface, output, primary_scan_out_output, select_dmabuf_feedback);
        for (popup, _) in PopupManager::popups_for_surface(surface) {
            let surface = popup.wl_surface();
            send_dmabuf_feedback_surface_tree(
                surface,
                output,
                primary_scan_out_output,
                select_dmabuf_feedback,
            );
        }
    }

    /// Takes the [`PresentationFeedbackCallback`](crate::wayland::presentation::PresentationFeedbackCallback)s from all subsurfaces in this layer
    ///
    /// see [`take_presentation_feedback_surface_tree`] for more information
    pub fn take_presentation_feedback<F1, F2>(
        &self,
        output_feedback: &mut OutputPresentationFeedback,
        primary_scan_out_output: F1,
        presentation_feedback_flags: F2,
    ) where
        F1: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
        F2: FnMut(&WlSurface, &SurfaceData) -> wp_presentation_feedback::Kind + Copy,
    {
        let surface = self.0.surface.wl_surface();
        take_presentation_feedback_surface_tree(
            surface,
            output_feedback,
            primary_scan_out_output,
            presentation_feedback_flags,
        );
        for (popup, _) in PopupManager::popups_for_surface(surface) {
            let surface = popup.wl_surface();
            take_presentation_feedback_surface_tree(
                surface,
                output_feedback,
                primary_scan_out_output,
                presentation_feedback_flags,
            );
        }
    }

    /// Run a closure on all surfaces in this layer (including it's popups, if [`PopupManager`] is used)
    pub fn with_surfaces<F>(&self, mut processor: F)
    where
        F: FnMut(&WlSurface, &SurfaceData),
    {
        let surface = self.0.surface.wl_surface();

        with_surfaces_surface_tree(surface, &mut processor);
        for (popup, _) in PopupManager::popups_for_surface(surface) {
            let surface = popup.wl_surface();
            with_surfaces_surface_tree(surface, &mut processor);
        }
    }

    /// Returns a [`UserDataMap`] to allow associating arbitrary data with this surface.
    pub fn user_data(&self) -> &UserDataMap {
        &self.0.userdata
    }

    /// Returns the exclusive zone set for the layer; except if an exclusive zone is set
    /// but the exclusive edge is not set and cannot be inferred, then return `Neutral`.
    fn effective_exclusive_zone(&self) -> ExclusiveZone {
        with_states(self.wl_surface(), |states| {
            let mut cached_state = states.cached_state.get::<LayerSurfaceCachedState>();
            let data = cached_state.current();
            if matches!(data.exclusive_zone, ExclusiveZone::Exclusive(_))
                && effective_exclusive_edge(data).is_none()
            {
                ExclusiveZone::Neutral
            } else {
                data.exclusive_zone
            }
        })
    }
}

impl WaylandFocus for LayerSurface {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, wl_surface::WlSurface>> {
        Some(Cow::Borrowed(self.0.surface.wl_surface()))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn output() -> Rectangle<i32, Logical> {
        Rectangle::from_size((1920, 1080).into())
    }

    fn zone_of(rect: Rectangle<i32, Logical>) -> Rectangle<Saturating<i32>, Logical> {
        Rectangle::new(
            Point::new(Saturating(rect.loc.x), Saturating(rect.loc.y)),
            Size::new(Saturating(rect.size.w), Saturating(rect.size.h)),
        )
    }

    fn layer(
        anchor: Anchor,
        size: (i32, i32),
        zone: ExclusiveZone,
        margin: Margins,
    ) -> LayerSurfaceCachedState {
        LayerSurfaceCachedState {
            size: size.into(),
            anchor,
            exclusive_zone: zone,
            margin,
            ..Default::default()
        }
    }

    fn inside(outer: Rectangle<i32, Logical>, zone: &Rectangle<Saturating<i32>, Logical>) -> bool {
        let zone = Rectangle::new(
            Point::new(zone.loc.x.0, zone.loc.y.0),
            Size::new(zone.size.w.0, zone.size.h.0),
        );
        zone.size.w >= 0 && zone.size.h >= 0 && outer.contains_rect(zone)
    }

    #[test]
    fn the_layer_map_is_as_big_as_the_space_at_fractional_scales() {
        use crate::desktop::{Space, Window};
        use crate::output::{Mode, PhysicalProperties, Scale, Subpixel};
        for ((w, h), scale) in [((1920, 1200), 1.75), ((1920, 1080), 1.75), ((2560, 1440), 1.25)] {
            let output = Output::new(
                "test".into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "test".into(),
                    serial_number: "test".into(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (w, h).into(),
                    refresh: 60_000,
                }),
                Some(crate::utils::Transform::Normal),
                Some(Scale::Fractional(scale)),
                Some((0, 0).into()),
            );
            let mut space: Space<Window> = Space::default();
            space.map_output(&output, (0, 0));
            let geometry = space.output_geometry(&output).unwrap();
            let mut map = layer_map_for_output(&output);
            assert_eq!(map.non_exclusive_zone().size, geometry.size, "{w}x{h} at {scale}");
            map.arrange();
            assert_eq!(map.non_exclusive_zone().size, geometry.size, "{w}x{h} at {scale}");
        }
    }

    #[test]
    fn absurd_margins_and_zones_do_not_move_later_layers_off_the_output() {
        let output = output();
        let wild = Margins {
            top: i32::MAX,
            right: i32::MAX,
            bottom: i32::MIN,
            left: i32::MIN,
        };
        for first in [
            layer(Anchor::LEFT, (300, 200), ExclusiveZone::Exclusive(50), wild),
            layer(Anchor::LEFT, (300, 200), ExclusiveZone::Exclusive(u32::MAX), wild),
            layer(Anchor::BOTTOM, (300, 200), ExclusiveZone::Exclusive(50), wild),
            layer(
                Anchor::TOP,
                (300, 200),
                ExclusiveZone::Exclusive(50),
                Margins {
                    top: i32::MIN,
                    ..Default::default()
                },
            ),
            layer(
                Anchor::RIGHT,
                (300, 200),
                ExclusiveZone::Exclusive(u32::MAX),
                Margins {
                    right: i32::MAX,
                    ..Default::default()
                },
            ),
        ] {
            let mut zone = zone_of(output);
            place_layer(output, &mut zone, &first);
            assert!(inside(output, &zone), "{first:?}: {zone:?}");

            // A second surface, maybe of another client, that asks for everything.
            let second = layer(
                Anchor::LEFT,
                (i32::MAX, i32::MAX),
                ExclusiveZone::Neutral,
                Margins::default(),
            );
            let (loc, size) = place_layer(output, &mut zone, &second);
            assert!(
                output.contains_rect(Rectangle::new(loc, size)),
                "{first:?}: {loc:?} {size:?}"
            );
        }
    }

    #[test]
    fn a_negative_margin_still_lets_a_surface_slide_past_its_edge() {
        // A bar of 40 that slides up out of view keeps its size and its zone.
        let output = output();
        let mut zone = zone_of(output);
        let bar = layer(
            Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            (0, 40),
            ExclusiveZone::Exclusive(40),
            Margins {
                top: -30,
                ..Default::default()
            },
        );
        let (loc, size) = place_layer(output, &mut zone, &bar);
        assert_eq!((loc, size), ((0, -30).into(), (1920, 40).into()));
        assert_eq!((zone.loc.y.0, zone.size.h.0), (10, 1070));
    }

    #[test]
    fn test_implied_exclusive_edge() {
        assert_eq!(implied_exclusive_edge_for_anchor(Anchor::empty()), None);
        assert_eq!(implied_exclusive_edge_for_anchor(Anchor::all()), None);
        assert_eq!(
            implied_exclusive_edge_for_anchor(Anchor::BOTTOM | Anchor::LEFT),
            None
        );
        assert_eq!(
            implied_exclusive_edge_for_anchor(Anchor::BOTTOM),
            Some(Anchor::BOTTOM)
        );
        assert_eq!(
            implied_exclusive_edge_for_anchor(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT),
            Some(Anchor::TOP)
        );
    }
}
