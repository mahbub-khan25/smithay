//! XDG Output advertising capabilities
//!
//! This protocol is meant for describing outputs in a way
//! which is more in line with the concept of an output on desktop oriented systems.

use std::sync::{Arc, Mutex, atomic::Ordering};

use tracing::trace;
use wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_v1::ZxdgOutputV1;
use wayland_server::{
    Resource, Weak,
    protocol::wl_output::{self, WlOutput},
};

use crate::utils::{Client, Logical, Physical, Point, Size, Transform};

use super::{Mode, OutputUserData, Scale, XdgOutputUserData};

#[derive(Debug)]
pub(crate) struct Inner {
    name: String,
    description: String,
    pub(super) logical_position: Point<i32, Logical>,

    pub(super) physical_size: Option<Size<i32, Physical>>,
    pub(super) scale: Scale,
    transform: Transform,

    pub instances: Vec<Weak<ZxdgOutputV1>>,
}

/// The size sent as `xdg_output.logical_size` for a mode of `physical` pixels at `scale`,
/// for a client at `client_scale`, after `transform`. Rounded up, as the desktop module's
/// `Space::output_geometry` rounds an output's logical size, so clients are told the same size the compositor's space uses and
/// neighbouring outputs meet with no gap.
fn xdg_logical_size(
    physical: Size<i32, Physical>,
    scale: f64,
    client_scale: f64,
    transform: Transform,
) -> Size<i32, Client> {
    let logical_size = physical
        .to_f64()
        .to_logical(scale)
        .to_client(client_scale)
        .to_i32_ceil();
    transform.transform_size(logical_size)
}

#[derive(Debug, Clone)]
pub(crate) struct XdgOutput {
    pub(crate) inner: Arc<Mutex<Inner>>,
}

impl XdgOutput {
    pub(super) fn new(output: &super::Inner) -> Self {
        trace!(name = output.name, "Creating new xdg_output");

        let physical_size = output.current_mode.map(|mode| mode.size);

        Self {
            inner: Arc::new(Mutex::new(Inner {
                name: output.name.clone(),
                description: output.description.clone(),
                logical_position: output.location,

                physical_size,
                scale: output.scale,
                transform: output.transform,

                instances: Vec::new(),
            })),
        }
    }

    pub(super) fn add_instance(&self, xdg_output: &ZxdgOutputV1, wl_output: &WlOutput) {
        let mut inner = self.inner.lock().unwrap();
        let client_scale = wl_output
            .data::<OutputUserData>()
            .unwrap()
            .client_scale
            .load(Ordering::Acquire);

        let logical_position = inner.logical_position.to_client_precise_round(client_scale);
        xdg_output.logical_position(logical_position.x, logical_position.y);

        if let Some(size) = inner.physical_size {
            let transformed_size = xdg_logical_size(
                size,
                inner.scale.fractional_scale(),
                client_scale,
                inner.transform,
            );
            xdg_output.logical_size(transformed_size.w, transformed_size.h);
        }

        if xdg_output.version() >= 2 {
            xdg_output.name(inner.name.clone());
            xdg_output.description(inner.description.clone());
        }

        // xdg_output.done() is deprecated since version 3
        if xdg_output.version() < 3 {
            xdg_output.done();
        }

        if wl_output.version() >= wl_output::EVT_DONE_SINCE {
            wl_output.done();
        }

        inner.instances.push(xdg_output.downgrade());
    }

    pub(super) fn change_current_state(
        &self,
        new_mode: Option<Mode>,
        new_scale: Option<Scale>,
        new_location: Option<Point<i32, Logical>>,
        new_transform: Option<impl Into<Transform>>,
    ) {
        let mut output = self.inner.lock().unwrap();

        let new_transform = new_transform.map(|x| x.into());

        if let Some(new_mode) = new_mode {
            output.physical_size = Some(new_mode.size);
        }
        if let Some(new_scale) = new_scale {
            output.scale = new_scale;
        }
        if let Some(new_location) = new_location {
            output.logical_position = new_location;
        }
        if let Some(new_transform) = new_transform {
            output.transform = new_transform;
        }

        for instance in output.instances.iter() {
            let Ok(instance) = instance.upgrade() else {
                continue;
            };

            let data = instance.data::<XdgOutputUserData>().unwrap();
            let client_scale = data.client_scale.load(Ordering::Acquire);
            let scale_changed = client_scale != data.last_client_scale.swap(client_scale, Ordering::AcqRel);

            if new_mode.is_some() || new_scale.is_some() || new_transform.is_some() || scale_changed {
                if let Some(size) = output.physical_size {
                    let transformed_size = xdg_logical_size(
                        size,
                        output.scale.fractional_scale(),
                        client_scale,
                        output.transform,
                    );
                    instance.logical_size(transformed_size.w, transformed_size.h);
                }
            }

            if new_location.is_some() || scale_changed {
                let logical_position = output.logical_position.to_client_precise_round(client_scale);
                instance.logical_position(logical_position.x, logical_position.y);
            }

            // xdg_output.done() is deprecated since version 3
            if instance.version() < 3 {
                instance.done();
            }

            // No need for wl_output.done() here, it will be called by caller (super::Output::change_current_state)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_size_is_rounded_up_as_the_space_does() {
        // 1920 / 1.75 = 1097.14 and 1200 / 1.75 = 685.71: up, never to nearest.
        let size = xdg_logical_size((1920, 1200).into(), 1.75, 1.0, Transform::Normal);
        assert_eq!((size.w, size.h), (1098, 686));
        // Exact sizes stay exact.
        let size = xdg_logical_size((2560, 1440).into(), 1.25, 1.0, Transform::Normal);
        assert_eq!((size.w, size.h), (2048, 1152));
        let size = xdg_logical_size((1920, 1200).into(), 1.75, 1.0, Transform::_90);
        assert_eq!((size.w, size.h), (686, 1098));
    }
}
