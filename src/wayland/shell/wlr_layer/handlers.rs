use std::sync::Mutex;

use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1;
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;
use wayland_server::protocol::wl_surface;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource, Weak};

use crate::utils::{
    Serial,
    alive_tracker::{AliveTracker, IsAlive},
};
use crate::wayland::shell::xdg::XdgPopupSurfaceData;
use crate::wayland::{Dispatch2, GlobalData, GlobalDispatch2, compositor, shell::wlr_layer::Layer};

use super::{
    Anchor, KeyboardInteractivity, LayerSurfaceAttributes, LayerSurfaceCachedState, LayerSurfaceData,
    Margins, WlrLayerShellGlobalData, WlrLayerShellHandler, WlrLayerShellState,
};

use super::LAYER_SURFACE_ROLE;

/*
 * layer_shell
 */

impl<D> GlobalDispatch2<ZwlrLayerShellV1, D> for WlrLayerShellGlobalData
where
    D: Dispatch<ZwlrLayerShellV1, GlobalData>,
    D: Dispatch<ZwlrLayerSurfaceV1, WlrLayerSurfaceUserData>,
    D: WlrLayerShellHandler,
    D: 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: wayland_server::New<ZwlrLayerShellV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ZwlrLayerShellV1, D> for GlobalData
where
    D: Dispatch<ZwlrLayerSurfaceV1, WlrLayerSurfaceUserData>,
    D: WlrLayerShellHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        shell: &ZwlrLayerShellV1,
        request: zwlr_layer_shell_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_layer_shell_v1::Request::GetLayerSurface {
                id,
                surface: wl_surface,
                output,
                layer,
                namespace,
            } => {
                let layer: Layer = match layer.try_into() {
                    Ok(layer) => layer,
                    Err(layer) => {
                        shell.post_error(
                            zwlr_layer_shell_v1::Error::InvalidLayer,
                            format!("invalid layer: {layer:?}"),
                        );
                        return;
                    }
                };

                if compositor::give_role(&wl_surface, LAYER_SURFACE_ROLE).is_err() {
                    shell.post_error(zwlr_layer_shell_v1::Error::Role, "Surface already has a role.");
                    return;
                }

                let id = data_init.init(
                    id,
                    WlrLayerSurfaceUserData {
                        shell_data: state.shell_state().clone(),
                        wl_surface: wl_surface.downgrade(),
                        alive_tracker: Default::default(),
                    },
                );

                let initial = compositor::with_states(&wl_surface, |states| {
                    let inserted = states
                        .data_map
                        .insert_if_missing_threadsafe(|| Mutex::new(LayerSurfaceAttributes::new(id.clone())));

                    if !inserted {
                        let mut attributes = states
                            .data_map
                            .get::<Mutex<LayerSurfaceAttributes>>()
                            .unwrap()
                            .lock()
                            .unwrap();
                        attributes.surface = id.clone();
                    }

                    states
                        .cached_state
                        .get::<LayerSurfaceCachedState>()
                        .pending()
                        .layer = layer;

                    inserted
                });

                if initial {
                    compositor::add_pre_commit_hook::<D, _>(
                        &wl_surface,
                        super::LayerSurface::pre_commit_hook,
                    );
                }

                let handle = super::LayerSurface {
                    wl_surface,
                    shell_surface: id,
                };

                state
                    .shell_state()
                    .known_layers
                    .lock()
                    .unwrap()
                    .push(handle.clone());

                WlrLayerShellHandler::new_layer_surface(state, handle, output, layer, namespace);
            }
            zwlr_layer_shell_v1::Request::Destroy => {
                // Handled by destructor
            }
            _ => {}
        }
    }
}

/*
 * layer_surface
 */

/// User data for wlr layer surface
#[derive(Debug)]
pub struct WlrLayerSurfaceUserData {
    shell_data: WlrLayerShellState,
    // `LayerSurfaceAttributes` stored in the surface `data_map`, contains a
    // `ZwlrLayerSurfaceV1`. So this reference needs to be weak to avoid a
    // cycle.
    wl_surface: Weak<wl_surface::WlSurface>,
    alive_tracker: AliveTracker,
}

impl IsAlive for ZwlrLayerSurfaceV1 {
    #[inline]
    fn alive(&self) -> bool {
        let data: &WlrLayerSurfaceUserData = self.data().unwrap();
        data.alive_tracker.alive()
    }
}

impl<D> Dispatch2<ZwlrLayerSurfaceV1, D> for WlrLayerSurfaceUserData
where
    D: WlrLayerShellHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        layer_surface: &ZwlrLayerSurfaceV1,
        request: zwlr_layer_surface_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_layer_surface_v1::Request::SetSize { width, height } => {
                let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
                    layer_surface.post_error(
                        zwlr_layer_surface_v1::Error::InvalidSize,
                        "width and height can't be greater than i32::MAX",
                    );
                    return;
                };

                let _ = with_surface_pending_state(layer_surface, |data| {
                    data.size = (width, height).into();
                });
            }
            zwlr_layer_surface_v1::Request::SetAnchor { anchor } => {
                match Anchor::try_from(anchor) {
                    Ok(anchor) => {
                        let _ = with_surface_pending_state(layer_surface, |data| {
                            data.anchor = Anchor::from_bits(anchor.bits()).unwrap_or_default();
                        });
                    }
                    Err((err, msg)) => {
                        layer_surface.post_error(err, msg);
                    }
                };
            }
            zwlr_layer_surface_v1::Request::SetExclusiveZone { zone } => {
                let _ = with_surface_pending_state(layer_surface, |data| {
                    data.exclusive_zone = zone.into();
                });
            }
            zwlr_layer_surface_v1::Request::SetMargin {
                top,
                right,
                bottom,
                left,
            } => {
                let _ = with_surface_pending_state(layer_surface, |data| {
                    data.margin = Margins {
                        top,
                        right,
                        bottom,
                        left,
                    };
                });
            }
            zwlr_layer_surface_v1::Request::SetKeyboardInteractivity {
                keyboard_interactivity,
            } => {
                match KeyboardInteractivity::try_from(keyboard_interactivity) {
                    Ok(keyboard_interactivity) => {
                        let _ = with_surface_pending_state(layer_surface, |data| {
                            data.keyboard_interactivity = keyboard_interactivity;
                        });
                    }
                    Err((err, msg)) => {
                        layer_surface.post_error(err, msg);
                    }
                };
            }
            zwlr_layer_surface_v1::Request::SetLayer { layer } => {
                match Layer::try_from(layer) {
                    Ok(layer) => {
                        let _ = with_surface_pending_state(layer_surface, |data| {
                            data.layer = layer;
                        });
                    }
                    Err((err, msg)) => {
                        layer_surface.post_error(err, msg);
                    }
                };
            }
            zwlr_layer_surface_v1::Request::SetExclusiveEdge { edge } => {
                match Anchor::try_from(edge) {
                    Ok(edge) => {
                        let _ = with_surface_pending_state(layer_surface, |data| {
                            if edge.is_empty() {
                                data.exclusive_edge = None;
                            } else if edge.bits().count_ones() == 1 {
                                data.exclusive_edge = Some(edge);
                            } else {
                                layer_surface.post_error(
                                    zwlr_layer_surface_v1::Error::InvalidExclusiveEdge,
                                    "exclusive edge cannot have multiple edges",
                                );
                            }
                        });
                    }
                    Err((err, msg)) => {
                        layer_surface.post_error(err, msg);
                    }
                };
            }
            zwlr_layer_surface_v1::Request::GetPopup { popup } => {
                let Ok(parent_surface) = self.wl_surface.upgrade() else {
                    return;
                };

                let data = popup
                    .data::<crate::wayland::shell::xdg::XdgShellSurfaceUserData>()
                    .unwrap();

                compositor::with_states(&data.wl_surface, move |states| {
                    states
                        .data_map
                        .get::<XdgPopupSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .parent = Some(parent_surface);
                });

                WlrLayerShellHandler::new_popup(
                    state,
                    make_surface_handle(layer_surface).unwrap(),
                    crate::wayland::shell::xdg::handlers::make_popup_handle(&popup),
                );
            }
            zwlr_layer_surface_v1::Request::AckConfigure { serial } => {
                let Ok(surface) = self.wl_surface.upgrade() else {
                    return;
                };

                let serial = Serial::from(serial);

                let found_configure = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .ack_configure(serial)
                });

                let configure = match found_configure {
                    Some(configure) => configure,
                    None => {
                        layer_surface.post_error(
                            zwlr_layer_surface_v1::Error::InvalidSurfaceState,
                            format!("wrong configure serial: {}", <u32>::from(serial)),
                        );
                        return;
                    }
                };

                WlrLayerShellHandler::ack_configure(state, surface, configure);
            }
            zwlr_layer_surface_v1::Request::Destroy => {
                // Handled by destroyed handler
            }
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut D,
        _client_id: wayland_server::backend::ClientId,
        layer_surface: &ZwlrLayerSurfaceV1,
    ) {
        self.alive_tracker.destroy_notify();

        // remove this surface from the known ones (as well as any leftover dead surface)
        let mut layers = self.shell_data.known_layers.lock().unwrap();
        if let Some(index) = layers
            .iter()
            .position(|layer| layer.shell_surface.id() == layer_surface.id())
        {
            let layer = layers.remove(index);
            drop(layers);
            let surface = layer.wl_surface().clone();
            WlrLayerShellHandler::layer_destroyed(state, layer);
            compositor::with_states(&surface, |states| {
                let mut attributes = states
                    .data_map
                    .get::<Mutex<LayerSurfaceAttributes>>()
                    .unwrap()
                    .lock()
                    .unwrap();
                attributes.reset();

                let mut guard = states.cached_state.get::<LayerSurfaceCachedState>();
                *guard.pending() = Default::default();
                *guard.current() = Default::default();
            });
        }
    }
}

fn with_surface_pending_state<F, T>(
    layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    f: F,
) -> Result<T, wayland_server::backend::InvalidId>
where
    F: FnOnce(&mut LayerSurfaceCachedState) -> T,
{
    let data = layer_surface.data::<WlrLayerSurfaceUserData>().unwrap();
    let surface = data.wl_surface.upgrade()?;
    Ok(compositor::with_states(&surface, |states| {
        f(states.cached_state.get::<LayerSurfaceCachedState>().pending())
    }))
}

pub fn make_surface_handle(
    resource: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
) -> Result<crate::wayland::shell::wlr_layer::LayerSurface, wayland_server::backend::InvalidId> {
    let data = resource.data::<WlrLayerSurfaceUserData>().unwrap();
    let wl_surface = data.wl_surface.upgrade()?;
    Ok(crate::wayland::shell::wlr_layer::LayerSurface {
        wl_surface,
        shell_surface: resource.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, sync::Arc};

    use wayland_client::{
        Connection, Dispatch as ClientDispatch, EventQueue, QueueHandle,
        protocol::{wl_compositor, wl_registry, wl_surface as client_wl_surface},
    };
    use wayland_protocols_wlr::layer_shell::v1::client::{
        zwlr_layer_shell_v1 as client_layer_shell, zwlr_layer_surface_v1 as client_layer_surface,
    };
    use wayland_server::{Display, backend::ClientData, protocol::wl_output::WlOutput};

    use super::*;
    use crate::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
    use crate::wayland::shell::wlr_layer::LayerSurface;

    struct ServerState {
        compositor: CompositorState,
        layer_shell: WlrLayerShellState,
    }

    #[derive(Default)]
    struct ServerClientData(CompositorClientState);

    impl ClientData for ServerClientData {}

    impl CompositorHandler for ServerState {
        fn compositor_state(&mut self) -> &mut CompositorState {
            &mut self.compositor
        }

        fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
            &client.get_data::<ServerClientData>().unwrap().0
        }

        fn commit(&mut self, _surface: &wl_surface::WlSurface) {}
    }

    impl WlrLayerShellHandler for ServerState {
        fn shell_state(&mut self) -> &mut WlrLayerShellState {
            &mut self.layer_shell
        }

        fn new_layer_surface(
            &mut self,
            _surface: LayerSurface,
            _output: Option<WlOutput>,
            _layer: Layer,
            _namespace: String,
        ) {
        }
    }

    crate::delegate_dispatch2!(ServerState);

    #[derive(Default)]
    struct ClientState {
        globals: Vec<(u32, String, u32)>,
    }

    impl ClientDispatch<wl_registry::WlRegistry, ()> for ClientState {
        fn event(
            state: &mut Self,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                state.globals.push((name, interface, version));
            }
        }
    }

    wayland_client::delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(ClientState: ignore client_wl_surface::WlSurface);
    wayland_client::delegate_noop!(ClientState: ignore client_layer_shell::ZwlrLayerShellV1);
    wayland_client::delegate_noop!(ClientState: ignore client_layer_surface::ZwlrLayerSurfaceV1);

    fn roundtrip(
        display: &mut Display<ServerState>,
        server: &mut ServerState,
        conn: &Connection,
        queue: &mut EventQueue<ClientState>,
        client: &mut ClientState,
    ) {
        conn.flush().unwrap();
        display.dispatch_clients(server).unwrap();
        display.flush_clients().unwrap();
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
        }
        let _ = queue.dispatch_pending(client);
    }

    #[test]
    fn set_size_above_i32_max_is_a_protocol_error() {
        let mut display = Display::<ServerState>::new().unwrap();
        let mut dh = display.handle();
        let mut server = ServerState {
            compositor: CompositorState::new::<ServerState>(&dh),
            layer_shell: WlrLayerShellState::new::<ServerState>(&dh),
        };

        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        dh.insert_client(server_stream, Arc::new(ServerClientData::default()))
            .unwrap();
        let conn = Connection::from_socket(client_stream).unwrap();
        let mut queue = conn.new_event_queue::<ClientState>();
        let qh = queue.handle();
        let mut client = ClientState::default();

        let registry = conn.display().get_registry(&qh, ());
        roundtrip(&mut display, &mut server, &conn, &mut queue, &mut client);

        let global = |interface: &str| {
            client
                .globals
                .iter()
                .find(|(_, i, _)| i == interface)
                .map(|&(name, _, version)| (name, version))
                .unwrap()
        };
        let (name, version) = global("wl_compositor");
        let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(name, version, &qh, ());
        let (name, version) = global("zwlr_layer_shell_v1");
        let layer_shell = registry.bind::<client_layer_shell::ZwlrLayerShellV1, _, _>(name, version, &qh, ());

        let surface = compositor.create_surface(&qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            client_layer_shell::Layer::Top,
            String::from("test"),
            &qh,
            (),
        );
        layer_surface.set_size(u32::MAX, 1);
        roundtrip(&mut display, &mut server, &conn, &mut queue, &mut client);

        let error = conn.protocol_error().expect("expected a protocol error");
        assert_eq!(error.object_interface, "zwlr_layer_surface_v1");
        assert_eq!(error.code, zwlr_layer_surface_v1::Error::InvalidSize as u32);
    }
}
