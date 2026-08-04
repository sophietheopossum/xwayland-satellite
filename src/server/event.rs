use super::clientside::LateInitObjectKey;
use super::decoration::DecorationMarker;
use super::*;
use hecs::{CommandBuffer, World};
use log::{debug, error, trace, warn};
use macros::simple_event_shunt;
use std::os::fd::AsFd;
use wayland_client::{Proxy, protocol as client};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_v1,
        pointer_constraints::zv1::server::{
            zwp_confined_pointer_v1::ZwpConfinedPointerV1 as ConfinedPointerServer,
            zwp_locked_pointer_v1::ZwpLockedPointerV1 as LockedPointerServer,
        },
        relative_pointer::zv1::{
            client::zwp_relative_pointer_v1,
            server::zwp_relative_pointer_v1::ZwpRelativePointerV1 as RelativePointerServer,
        },
        tablet::zv2::{
            client::{
                zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2, zwp_tablet_pad_strip_v2,
                zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2,
                zwp_tablet_v2::{self, ZwpTabletV2 as TabletClient},
            },
            server::{
                zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2 as TabletPadGroupServer,
                zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2 as TabletPadRingServer,
                zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2 as TabletPadStripServer,
                zwp_tablet_pad_v2::ZwpTabletPadV2 as TabletPadServer,
                zwp_tablet_seat_v2::ZwpTabletSeatV2 as TabletSeatServer,
                zwp_tablet_tool_v2::ZwpTabletToolV2 as TabletToolServer,
                zwp_tablet_v2::ZwpTabletV2 as TabletServer,
            },
        },
        viewporter::client::wp_viewport::WpViewport,
    },
    xdg::{
        decoration::zv1::client::zxdg_toplevel_decoration_v1,
        shell::client::{xdg_popup, xdg_surface, xdg_toplevel},
        xdg_output::zv1::{
            client::zxdg_output_v1, server::zxdg_output_v1::ZxdgOutputV1 as XdgOutputServer,
        },
    },
};
use wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer,
    wl_seat::WlSeat, wl_touch::WlTouch,
};

#[derive(Copy, Clone)]
pub(super) struct SurfaceScaleFactor(pub f64);

/// Every output a surface is currently on, in enter order (most recent last).
/// [`OnOutput`] is the surface's positioning anchor; this set decides where to
/// re-anchor when the anchor output itself leaves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct EnteredOutputs(pub Vec<Entity>);

#[derive(hecs::Bundle)]
pub(super) struct SurfaceBundle {
    pub client: client::wl_surface::WlSurface,
    pub server: WlSurface,
    pub viewport: WpViewport,
    pub scale: SurfaceScaleFactor,
    pub entered_outputs: EnteredOutputs,
}

#[derive(Debug)]
pub(crate) enum SurfaceEvents {
    WlSurface(client::wl_surface::Event),
    XdgSurface(xdg_surface::Event),
    Toplevel(xdg_toplevel::Event),
    Popup(xdg_popup::Event),
    FractionalScale(wp_fractional_scale_v1::Event),
    DecorationEvent(zxdg_toplevel_decoration_v1::Event),
}
macro_rules! impl_from {
    ($type:ty, $variant:ident) => {
        impl From<$type> for ObjectEvent {
            fn from(value: $type) -> Self {
                Self::Surface(SurfaceEvents::$variant(value))
            }
        }
    };
}
impl_from!(client::wl_surface::Event, WlSurface);
impl_from!(xdg_surface::Event, XdgSurface);
impl_from!(xdg_toplevel::Event, Toplevel);
impl_from!(xdg_popup::Event, Popup);
impl_from!(wp_fractional_scale_v1::Event, FractionalScale);
impl_from!(zxdg_toplevel_decoration_v1::Event, DecorationEvent);

impl Event for SurfaceEvents {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            SurfaceEvents::WlSurface(event) => Self::surface_event(event, target, state),
            SurfaceEvents::XdgSurface(event) => Self::xdg_event(event, target, state),
            SurfaceEvents::Toplevel(event) => Self::toplevel_event(event, target, state),
            SurfaceEvents::Popup(event) => Self::popup_event(event, target, state),
            SurfaceEvents::FractionalScale(event) => match event {
                wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                    let state = state.deref_mut();
                    let entity = state.world.entity(target).unwrap();
                    let true_fractional = scale as f64 / 120.0;
                    let factor = if state.compositor_scaling {
                        state.global_scale
                    } else {
                        true_fractional
                    };
                    debug!(
                        "{} scale factor: {}",
                        entity.get::<&WlSurface>().unwrap().id(),
                        factor
                    );

                    entity.get::<&mut SurfaceScaleFactor>().unwrap().0 = factor;

                    if let Some(OnOutput(output)) = entity.get::<&OnOutput>().as_deref().copied() {
                        if update_output_scale(
                            state.world.query_one(output).unwrap(),
                            OutputScaleFactor::Fractional(true_fractional),
                            state.compositor_scaling,
                            state.global_scale,
                        ) {
                            state.updated_outputs.push(output);
                        }
                    }
                    if entity.has::<WindowData>() {
                        update_surface_viewport(
                            &state.world,
                            state.world.query_one(target).unwrap(),
                        );
                    }
                }
                _ => unreachable!(),
            },
            SurfaceEvents::DecorationEvent(event) => {
                use zxdg_toplevel_decoration_v1::{Event, Mode};
                let Event::Configure { mode } = event else {
                    error!("unhandled toplevel decoration event: {event:?}");
                    return;
                };

                let entity = state.world.entity(target).unwrap();
                let Some(window_data) = entity.get::<&WindowData>() else {
                    return;
                };
                let Ok(mode) = mode.into_result() else {
                    warn!("unknown decoration mode: {mode:?}");
                    return;
                };

                let needs_server_side_decorations = window_data
                    .attrs
                    .decorations
                    .is_none_or(|d| d.is_serverside());

                if mode == Mode::ServerSide || !needs_server_side_decorations {
                    let mut role = entity.get::<&mut SurfaceRole>().unwrap();
                    if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                        toplevel.decoration.satellite.take();
                    }
                    return;
                }

                let Some((sat_decoration, buf)) = entity
                    .get::<&client::wl_surface::WlSurface>()
                    .and_then(|surface| {
                        DecorationsDataSatellite::try_new(
                            state,
                            &surface,
                            window_data.attrs.title.as_ref().map(WmName::name),
                        )
                    })
                else {
                    warn!("Needed to create decorations for window, but couldn't create them!");
                    return;
                };

                let mut role = entity.get::<&mut SurfaceRole>().unwrap();
                // This should always be the case, but, you never know.
                if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                    toplevel.decoration.satellite = Some(sat_decoration);
                } else {
                    warn!("Created a decoration for a surface that isn't a toplevel?");
                }

                drop(window_data);
                drop(role);
                if let Some(mut buf) = buf {
                    buf.run_on(&mut state.world);
                }
            }
        }
    }
}

impl SurfaceEvents {
    fn surface_event(
        event: client::wl_surface::Event,
        target: Entity,
        state: &mut ServerState<impl XConnection>,
    ) {
        use client::wl_surface::Event;
        let connection = &mut state.connection;
        let state = &mut state.inner;

        let data = state.world.entity(target).unwrap();
        let surface = data.get::<&WlSurface>().unwrap();
        let mut cmd = CommandBuffer::new();
        match event {
            Event::Enter { output } => {
                let output_entity = output.data().copied().unwrap();
                let Ok(output_data) = state.world.entity(output_entity) else {
                    return;
                };
                let Some(output) = output_data.get::<&WlOutput>() else {
                    return;
                };

                surface.enter(&output);

                debug!("{} entered {}", surface.id(), output.id());

                // Track every output the surface is on (the SurfaceBundle
                // guarantees the component exists). The anchor (OnOutput)
                // only changes when its output actually leaves: re-anchoring
                // on every enter turns transient overlaps (bounding-box
                // sweeps during animations, brief flickers on layout
                // changes) into X11 repositioning feedback loops.
                if let Ok(mut entered) = state.world.get::<&mut EnteredOutputs>(target) {
                    entered.0.retain(|e| *e != output_entity);
                    entered.0.push(output_entity);
                }

                let anchor_is_valid = data.get::<&OnOutput>().is_some_and(|o| {
                    state
                        .world
                        .entity(o.0)
                        .is_ok_and(|e| e.has::<OutputDimensions>())
                });

                let mut query = data.query::<(&x::Window, &mut WindowData)>();
                if let Some((window, win_data)) = query.get() {
                    if !anchor_is_valid
                        && anchor_window_to_output(
                            &state.world,
                            *window,
                            win_data,
                            output_entity,
                            &state.global_output_offset,
                            state.last_focused_toplevel,
                            connection,
                        )
                    {
                        cmd.insert_one(target, OnOutput(output_entity));
                    }

                    if state.fractional_scale.is_none() {
                        let output_scale = if state.compositor_scaling {
                            if state.global_scale == 1.0 {
                                // global_scale isn't set this early, so assume 1.0
                                debug!("compositor scaling: global_scale not ready yet, using 1.0");
                            }
                            state.global_scale
                        } else {
                            output_data.get::<&OutputScaleFactor>().unwrap().get()
                        };
                        data.get::<&mut SurfaceScaleFactor>().unwrap().0 = output_scale;
                        drop(query);
                        update_surface_viewport(
                            &state.world,
                            state.world.query_one(target).unwrap(),
                        );
                    } else {
                        let scale = data.get::<&SurfaceScaleFactor>().unwrap();
                        if update_output_scale(
                            state.world.query_one(output_entity).unwrap(),
                            OutputScaleFactor::Fractional(scale.0),
                            state.compositor_scaling,
                            state.global_scale,
                        ) {
                            state.updated_outputs.push(output_entity);
                        }
                    }
                } else {
                    // No X11 window on this surface: either it has not been
                    // associated with one yet (that happens later, when the
                    // Xwayland serial handshake pairs them), or its window has
                    // since been destroyed. Nothing has an X11 position to keep
                    // stable, so just track the most recent output.
                    cmd.insert_one(target, OnOutput(output_entity));
                }
            }
            Event::Leave { output } => {
                let output_entity = output.data().copied().unwrap();
                let Ok(output) = state.world.get::<&WlOutput>(output_entity) else {
                    return;
                };
                surface.leave(&output);

                if let Some(mut entered) = data.get::<&mut EnteredOutputs>() {
                    entered.0.retain(|e| *e != output_entity);
                }

                let anchored_here = data
                    .get::<&OnOutput>()
                    .is_some_and(|o| o.0 == output_entity);
                if anchored_here {
                    // The anchor output is gone for real: re-anchor to the
                    // most recently entered output the surface is still on.
                    let next = data.get::<&EnteredOutputs>().and_then(|entered| {
                        entered.0.iter().rev().copied().find(|e| {
                            state
                                .world
                                .entity(*e)
                                .is_ok_and(|en| en.has::<OutputDimensions>())
                        })
                    });
                    match next {
                        Some(next_output) => {
                            let mut query = data.query::<(&x::Window, &mut WindowData)>();
                            if let Some((window, win_data)) = query.get() {
                                anchor_window_to_output(
                                    &state.world,
                                    *window,
                                    win_data,
                                    next_output,
                                    &state.global_output_offset,
                                    state.last_focused_toplevel,
                                    connection,
                                );
                            }
                            drop(query);
                            cmd.insert_one(target, OnOutput(next_output));
                            // Keep scale bookkeeping in sync with the new
                            // anchor; the compositor-scaling path recomputes
                            // from updated_outputs.
                            state.updated_outputs.push(next_output);
                            let scale = data.get::<&SurfaceScaleFactor>().unwrap();
                            let _ = update_output_scale(
                                state.world.query_one(next_output).unwrap(),
                                OutputScaleFactor::Fractional(scale.0),
                                state.compositor_scaling,
                                state.global_scale,
                            );
                        }
                        None => {
                            cmd.remove_one::<OnOutput>(target);
                        }
                    }
                }
            }
            Event::PreferredBufferScale { .. } => {}
            other => warn!("unhandled surface request: {other:?}"),
        }

        drop(surface);
        cmd.run_on(&mut state.world);
    }

    fn xdg_event<C: XConnection>(
        event: xdg_surface::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        let state = &mut state.inner;
        let xdg_surface::Event::Configure { serial } = event else {
            unreachable!();
        };

        let data = state.world.entity(target).unwrap();
        let mut xdg = hecs::RefMut::map(data.get::<&mut SurfaceRole>().unwrap(), |r| {
            r.xdg_mut().unwrap()
        });
        xdg.surface.ack_configure(serial);
        xdg.configured = true;

        let pending = xdg.pending.take();
        drop(xdg);

        if let Some(pending) = pending {
            let (scale_factor, window, output_offset, _is_popup, popup_parent) = {
                let mut query =
                    data.query::<(&SurfaceScaleFactor, &x::Window, &WindowData, &SurfaceRole)>();
                let (scale_factor, window, window_data, role) = query.get().unwrap();
                let is_popup = matches!(role, SurfaceRole::Popup(_));
                let popup_parent = if let SurfaceRole::Popup(Some(popup_data)) = role {
                    Some(popup_data.parent)
                } else {
                    None
                };
                (
                    scale_factor.0,
                    *window,
                    window_data.output_offset,
                    is_popup,
                    popup_parent,
                )
            };

            let parent_pos_and_decorations_height = if let Some(parent_win) = popup_parent {
                if let Some(&parent_entity) = state.windows.get(&parent_win) {
                    let parent_window_data = state.world.get::<&WindowData>(parent_entity).unwrap();
                    let parent_role = state.world.get::<&SurfaceRole>(parent_entity).unwrap();
                    let mut decorations_height_scaled = 0;
                    if let SurfaceRole::Toplevel(Some(toplevel)) = &*parent_role {
                        if let Some(d) = &toplevel.decoration.satellite {
                            let parent_surface_width = scale_pos_to_logical(
                                parent_window_data.attrs.dims.width as f64,
                                scale_factor,
                            );
                            if d.will_draw_decorations(parent_surface_width) {
                                decorations_height_scaled =
                                    scale_pos_to_physical(d.titlebar_height() as f64, scale_factor);
                            }
                        }
                    }
                    Some((
                        parent_window_data.attrs.dims.x,
                        parent_window_data.attrs.dims.y,
                        decorations_height_scaled,
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            let (x, y) = if let Some((px, py, dec_h)) = parent_pos_and_decorations_height {
                let rx = px as i32 + scale_pos_to_physical(pending.x as f64, scale_factor);
                let ry = py as i32 + dec_h + scale_pos_to_physical(pending.y as f64, scale_factor);
                (rx, ry)
            } else {
                let rx = scale_pos_to_physical(pending.x as f64, scale_factor) + output_offset.x;
                let ry = scale_pos_to_physical(pending.y as f64, scale_factor) + output_offset.y;
                (rx, ry)
            };

            let width = if pending.width > 0 {
                scale_size_to_physical(pending.width as f64, scale_factor).clamp(1, u16::MAX as i32)
                    as u16
            } else {
                let window_data = state.world.get::<&WindowData>(target).unwrap();
                window_data.attrs.dims.width
            };
            let mut height = if pending.height > 0 {
                scale_size_to_physical(pending.height as f64, scale_factor)
                    .clamp(1, u16::MAX as i32) as u16
            } else {
                let window_data = state.world.get::<&WindowData>(target).unwrap();
                window_data.attrs.dims.height
            };
            debug!(
                "configuring {} ({window:?}): {x}x{y}, {width}x{height}",
                data.get::<&WlSurface>().unwrap().id(),
            );

            let role = state.world.get::<&SurfaceRole>(target).unwrap();
            if let SurfaceRole::Toplevel(Some(toplevel)) = &*role {
                if let Some(d) = &toplevel.decoration.satellite {
                    let surface_width = scale_pos_to_logical(width as f64, scale_factor);
                    if d.will_draw_decorations(surface_width) {
                        let bar_h_scaled =
                            scale_size_to_physical(d.titlebar_height() as f64, scale_factor)
                                .clamp(0, u16::MAX as i32) as u16;
                        height = height.saturating_sub(bar_h_scaled).max(1);
                    }
                }
            }
            drop(role);

            {
                let mut window_data = state.world.get::<&mut WindowData>(target).unwrap();
                window_data.attrs.dims = WindowDims {
                    x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width,
                    height,
                };
            }
            let pending = PendingSurfaceState {
                x,
                y,
                width: width as _,
                height: height as _,
            };
            state.world.insert_one(target, pending).unwrap();
            update_surface_viewport(&state.world, state.world.query_one(target).unwrap());
        }

        let (surface, attach, callback) = state
            .world
            .query_one_mut::<(
                &client::wl_surface::WlSurface,
                Option<&SurfaceAttach>,
                Option<&WlCallback>,
            )>(target)
            .unwrap();

        let mut cmd = CommandBuffer::new();

        if let Some(SurfaceAttach { buffer, x, y }) = attach {
            surface.attach(buffer.as_ref(), *x, *y);
            cmd.remove_one::<SurfaceAttach>(target);
        }
        if let Some(cb) = callback {
            surface.frame(&state.qh, cb.clone());
            cmd.remove_one::<client::wl_callback::WlCallback>(target);
        }
        surface.commit();
        cmd.run_on(&mut state.world);
    }

    fn toplevel_event<C: XConnection>(
        event: xdg_toplevel::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        let data = state.inner.world.entity(target).unwrap();
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                debug!(
                    "configuring toplevel {} {width}x{height}, {states:?}",
                    data.get::<&WlSurface>().unwrap().id()
                );

                let mut role = data.get::<&mut SurfaceRole>().unwrap();
                if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                    let prev_fs = toplevel.fullscreen;
                    toplevel.fullscreen =
                        states.contains(&(u32::from(xdg_toplevel::State::Fullscreen) as u8));
                    if toplevel.fullscreen != prev_fs {
                        state.connection.set_fullscreen(
                            *data.get::<&x::Window>().unwrap(),
                            toplevel.fullscreen,
                        );
                        if let Some(decorations) = toplevel.decoration.satellite.as_mut() {
                            decorations.handle_fullscreen(toplevel.fullscreen);
                        }
                    }
                };

                role.xdg_mut().unwrap().pending = Some(PendingSurfaceState {
                    width,
                    height,
                    ..Default::default()
                });
            }
            xdg_toplevel::Event::Close => {
                let window = *data.get::<&x::Window>().unwrap();
                state.close_x_window(window);
            }
            // TODO: support capabilities (minimize, maximize, etc)
            xdg_toplevel::Event::WmCapabilities { .. } => {}
            xdg_toplevel::Event::ConfigureBounds { .. } => {}
            ref other => warn!("unhandled xdgtoplevel event: {other:?}"),
        }
    }

    fn popup_event<C: XConnection>(
        event: xdg_popup::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        let data = state.inner.world.entity(target).unwrap();
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                trace!(
                    "popup configure {}: {x}x{y}, {width}x{height}",
                    data.get::<&WlSurface>().unwrap().id()
                );

                let mut role = data.get::<&mut SurfaceRole>().unwrap();
                let xdg = role.xdg_mut().unwrap();
                let first_configure = !xdg.configured;
                xdg.pending = Some(PendingSurfaceState {
                    x,
                    y,
                    width,
                    height,
                });

                if first_configure {
                    let window_data = data.get::<&WindowData>().unwrap();
                    if window_data.attrs.require_wm_focus() {
                        let window = *data.get::<&x::Window>().unwrap();
                        state.inner.to_focus = Some(FocusData {
                            window,
                            output_name: None,
                            is_popup: true,
                        });
                    }
                }
            }
            xdg_popup::Event::Repositioned { .. } => {}
            xdg_popup::Event::PopupDone => {
                state
                    .connection
                    .unmap_window(*data.get::<&x::Window>().unwrap());
            }
            other => warn!("unhandled xdg popup event: {other:?}"),
        }
    }
}

pub(super) fn update_surface_viewport(
    world: &World,
    mut surface_query: hecs::QueryOne<(
        &WindowData,
        &WpViewport,
        &SurfaceScaleFactor,
        Option<&mut SurfaceRole>,
        &WlSurface,
    )>,
) {
    let (window_data, viewport, scale_factor, mut role, surface) = surface_query.get().unwrap();
    let dims = &window_data.attrs.dims;
    let size_hints = &window_data.attrs.size_hints;

    let width = (dims.width as f64 / scale_factor.0).ceil() as i32;
    let height = (dims.height as f64 / scale_factor.0).ceil() as i32;
    if width > 0 && height > 0 {
        viewport.set_destination(width, height);
    }

    let mut toplevel_data = match &mut role {
        Some(SurfaceRole::Toplevel(Some(data))) => Some(data),
        _ => None,
    };
    if let Some(d) = toplevel_data
        .as_mut()
        .and_then(|d| d.decoration.satellite.as_deref_mut())
    {
        d.draw_decorations(world, width, scale_factor.0 as f32);
    }
    debug!("{} viewport: {width}x{height}", surface.id());

    if let Some(hints) = size_hints {
        let Some(data) = toplevel_data else {
            return;
        };

        let decorations_height = data
            .decoration
            .satellite
            .as_ref()
            .map(|s| s.titlebar_height())
            .unwrap_or(0);

        if let Some(min) = hints.min_size {
            debug!(
                "updated min height: {}",
                (min.height as f64 / scale_factor.0) as i32 + decorations_height
            );
            data.toplevel.set_min_size(
                (min.width as f64 / scale_factor.0) as i32,
                (min.height as f64 / scale_factor.0) as i32 + decorations_height,
            );
        }
        if let Some(max) = hints.max_size {
            data.toplevel.set_max_size(
                (max.width as f64 / scale_factor.0) as i32,
                (max.height as f64 / scale_factor.0) as i32 + decorations_height,
            );
        }
    }
}

impl Event for client::wl_buffer::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        // The only event from a buffer would be the release.
        state.world.get::<&WlBuffer>(target).unwrap().release();
    }
}

impl Event for client::wl_seat::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&WlSeat>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Capabilities { |capabilities| convert_wenum(capabilities) },
                Name { name }
            ]
        }
    }
}

struct PendingEnter(client::wl_pointer::Event);
#[inline]
pub(super) fn safe_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

#[inline]
pub(super) fn scale_pos_to_physical(value: f64, scale: f64) -> i32 {
    (value * safe_scale(scale)).round() as i32
}

#[inline]
pub(super) fn scale_pos_to_logical(value: f64, scale: f64) -> i32 {
    (value / safe_scale(scale)).round() as i32
}

#[inline]
pub(super) fn scale_size_to_physical(value: f64, scale: f64) -> i32 {
    (value * safe_scale(scale)).ceil().max(1.0) as i32
}

#[inline]
pub(super) fn scale_size_to_logical(value: f64, scale: f64) -> i32 {
    (value / safe_scale(scale)).ceil().max(1.0) as i32
}

#[inline]
pub(super) fn scale_input_to_xwayland(value: f64, scale: f64) -> f64 {
    value * safe_scale(scale)
}
#[derive(Clone, Copy)]
pub(super) enum CurrentSurface {
    Xwayland(Entity),
    Decoration(Entity),
}

impl CurrentSurface {
    fn is_decoration(&self) -> bool {
        matches!(self, Self::Decoration(..))
    }
}

#[derive(Default)]
pub(super) struct TouchFocus {
    pub points: std::collections::HashMap<i32, CurrentSurface>,
}
pub struct LastClickSerial(pub client::wl_seat::WlSeat, pub u32);

impl Event for client::wl_pointer::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        // Workaround GTK (stupidly) autoclosing popups if it receives an wl_pointer.enter
        // event shortly after creation.
        // When Niri creates a popup, it immediately sends wl_pointer.enter on the new surface,
        // generating an EnterNotify event, and Xwayland will send a release button event.
        // In its menu implementation, GTK treats EnterNotify "this menu is now active" and will
        // destroy the menu if this occurs within a 500 ms interval (which it always does with
        // Niri). Other compositors do not run into this problem because they appear to not send
        // wl_pointer.enter until the user actually moves the mouse in the popup.

        fn handle_pending_enter<C: XConnection>(
            target: Entity,
            state: &mut ServerState<C>,
            event_str: &str,
        ) -> bool {
            loop {
                let Ok(pe) = state.world.get::<&PendingEnter>(target) else {
                    return true;
                };
                let PendingEnter(client::wl_pointer::Event::Enter {
                    serial,
                    surface,
                    surface_x,
                    surface_y,
                }) = pe.deref()
                else {
                    unreachable!();
                };
                if surface
                    .data()
                    .copied()
                    .is_some_and(|key| state.world.contains(key))
                {
                    trace!("resending enter ({serial}) before {}", event_str);
                    let enter_event = client::wl_pointer::Event::Enter {
                        serial: *serial,
                        surface: surface.clone(),
                        surface_x: *surface_x,
                        surface_y: *surface_y,
                    };

                    drop(pe);
                    Event::handle(enter_event, target, state);
                } else {
                    warn!("could not move pointer to surface: stale surface");
                    return false;
                }
            }
        }

        match self {
            Self::Enter {
                serial,
                ref surface,
                surface_x,
                surface_y,
            } => {
                let connection = &mut state.connection;
                let state = &mut state.inner;
                let mut cmd = CommandBuffer::new();
                let pending_enter = state.world.remove_one::<PendingEnter>(target).ok();
                let surface_entity = surface.data().copied();
                let mut query = surface_entity.and_then(|e| {
                    state
                        .world
                        .query_one::<(&WlSurface, &SurfaceRole, &SurfaceScaleFactor, &x::Window)>(e)
                        .ok()
                });
                let Some((surface, role, scale, window)) = query.as_mut().and_then(|q| q.get())
                else {
                    if let Some(&DecorationMarker { parent }) = surface.data() {
                        drop(query);
                        state
                            .world
                            .insert_one(target, CurrentSurface::Decoration(parent))
                            .unwrap();
                    } else {
                        warn!("could not enter surface {}: stale surface", surface.id());
                    }

                    return;
                };

                let server = state.world.get::<&WlPointer>(target).unwrap();
                cmd.insert(target, (*scale,));

                let surface_is_popup = matches!(role, SurfaceRole::Popup(_));
                let pointer_coords = if state.compositor_scaling {
                    surface_to_xwayland_input_coords(
                        &state.world,
                        surface_entity.unwrap(),
                        surface_x,
                        surface_y,
                    )
                } else {
                    (surface_x * scale.0, surface_y * scale.0)
                };
                let mut do_enter = || {
                    debug!("pointer entering {} ({serial} {})", surface.id(), scale.0);
                    server.enter(serial, surface, pointer_coords.0, pointer_coords.1);
                    connection.raise_to_top(*window);
                    if !surface_is_popup {
                        state.last_hovered = Some(*window);
                    }
                    cmd.insert_one(target, CurrentSurface::Xwayland(surface_entity.unwrap()));
                };

                if !surface_is_popup {
                    do_enter();
                } else {
                    match pending_enter {
                        Some(e) => {
                            let PendingEnter(client::wl_pointer::Event::Enter {
                                serial: pending_serial,
                                ..
                            }) = e
                            else {
                                unreachable!();
                            };
                            if serial == pending_serial {
                                do_enter();
                            } else {
                                cmd.insert(target, (PendingEnter(self),));
                            }
                        }
                        None => {
                            cmd.insert(target, (PendingEnter(self),));
                        }
                    }
                }
                drop(query);
                drop(server);
                cmd.run_on(&mut state.world);
            }
            Self::Leave { serial, surface } => {
                let _ = state.world.remove_one::<PendingEnter>(target);
                if !surface.is_alive() {
                    return;
                }
                debug!("leaving surface ({})", surface.id());
                if let Ok(CurrentSurface::Decoration(parent)) =
                    state.world.remove_one::<CurrentSurface>(target)
                {
                    decoration::handle_pointer_leave(state, parent);
                    return;
                }

                if let Some(surface) = surface
                    .data()
                    .copied()
                    .and_then(|key| state.world.get::<&WlSurface>(key).ok())
                {
                    state
                        .world
                        .get::<&WlPointer>(target)
                        .unwrap()
                        .leave(serial, &surface);
                } else {
                    warn!("could not leave surface: stale surface");
                }
            }
            Self::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                if !handle_pending_enter(target, state, "motion") {
                    return;
                }
                {
                    let Ok(surface) = state.world.get::<&CurrentSurface>(target) else {
                        warn!("could not motion on surface: stale surface");
                        return;
                    };
                    if let CurrentSurface::Decoration(parent) = &*surface {
                        decoration::handle_pointer_motion(state, *parent, surface_x, surface_y);
                        return;
                    }
                }
                let pointer_coords = state
                    .world
                    .get::<&CurrentSurface>(target)
                    .ok()
                    .and_then(|surf| match &*surf {
                        CurrentSurface::Xwayland(e) => {
                            if state.compositor_scaling {
                                Some(surface_to_xwayland_input_coords(
                                    &state.world,
                                    *e,
                                    surface_x,
                                    surface_y,
                                ))
                            } else {
                                state
                                    .world
                                    .get::<&SurfaceScaleFactor>(*e)
                                    .ok()
                                    .map(|s| (surface_x * s.0, surface_y * s.0))
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or((surface_x, surface_y));
                let server = state.world.get::<&WlPointer>(target).unwrap();
                trace!(
                    target: "pointer_position",
                    "pointer motion {} {}",
                    pointer_coords.0,
                    pointer_coords.1
                );
                server.motion(time, pointer_coords.0, pointer_coords.1);
            }
            Self::Button {
                serial,
                time,
                button,
                state: button_state,
            } => {
                if !handle_pending_enter(target, state, "click") {
                    return;
                }
                let mut cmd = CommandBuffer::new();

                let Ok(mut query) =
                    state
                        .world
                        .query_one::<(&WlPointer, &client::wl_seat::WlSeat, &CurrentSurface)>(
                            target,
                        )
                else {
                    warn!("could not click on surface: stale surface");
                    return;
                };

                let (server, seat, current_surface) = query.get().unwrap();

                // from linux/input-event-codes.h
                mod button_codes {
                    pub const LEFT: u32 = 0x110;
                }

                if button_state == WEnum::Value(client::wl_pointer::ButtonState::Pressed)
                    && button == button_codes::LEFT
                {
                    match current_surface {
                        CurrentSurface::Xwayland(entity) => {
                            cmd.insert(*entity, (LastClickSerial(seat.clone(), serial),));
                        }
                        CurrentSurface::Decoration(parent) => {
                            let seat = seat.clone();
                            let parent = *parent;
                            drop(query);
                            decoration::handle_pointer_click(state, parent, &seat, serial);
                            return;
                        }
                    }
                }

                server.button(serial, time, button, convert_wenum(button_state));
                drop(query);
                cmd.run_on(&mut state.world);
            }
            _ => {
                let (server, current_surface) = state
                    .world
                    .query_one_mut::<(&WlPointer, Option<&CurrentSurface>)>(target)
                    .unwrap();

                if current_surface.is_some_and(CurrentSurface::is_decoration) {
                    return;
                }
                simple_event_shunt! {
                    server, self => [
                        Frame,
                        Axis {
                            time,
                            |axis| convert_wenum(axis),
                            value
                        },
                        AxisSource {
                            |axis_source| convert_wenum(axis_source)
                        },
                        AxisStop {
                            time,
                            |axis| convert_wenum(axis)
                        },
                        AxisDiscrete {
                            |axis| convert_wenum(axis),
                            discrete
                        },
                        AxisValue120 {
                            |axis| convert_wenum(axis),
                            value120
                        },
                        AxisRelativeDirection {
                            |axis| convert_wenum(axis),
                            |direction| convert_wenum(direction)
                        }
                    ]
                }
            }
        }
    }
}

impl Event for client::wl_keyboard::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let state = state.deref_mut();
        let data = state.world.entity(target).unwrap();
        let keyboard = data.get::<&WlKeyboard>().unwrap();
        match self {
            client::wl_keyboard::Event::Enter {
                serial,
                surface,
                keys,
            } => {
                let mut query = surface.data().copied().and_then(|key| {
                    state
                        .world
                        .query_one::<(&x::Window, &WlSurface, Option<&OnOutput>)>(key)
                        .ok()
                });
                let Some((window, surface, output)) = query.as_mut().and_then(|q| q.get()) else {
                    return;
                };
                state.last_kb_serial = Some((
                    data.get::<&client::wl_seat::WlSeat>()
                        .as_deref()
                        .unwrap()
                        .clone(),
                    serial,
                ));
                let output_name = get_output_name(output, &state.world);
                state.to_focus = Some(FocusData {
                    window: *window,
                    output_name,
                    is_popup: false,
                });
                keyboard.enter(serial, surface, keys);
            }
            client::wl_keyboard::Event::Leave { serial, surface } => {
                if !surface.is_alive() {
                    return;
                }
                let mut query = surface
                    .data()
                    .copied()
                    .and_then(|key| state.world.query_one::<(&x::Window, &WlSurface)>(key).ok());
                let Some((window, surface)) = query.as_mut().and_then(|q| q.get()) else {
                    return;
                };
                if state.to_focus.as_ref().map(|d| d.window) == Some(*window) {
                    state.to_focus.take();
                } else {
                    state.unfocus = true;
                }
                keyboard.leave(serial, surface);
            }
            client::wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state: key_state,
            } => {
                state.last_kb_serial = Some((
                    data.get::<&client::wl_seat::WlSeat>()
                        .as_deref()
                        .unwrap()
                        .clone(),
                    serial,
                ));
                keyboard.key(serial, time, key, convert_wenum(key_state));
            }
            _ => simple_event_shunt! {
                keyboard, self => [
                    Keymap {
                        |format| convert_wenum(format),
                        |fd| fd.as_fd(),
                        size
                    },
                    Modifiers {
                        serial,
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        group
                    },
                    RepeatInfo {
                        rate,
                        delay
                    }
                ]
            },
        }
    }
}

impl Event for client::wl_touch::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        use client::wl_touch::Event::*;
        match self {
            Down {
                serial,
                time,
                surface,
                id,
                x,
                y,
            } => {
                let mut cmd = CommandBuffer::new();
                {
                    let connection = &mut state.connection;
                    let world = &mut state.inner.world;
                    let s_entity = surface.data().copied();

                    let mut s_query = s_entity.and_then(|key| {
                        world
                            .query_one::<(&WlSurface, &SurfaceScaleFactor, &x::Window)>(key)
                            .ok()
                    });

                    let target_info = s_query
                        .as_mut()
                        .and_then(|q| q.get())
                        .map(|(s_surf, scale_factor, win)| (s_surf.clone(), scale_factor.0, *win));
                    drop(s_query);

                    if let Some((s_surface, scale_factor, window)) = target_info {
                        let Some(s_entity) = s_entity else {
                            warn!("touch down failed: stale surface");
                            return;
                        };

                        let scales = if state.inner.compositor_scaling {
                            get_surface_input_scales(world, s_entity)
                        } else {
                            (scale_factor, scale_factor)
                        };

                        let mut focus = world.remove_one::<TouchFocus>(target).unwrap_or_default();
                        focus.points.insert(id, CurrentSurface::Xwayland(s_entity));
                        cmd.insert_one(target, focus);

                        connection.raise_to_top(window);

                        if let Ok(touch) = world.get::<&WlTouch>(target) {
                            touch.down(
                                serial,
                                time,
                                &s_surface,
                                id,
                                scale_input_to_xwayland(x, scales.0),
                                scale_input_to_xwayland(y, scales.1),
                            );
                        } else {
                            warn!("touch down failed: stale touch object");
                        }
                    } else if let Some(&DecorationMarker { parent }) = surface.data() {
                        let mut focus = world.remove_one::<TouchFocus>(target).unwrap_or_default();
                        focus.points.insert(id, CurrentSurface::Decoration(parent));
                        cmd.insert_one(target, focus);

                        let seat = {
                            let Ok(seat_ref) = state.world.get::<&client::wl_seat::WlSeat>(target)
                            else {
                                warn!("touch decoration click failed: stale seat");
                                return;
                            };
                            (*seat_ref).clone()
                        };
                        decoration::handle_pointer_motion(state, parent, x, y);
                        decoration::handle_pointer_click(state, parent, &seat, serial);
                    } else {
                        warn!("touch down failed: stale surface");
                    }
                }
                cmd.run_on(&mut state.world);
            }
            Motion { time, id, x, y } => {
                let Some(current_surface) = state
                    .world
                    .get::<&TouchFocus>(target)
                    .ok()
                    .and_then(|focus| focus.points.get(&id).copied())
                else {
                    warn!("touch motion failed: missing touch focus for id {id}");
                    return;
                };

                let scales = if state.inner.compositor_scaling {
                    match current_surface {
                        CurrentSurface::Xwayland(e) => get_surface_input_scales(&state.world, e),
                        CurrentSurface::Decoration(_) => (1.0, 1.0),
                    }
                } else {
                    match current_surface {
                        CurrentSurface::Xwayland(e) => {
                            let scale = state
                                .world
                                .get::<&SurfaceScaleFactor>(e)
                                .ok()
                                .map(|s| s.0)
                                .unwrap_or(1.0);
                            (scale, scale)
                        }
                        CurrentSurface::Decoration(_) => (1.0, 1.0),
                    }
                };

                match current_surface {
                    CurrentSurface::Xwayland(_) => {
                        if let Ok(touch) = state.world.get::<&WlTouch>(target) {
                            touch.motion(
                                time,
                                id,
                                scale_input_to_xwayland(x, scales.0),
                                scale_input_to_xwayland(y, scales.1),
                            );
                        } else {
                            warn!("touch motion failed: stale touch object");
                        }
                    }
                    CurrentSurface::Decoration(parent) => {
                        decoration::handle_pointer_motion(state, parent, x, y);
                    }
                }
            }
            Up { serial, time, id } => {
                let mut cmd = CommandBuffer::new();
                let remove_focus = if let Ok(mut focus) = state.world.get::<&mut TouchFocus>(target)
                {
                    focus.points.remove(&id);
                    focus.points.is_empty()
                } else {
                    false
                };

                if remove_focus {
                    cmd.remove_one::<TouchFocus>(target);
                }

                if let Ok(touch) = state.world.get::<&WlTouch>(target) {
                    touch.up(serial, time, id);
                } else {
                    warn!("touch up failed: stale touch object");
                }
                cmd.run_on(&mut state.world);
            }
            Cancel => {
                let mut cmd = CommandBuffer::new();
                cmd.remove_one::<TouchFocus>(target);
                if let Ok(touch) = state.world.get::<&WlTouch>(target) {
                    touch.cancel();
                } else {
                    warn!("touch cancel failed: stale touch object");
                }
                cmd.run_on(&mut state.world);
            }
            Frame => {
                if let Ok(touch) = state.world.get::<&WlTouch>(target) {
                    touch.frame();
                } else {
                    warn!("touch frame failed: stale touch object");
                }
            }
            Shape { id, major, minor } => {
                if let Ok(touch) = state.world.get::<&WlTouch>(target) {
                    touch.shape(id, major, minor);
                } else {
                    warn!("touch shape failed: stale touch object");
                }
            }
            Orientation { id, orientation } => {
                if let Ok(touch) = state.world.get::<&WlTouch>(target) {
                    touch.orientation(id, orientation);
                } else {
                    warn!("touch orientation failed: stale touch object");
                }
            }
            _ => warn!("unhandled touch event: {:?}", self),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) struct OnOutput(pub Entity);

struct OutputName(String);
fn get_output_name(output: Option<&OnOutput>, world: &World) -> Option<String> {
    output.map(|o| world.get::<&OutputName>(o.0).unwrap().0.clone())
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub(super) enum OutputScaleFactor {
    Output(i32),
    Fractional(f64),
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct TrueOutputScaleFactor(pub OutputScaleFactor);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct X11OutputPosition {
    pub x: i32,
    pub y: i32,
}

pub(super) fn recalculate_x11_output_positions(
    world: &mut World,
    compositor_scaling: bool,
    global_scale: f64,
) {
    if !compositor_scaling {
        return;
    }

    struct OutputNode {
        entity: Entity,
        lx: f64,
        ly: f64,
        rx: Option<i32>,
        ry: Option<i32>,
    }

    // treat outputs as nodes in the compositor layout
    let mut nodes: Vec<_> = world
        .query::<(&OutputDimensions, &TrueOutputScaleFactor)>()
        .iter()
        .map(|(e, (d, _s))| OutputNode {
            entity: e,
            lx: d.x as f64,
            ly: d.y as f64,
            rx: None,
            ry: None,
        })
        .collect();

    if nodes.is_empty() {
        return;
    }

    // anchor near zero, then walk neighbors so mixed-dpi layouts stay flush
    let root_idx = nodes
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let dist_a = a.lx * a.lx + a.ly * a.ly;
            let dist_b = b.lx * b.lx + b.ly * b.ly;
            dist_a.total_cmp(&dist_b)
        })
        .map(|(i, _)| i)
        .unwrap();

    nodes[root_idx].rx = Some(0);
    nodes[root_idx].ry = Some(0);

    // nearest placed output gives the least surprising seam
    loop {
        let mut progress = false;
        let mut best_transition = None;

        for (u_idx, u_node) in nodes.iter().enumerate() {
            if u_node.rx.is_some() {
                continue;
            }
            for (r_idx, r_node) in nodes.iter().enumerate() {
                if r_node.rx.is_none() {
                    continue;
                }
                let dx = u_node.lx - r_node.lx;
                let dy = u_node.ly - r_node.ly;
                let dist = dx * dx + dy * dy;

                match best_transition {
                    None => {
                        best_transition = Some((u_idx, r_idx, dist));
                    }
                    Some((_, _, min_dist)) if dist < min_dist => {
                        best_transition = Some((u_idx, r_idx, dist));
                    }
                    _ => {}
                }
            }
        }

        if let Some((u_idx, r_idx, _)) = best_transition {
            let u_node = &nodes[u_idx];
            let r_node = &nodes[r_idx];

            let dx = u_node.lx - r_node.lx;
            let dy = u_node.ly - r_node.ly;

            // compositor scaling wants one x11-sized grid
            let rx = r_node.rx.unwrap() + scale_pos_to_physical(dx, global_scale);
            let ry = r_node.ry.unwrap() + scale_pos_to_physical(dy, global_scale);

            nodes[u_idx].rx = Some(rx);
            nodes[u_idx].ry = Some(ry);
            progress = true;
        }

        if !progress {
            break;
        }
    }

    for node in nodes {
        if node.rx.is_none() {
            warn!(
                "Failed to calculate relative X11 position for output {:?}, falling back to (0, 0)",
                node.entity
            );
        }
        let rx = node.rx.unwrap_or(0);
        let ry = node.ry.unwrap_or(0);
        world
            .insert_one(node.entity, X11OutputPosition { x: rx, y: ry })
            .unwrap();
    }
}

pub(super) fn surface_to_xwayland_input_coords(
    world: &World,
    e: Entity,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let scales = get_surface_input_scales(world, e);
    (
        scale_input_to_xwayland(x, scales.0),
        scale_input_to_xwayland(y, scales.1),
    )
}

pub(super) fn xwayland_to_surface_input_coords(
    world: &World,
    e: Entity,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let scales = get_surface_input_scales(world, e);
    let scale_x = safe_scale(scales.0);
    let scale_y = safe_scale(scales.1);
    (x / scale_x, y / scale_y)
}

pub(super) fn get_surface_input_scales(world: &World, e: Entity) -> (f64, f64) {
    let target_e = if world.contains(e) && world.get::<&OnOutput>(e).is_err() {
        if let Ok(marker) = world.get::<&DecorationMarker>(e) {
            marker.parent
        } else {
            let surf = world.get::<&WlSurface>(e).ok();
            let mut query = world.query::<(&x::Window, &WlSurface)>();
            query
                .into_iter()
                .find(|(win_entity, (_, wl_surf))| {
                    *win_entity == e
                        || surf
                            .as_ref()
                            .map(|s| wl_surf.id() == s.id())
                            .unwrap_or(false)
                })
                .map(|(win_entity, _)| win_entity)
                .or_else(|| {
                    let mut on_output_query = world.query::<(&x::Window, &OnOutput)>();
                    on_output_query
                        .into_iter()
                        .next()
                        .map(|(win_entity, _)| win_entity)
                })
                .unwrap_or(e)
        }
    } else {
        e
    };

    let scale = world
        .get::<&SurfaceScaleFactor>(target_e)
        .ok()
        .map(|s| s.0)
        .filter(|s| s.is_finite() && *s > 0.0)
        .or_else(|| {
            world.get::<&OnOutput>(target_e).ok().and_then(|on_output| {
                world
                    .get::<&TrueOutputScaleFactor>(on_output.0)
                    .ok()
                    .map(|s| s.0.get())
                    .or_else(|| {
                        world
                            .get::<&OutputScaleFactor>(on_output.0)
                            .ok()
                            .map(|s| s.get())
                    })
            })
        })
        .unwrap_or(1.0);
    let win_data = world.get::<&WindowData>(target_e).ok();

    let is_fullscreen = world
        .get::<&SurfaceRole>(target_e)
        .ok()
        .map(|r| match &*r {
            SurfaceRole::Toplevel(Some(toplevel)) => toplevel.fullscreen,
            _ => false,
        })
        .unwrap_or(false);

    if let Ok(on_output) = world.get::<&OnOutput>(target_e) {
        if let Ok(dims) = world.get::<&OutputDimensions>(on_output.0) {
            let (lw, lh) = match (dims.logical_width, dims.logical_height) {
                (Some(lw), Some(lh)) => (lw, lh),
                _ => {
                    if dims.rotated_90 {
                        (dims.height, dims.width)
                    } else {
                        (dims.width, dims.height)
                    }
                }
            };
            let (physical_w, physical_h) = if dims.rotated_90 {
                (dims.height as f64, dims.width as f64)
            } else {
                (dims.width as f64, dims.height as f64)
            };
            let logical_w = lw as f64;
            let logical_h = lh as f64;
            if logical_w.is_finite()
                && logical_w > 0.0
                && logical_h.is_finite()
                && logical_h > 0.0
                && physical_w > 0.0
                && physical_h > 0.0
            {
                if is_fullscreen {
                    if let Some(win) = &win_data {
                        let win_w = win.attrs.dims.width as f64;
                        let win_h = win.attrs.dims.height as f64;
                        if win_w > 0.0 && win_h > 0.0 {
                            let scale_x = win_w / logical_w;
                            let scale_y = win_h / logical_h;
                            if scale_x.is_finite()
                                && scale_y.is_finite()
                                && scale_x > 0.0
                                && scale_y > 0.0
                            {
                                let (physical_w, physical_h) = if dims.rotated_90 {
                                    (dims.height as f64, dims.width as f64)
                                } else {
                                    (dims.width as f64, dims.height as f64)
                                };
                                let eff_scale = world
                                    .get::<&TrueOutputScaleFactor>(on_output.0)
                                    .ok()
                                    .map(|s| s.0.get())
                                    .or_else(|| {
                                        world
                                            .get::<&OutputScaleFactor>(on_output.0)
                                            .ok()
                                            .map(|s| s.get())
                                    })
                                    .unwrap_or(scale);

                                let expected_w = if (win_w - physical_w * eff_scale).abs()
                                    < (win_w - physical_w).abs()
                                {
                                    physical_w * eff_scale
                                } else {
                                    physical_w
                                };
                                let expected_h = if (win_h - physical_h * eff_scale).abs()
                                    < (win_h - physical_h).abs()
                                {
                                    physical_h * eff_scale
                                } else {
                                    physical_h
                                };

                                let width_error = (win_w - expected_w).abs() / expected_w;
                                let height_error = (win_h - expected_h).abs() / expected_h;

                                let is_stale_fullscreen_size =
                                    width_error > 0.08 || height_error > 0.08;

                                if !is_stale_fullscreen_size {
                                    return (scale_x, scale_y);
                                }
                            }
                        }
                    }
                }
                return (scale, scale);
            }
        }
    }

    (scale, scale)
}

impl OutputScaleFactor {
    pub(super) fn get(&self) -> f64 {
        match *self {
            Self::Output(o) => o as _,
            Self::Fractional(f) => f,
        }
    }
}

#[must_use]
pub(super) fn update_output_scale(
    mut output: hecs::QueryOne<(&mut OutputScaleFactor, &mut TrueOutputScaleFactor)>,
    factor: OutputScaleFactor,
    compositor_scaling: bool,
    global_scale: f64,
) -> bool {
    let Some((output_scale, true_output_scale)) = output.get() else {
        return false;
    };

    if matches!(true_output_scale.0, OutputScaleFactor::Fractional(..))
        && matches!(factor, OutputScaleFactor::Output(..))
    {
        return false;
    }

    let mut changed = false;
    if true_output_scale.0 != factor {
        true_output_scale.0 = factor;
        changed = true;
    }

    let effective = if compositor_scaling {
        OutputScaleFactor::Fractional(global_scale)
    } else {
        factor
    };

    if *output_scale != effective {
        *output_scale = effective;
        changed = true;
    }

    changed
}

pub(super) enum OutputDimensionsSource {
    // The data in this variant is the values needed for the wl_output.geometry event.
    Wl {
        physical_width: i32,
        physical_height: i32,
        subpixel: WEnum<client::wl_output::Subpixel>,
        make: String,
        model: String,
        transform: WEnum<client::wl_output::Transform>,
    },
    Xdg,
}

pub(super) struct OutputDimensions {
    pub source: OutputDimensionsSource,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub refresh: i32,
    pub mode_flags: WEnum<client::wl_output::Mode>,
    pub rotated_90: bool,
    pub logical_width: Option<i32>,
    pub logical_height: Option<i32>,
}

impl Default for OutputDimensions {
    fn default() -> Self {
        Self {
            source: OutputDimensionsSource::Wl {
                physical_height: 0,
                physical_width: 0,
                subpixel: WEnum::Value(client::wl_output::Subpixel::Unknown),
                make: "<unknown>".to_string(),
                model: "<unknown>".to_string(),
                transform: WEnum::Value(client::wl_output::Transform::Normal),
            },
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            refresh: 60000,
            mode_flags: WEnum::Value(client::wl_output::Mode::Current),
            rotated_90: false,
            logical_width: None,
            logical_height: None,
        }
    }
}

/// Point a window's X11 position at `output_entity`, resetting the X11
/// primary output if the window is focused. Returns false if the output's
/// dimensions aren't known yet (no anchoring possible).
fn anchor_window_to_output<C: XConnection>(
    world: &World,
    window: x::Window,
    win_data: &mut WindowData,
    output_entity: Entity,
    global_output_offset: &GlobalOutputOffset,
    last_focused_toplevel: Option<x::Window>,
    connection: &mut C,
) -> bool {
    let Ok(dimensions) = world.get::<&OutputDimensions>(output_entity) else {
        return false;
    };
    // Prefer the compositor-scaling X11 position when present; fall back to
    // the raw dimensions-derived offset otherwise.
    let (ox, oy) = if let Ok(pos) = world.get::<&X11OutputPosition>(output_entity) {
        (pos.x, pos.y)
    } else {
        (
            dimensions.x - global_output_offset.x.value,
            dimensions.y - global_output_offset.y.value,
        )
    };
    win_data.update_output_offset(
        window,
        WindowOutputOffset { x: ox, y: oy },
        connection,
    );
    if last_focused_toplevel == Some(window) {
        debug!("focused window changed outputs - resetting primary output");
        let name = get_output_name(Some(&OnOutput(output_entity)), world);
        connection.focus_window(window, name);
    }
    true
}

fn update_output_offset(
    output: Entity,
    source: OutputDimensionsSource,
    x: i32,
    y: i32,
    state: &mut ServerState<impl XConnection>,
) {
    let connection = &mut state.connection;
    let state = &mut state.inner;
    {
        let Ok(mut dimensions) = state.world.get::<&mut OutputDimensions>(output) else {
            return;
        };
        if matches!(source, OutputDimensionsSource::Wl { .. })
            && matches!(dimensions.source, OutputDimensionsSource::Xdg)
        {
            return;
        }

        let global_offset = &mut state.global_output_offset;
        let mut maybe_update_dimension = |value, dim: &mut GlobalOutputOffsetDimension| {
            if value < dim.value {
                *dim = GlobalOutputOffsetDimension {
                    owner: Some(output),
                    value,
                };
                state.global_offset_updated = true;
            } else if dim.owner == Some(output) && value > dim.value {
                // Another output's position could be less than the new value, so recalculate
                dim.owner = None;
                state.global_offset_updated = true;
            }
        };

        maybe_update_dimension(x, &mut global_offset.x);
        maybe_update_dimension(y, &mut global_offset.y);

        dimensions.source = source;
        dimensions.x = x;
        dimensions.y = y;
        debug!(
            "moving {} to {x}x{y}",
            state.world.get::<&WlOutput>(output).unwrap().id()
        );
    }

    recalculate_x11_output_positions(
        &mut state.world,
        state.compositor_scaling,
        state.global_scale,
    );
    let outputs: Vec<hecs::Entity> = state
        .world
        .query::<&OutputDimensions>()
        .iter()
        .map(|(e, _)| e)
        .collect();
    for out in outputs {
        update_window_output_offsets(out, &state.global_output_offset, &state.world, connection);
    }
}

pub(super) fn update_window_output_offsets(
    output: Entity,
    global_output_offset: &GlobalOutputOffset,
    world: &World,
    connection: &mut impl XConnection,
) {
    let Ok(dimensions) = world.get::<&OutputDimensions>(output) else {
        return;
    };
    let (ox, oy) = if let Ok(pos) = world.get::<&X11OutputPosition>(output) {
        (pos.x, pos.y)
    } else {
        (
            dimensions.x - global_output_offset.x.value,
            dimensions.y - global_output_offset.y.value,
        )
    };
    let new_offset = WindowOutputOffset { x: ox, y: oy };

    let mut updated_windows: Vec<x::Window> = vec![];
    let mut query = world.query::<(&x::Window, &mut WindowData, &OnOutput)>();
    for (_, (window, data, _)) in query
        .into_iter()
        .filter(|(_, (_, _, on_output))| on_output.0 == output)
    {
        data.update_output_offset(*window, new_offset, connection);
        updated_windows.push(*window);
    }
    drop(query);

    // Transient and popup windows don't carry OnOutput, so copy the parent offset
    // before the next configure uses it for positioner math.
    if !updated_windows.is_empty() {
        let mut popup_query =
            world.query::<(&x::Window, &mut WindowData, &mut SurfaceScaleFactor)>();
        for (_, (window, data, scale)) in popup_query.into_iter().filter(|(_, (_, data, _))| {
            data.attrs.role.is_popup()
                && data
                    .attrs
                    .transient_for
                    .map(|p| updated_windows.contains(&p))
                    .unwrap_or(false)
        }) {
            data.update_output_offset(*window, new_offset, connection);
            if let Ok(out_scale) = world.get::<&OutputScaleFactor>(output) {
                scale.0 = out_scale.get();
            }
        }
    }
}

impl OutputDimensions {
    pub fn get_true_scale(&self) -> f64 {
        if let (Some(lw), Some(lh)) = (self.logical_width, self.logical_height) {
            if lw > 0 && lh > 0 {
                let physical_width = if self.rotated_90 {
                    self.height
                } else {
                    self.width
                };
                return physical_width as f64 / lw as f64;
            }
        }
        1.0
    }
}

pub(super) fn guess_initial_scale(
    world: &World,
    dims: WindowDims,
    default_scale: f64,
    compositor_scaling: bool,
) -> f64 {
    if dims.width == 0 || dims.height == 0 {
        return default_scale;
    }

    let cx = dims.x as f64 + dims.width as f64 / 2.0;
    let cy = dims.y as f64 + dims.height as f64 / 2.0;

    let mut best_scale = None;
    let mut max_scale = 1.0;

    for (entity, (dimensions, scale_factor, true_scale)) in world
        .query::<(
            &OutputDimensions,
            &OutputScaleFactor,
            Option<&TrueOutputScaleFactor>,
        )>()
        .iter()
    {
        let s = if compositor_scaling {
            let ts = true_scale
                .map(|ts| ts.0.get())
                .unwrap_or_else(|| dimensions.get_true_scale());
            if ts > 0.0 { ts } else { scale_factor.get() }
        } else {
            scale_factor.get()
        };

        if s > max_scale {
            max_scale = s;
        }

        let (ox, oy) = if compositor_scaling {
            if let Ok(pos) = world.get::<&X11OutputPosition>(entity) {
                (pos.x as f64, pos.y as f64)
            } else {
                (dimensions.x as f64, dimensions.y as f64)
            }
        } else {
            (dimensions.x as f64, dimensions.y as f64)
        };

        let (ow, oh) = if dimensions.rotated_90 {
            (dimensions.height as f64, dimensions.width as f64)
        } else {
            (dimensions.width as f64, dimensions.height as f64)
        };

        if cx >= ox && cx < ox + ow && cy >= oy && cy < oy + oh {
            best_scale = Some(s);
            break;
        }
    }

    best_scale.unwrap_or(if max_scale > 1.0 {
        max_scale
    } else {
        default_scale
    })
}

pub(super) fn update_global_output_offset(
    output: Entity,
    global_output_offset: &GlobalOutputOffset,
    world: &World,
    connection: &mut impl XConnection,
    global_scale: f64,
    compositor_scaling: bool,
) {
    let entity = world.entity(output).unwrap();
    let mut query = entity.query::<(&OutputDimensions, &WlOutput)>();
    let Some((dimensions, server)) = query.get() else {
        return;
    };

    let x = dimensions.x - global_output_offset.x.value;
    let y = dimensions.y - global_output_offset.y.value;
    let (scaled_x, scaled_y) = if compositor_scaling {
        let pos = world.get::<&X11OutputPosition>(output).ok();
        pos.map(|p| (p.x, p.y)).unwrap_or_else(|| {
            (
                scale_pos_to_physical(x as f64, global_scale),
                scale_pos_to_physical(y as f64, global_scale),
            )
        })
    } else {
        (x, y)
    };

    match &dimensions.source {
        OutputDimensionsSource::Wl {
            physical_width,
            physical_height,
            subpixel,
            make,
            model,
            transform,
        } => {
            let (pw, ph) = (*physical_width, *physical_height);
            server.geometry(
                scaled_x,
                scaled_y,
                pw,
                ph,
                convert_wenum(*subpixel),
                make.clone(),
                model.clone(),
                convert_wenum(*transform),
            );
        }
        OutputDimensionsSource::Xdg => {
            entity
                .get::<&XdgOutputServer>()
                .unwrap()
                .logical_position(scaled_x, scaled_y);
        }
    }

    server.done();
    drop(query);

    update_window_output_offsets(output, global_output_offset, world, connection);
}

#[derive(Debug)]
pub enum OutputEvent {
    Wl(client::wl_output::Event),
    Xdg(zxdg_output_v1::Event),
}

impl From<client::wl_output::Event> for ObjectEvent {
    fn from(value: client::wl_output::Event) -> Self {
        Self::Output(OutputEvent::Wl(value))
    }
}

impl From<zxdg_output_v1::Event> for ObjectEvent {
    fn from(value: zxdg_output_v1::Event) -> Self {
        Self::Output(OutputEvent::Xdg(value))
    }
}

impl Event for OutputEvent {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            OutputEvent::Xdg(event) => Self::xdg_event(event, target, state),
            OutputEvent::Wl(event) => Self::wl_event(event, target, state),
        }
    }
}

impl OutputEvent {
    fn wl_event<C: XConnection>(
        event: client::wl_output::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        use client::wl_output::Event;
        match event {
            Event::Geometry {
                x,
                y,
                physical_width,
                physical_height,
                subpixel,
                make,
                model,
                transform,
            } => {
                update_output_offset(
                    target,
                    OutputDimensionsSource::Wl {
                        physical_width,
                        physical_height,
                        subpixel,
                        make: make.clone(),
                        model: model.clone(),
                        transform,
                    },
                    x,
                    y,
                    state,
                );
                let global_output_offset = state.global_output_offset;
                let compositor_scaling = state.compositor_scaling;
                let global_scale = state.global_scale;

                let pos = if compositor_scaling {
                    state
                        .world
                        .get::<&X11OutputPosition>(target)
                        .ok()
                        .map(|p| *p)
                } else {
                    None
                };

                let Ok((output, dimensions, xdg)) = state.world.query_one_mut::<(
                    &WlOutput,
                    &mut OutputDimensions,
                    Option<&XdgOutputServer>,
                )>(target) else {
                    return;
                };

                let (scaled_x, scaled_y) = if compositor_scaling {
                    pos.map(|p| (p.x, p.y)).unwrap_or_else(|| {
                        (
                            ((x - global_output_offset.x.value) as f64 * global_scale).round()
                                as i32,
                            ((y - global_output_offset.y.value) as f64 * global_scale).round()
                                as i32,
                        )
                    })
                } else {
                    (
                        x - global_output_offset.x.value,
                        y - global_output_offset.y.value,
                    )
                };
                let (pw, ph) = (physical_width, physical_height);
                output.geometry(
                    scaled_x,
                    scaled_y,
                    pw,
                    ph,
                    convert_wenum(subpixel),
                    make,
                    model,
                    convert_wenum(transform),
                );
                dimensions.rotated_90 = transform.into_result().is_ok_and(|t| {
                    matches!(
                        t,
                        client::wl_output::Transform::_90
                            | client::wl_output::Transform::_270
                            | client::wl_output::Transform::Flipped90
                            | client::wl_output::Transform::Flipped270
                    )
                });
                if let Some(xdg) = xdg {
                    let (w, h) = if dimensions.rotated_90 {
                        (dimensions.height, dimensions.width)
                    } else {
                        (dimensions.width, dimensions.height)
                    };
                    let (sw, sh) = if compositor_scaling {
                        (
                            scale_size_to_physical(w as f64, global_scale),
                            scale_size_to_physical(h as f64, global_scale),
                        )
                    } else {
                        (w, h)
                    };
                    xdg.logical_size(sw, sh);
                }
            }
            Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                let compositor_scaling = state.compositor_scaling;
                let global_scale = state.global_scale;
                let Ok((output, dimensions)) = state
                    .world
                    .query_one_mut::<(&WlOutput, &mut OutputDimensions)>(target)
                else {
                    return;
                };

                if flags
                    .into_result()
                    .is_ok_and(|f| f.contains(client::wl_output::Mode::Current))
                {
                    dimensions.width = width;
                    dimensions.height = height;
                    dimensions.refresh = refresh;
                    dimensions.mode_flags = convert_wenum(flags);
                    debug!("{} dimensions: {width}x{height}", output.id());
                }
                let (w, h) = if compositor_scaling {
                    (
                        scale_size_to_physical(width as f64, global_scale),
                        scale_size_to_physical(height as f64, global_scale),
                    )
                } else {
                    (width, height)
                };
                output.mode(convert_wenum(flags), w, h, refresh);
            }
            Event::Scale { factor } => {
                debug!(
                    "{} scale: {factor}",
                    state.world.get::<&WlOutput>(target).unwrap().id()
                );
                if update_output_scale(
                    state.world.query_one(target).unwrap(),
                    OutputScaleFactor::Output(factor),
                    state.compositor_scaling,
                    state.global_scale,
                ) {
                    state.updated_outputs.push(target);
                }
                if state.fractional_scale.is_none() {
                    let send_factor = if state.compositor_scaling { 1 } else { factor };
                    state
                        .world
                        .get::<&WlOutput>(target)
                        .unwrap()
                        .scale(send_factor);
                }
            }
            Event::Name { name } => {
                state
                    .world
                    .get::<&WlOutput>(target)
                    .unwrap()
                    .name(name.clone());
                state.world.insert(target, (OutputName(name),)).unwrap();
            }
            _ => simple_event_shunt! {
                state.world.get::<&WlOutput>(target).unwrap(),
                event: client::wl_output::Event => [
                    Description { description },
                    Done
                ]
            },
        }
    }

    fn xdg_event<C: XConnection>(
        event: zxdg_output_v1::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        use zxdg_output_v1::Event;

        match event {
            Event::LogicalPosition { x, y } => {
                update_output_offset(target, OutputDimensionsSource::Xdg, x, y, state);
                if !state.global_offset_updated {
                    let compositor_scaling = state.inner.compositor_scaling;
                    let global_scale = state.inner.global_scale;
                    let global_output_offset = state.inner.global_output_offset;

                    let (scaled_x, scaled_y) = if compositor_scaling {
                        let pos = state.world.get::<&X11OutputPosition>(target).ok();
                        pos.map(|p| (p.x, p.y)).unwrap_or_else(|| {
                            (
                                scale_pos_to_physical(
                                    (x - global_output_offset.x.value) as f64,
                                    global_scale,
                                ),
                                scale_pos_to_physical(
                                    (y - global_output_offset.y.value) as f64,
                                    global_scale,
                                ),
                            )
                        })
                    } else {
                        (
                            x - state.inner.global_output_offset.x.value,
                            y - state.inner.global_output_offset.y.value,
                        )
                    };
                    state
                        .world
                        .get::<&XdgOutputServer>(target)
                        .unwrap()
                        .logical_position(scaled_x, scaled_y);
                }
            }
            Event::LogicalSize { width, height } => {
                let compositor_scaling = state.inner.compositor_scaling;
                let global_scale = state.inner.global_scale;
                let Ok((xdg, dimensions)) = state
                    .world
                    .query_one_mut::<(&XdgOutputServer, &mut OutputDimensions)>(target)
                else {
                    return;
                };
                dimensions.logical_width = Some(width);
                dimensions.logical_height = Some(height);

                let (sw, sh) = if compositor_scaling {
                    (
                        scale_size_to_physical(width as f64, global_scale),
                        scale_size_to_physical(height as f64, global_scale),
                    )
                } else {
                    (width, height)
                };
                xdg.logical_size(sw, sh);
            }
            _ => simple_event_shunt! {
                state.world.get::<&XdgOutputServer>(target).unwrap(),
                event: zxdg_output_v1::Event => [
                    Done,
                    Name { name },
                    Description { description }
                ]
            },
        }
    }
}

impl Event for wl_drm::client::wl_drm::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&WlDrmServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Device { name },
                Format { format },
                Authenticated,
                Capabilities { value }
            ]
        }
    }
}

impl Event for c_dmabuf::zwp_linux_dmabuf_feedback_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state
            .world
            .get::<&s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1>(target)
            .unwrap();
        simple_event_shunt! {
            server, self => [
                Done,
                FormatTable { |fd| fd.as_fd(), size },
                MainDevice { device },
                TrancheDone,
                TrancheTargetDevice { device },
                TrancheFormats { indices },
                TrancheFlags { |flags| convert_wenum(flags) }
            ]
        }
    }
}

impl Event for zwp_relative_pointer_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&RelativePointerServer>(target).unwrap();
        // Relative motion is already a pointer delta. Scaling it again breaks
        // locked-pointer clients and games that consume the deltas directly.
        simple_event_shunt! {
            server, self => [
                RelativeMotion {
                    utime_hi,
                    utime_lo,
                    dx,
                    dy,
                    dx_unaccel,
                    dy_unaccel
                }
            ]
        }
    }
}

impl Event for zwp_locked_pointer_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&LockedPointerServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Locked,
                Unlocked
            ]
        }
    }
}

impl Event for zwp_confined_pointer_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&ConfinedPointerServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Confined,
                Unconfined
            ]
        }
    }
}

impl Event for zwp_tablet_seat_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let seat = state.world.get::<&TabletSeatServer>(target).unwrap();
        match self {
            Self::TabletAdded { id } => {
                let (e, tab) = from_client::<TabletServer, _, _>(&id, state);
                seat.tablet_added(&tab);
                drop(seat);
                state.world.spawn_at(e, (tab, id));
            }
            Self::ToolAdded { id } => {
                let (e, tool) = from_client::<TabletToolServer, _, _>(&id, state);
                seat.tool_added(&tool);
                drop(seat);
                state.world.spawn_at(e, (tool, id));
            }
            Self::PadAdded { id } => {
                let (e, pad) = from_client::<TabletPadServer, _, _>(&id, state);
                seat.pad_added(&pad);
                drop(seat);
                state.world.spawn_at(e, (pad, id));
            }
            _ => log::warn!("unhandled {}: {self:?}", std::any::type_name::<Self>()),
        }
    }
}

impl Event for zwp_tablet_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let tab = state.world.get::<&TabletServer>(target).unwrap();
        simple_event_shunt! {
            tab, self => [
                Name { name },
                Id { vid, pid },
                Path { path },
                Done,
                Removed
            ]
        }
    }
}

impl Event for zwp_tablet_pad_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let pad = state.world.get::<&TabletPadServer>(target).unwrap();
        let s_surf;
        match self {
            Self::Group { pad_group } => {
                let (e, s_group) = from_client::<TabletPadGroupServer, _, _>(&pad_group, state);
                pad.group(&s_group);
                drop(pad);
                state.world.spawn_at(e, (pad_group, s_group));
            }
            Self::Enter {
                serial,
                tablet,
                surface,
            } => {
                let Some(surface) = surface
                    .data()
                    .copied()
                    .and_then(|key| state.world.get::<&WlSurface>(key).ok())
                else {
                    return;
                };
                let Some(s_tablet) =
                    tablet
                        .data()
                        .and_then(|key: &LateInitObjectKey<TabletClient>| {
                            state.world.get::<&TabletServer>(key.get()).ok()
                        })
                else {
                    return;
                };
                pad.enter(serial, &s_tablet, &surface);
            }
            _ => simple_event_shunt! {
                pad, self => [
                    Path { path },
                    Buttons { buttons },
                    Done,
                    Button {
                        time,
                        button,
                        |state| convert_wenum(state)
                    },
                    Leave {
                        serial,
                        |surface| {
                            s_surf = surface.data().copied().and_then(|key| state.world.get::<&WlSurface>(key).ok());
                            if let Some(s) = &s_surf { s } else { return; }
                        }
                    },
                    Removed
                ]
            },
        }
    }
}

impl Event for zwp_tablet_tool_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            Self::ProximityIn {
                serial,
                tablet,
                surface,
            } => {
                let mut cmd = CommandBuffer::new();
                {
                    let connection = &mut state.connection;
                    let world = &mut state.inner.world;

                    let Some(mut query) = surface.data().copied().and_then(|key| {
                        world
                            .query_one::<(&WlSurface, &SurfaceScaleFactor, &x::Window)>(key)
                            .ok()
                    }) else {
                        warn!("tablet tool proximity_in failed: stale surface");
                        return;
                    };
                    let s_entity = surface.data().copied().unwrap();
                    let (surface, _, window) = query.get().unwrap();
                    cmd.insert(target, (CurrentSurface::Xwayland(s_entity),));

                    let Some(s_tablet) =
                        tablet
                            .data()
                            .and_then(|key: &LateInitObjectKey<TabletClient>| {
                                world.get::<&TabletServer>(key.get()).ok()
                            })
                    else {
                        warn!("tablet tool proximity_in failed: stale tablet");
                        return;
                    };

                    world
                        .get::<&TabletToolServer>(target)
                        .unwrap()
                        .proximity_in(serial, &s_tablet, surface);

                    connection.raise_to_top(*window);
                }
                cmd.run_on(&mut state.world);
            }
            Self::Motion { x, y } => {
                let scales = state
                    .world
                    .get::<&CurrentSurface>(target)
                    .ok()
                    .and_then(|surf| match &*surf {
                        CurrentSurface::Xwayland(e) => {
                            Some(get_surface_input_scales(&state.world, *e))
                        }
                        _ => None,
                    })
                    .unwrap_or((1.0, 1.0));
                let tool = state.world.get::<&TabletToolServer>(target).unwrap();
                tool.motion(x * scales.0, y * scales.1);
            }
            Self::ProximityOut => {
                let mut cmd = CommandBuffer::new();
                cmd.remove_one::<CurrentSurface>(target);
                {
                    let tool = state.world.get::<&TabletToolServer>(target).unwrap();
                    tool.proximity_out();
                }
                cmd.run_on(&mut state.world);
            }
            other => {
                let tool = state.world.get::<&TabletToolServer>(target).unwrap();
                simple_event_shunt! {
                    tool, other: zwp_tablet_tool_v2::Event => [
                        Type { |tool_type| convert_wenum(tool_type) },
                        HardwareSerial { hardware_serial_hi, hardware_serial_lo },
                        HardwareIdWacom { hardware_id_hi, hardware_id_lo },
                        Capability { |capability| convert_wenum(capability) },
                        Done,
                        Removed,
                        Down { serial },
                        Up,
                        Distance { distance },
                        Pressure { pressure },
                        Tilt { tilt_x, tilt_y },
                        Rotation { degrees },
                        Slider { position },
                        Wheel { degrees, clicks },
                        Button { serial, button, |state| convert_wenum(state) },
                        Frame { time },
                    ]
                }
            }
        }
    }
}

#[must_use]
fn from_client<
    Server: Resource + 'static,
    Client: Proxy + Send + Sync + 'static,
    S: X11Selection + 'static,
>(
    client: &Client,
    state: &InnerServerState<S>,
) -> (Entity, Server)
where
    Client::Event: Send + Into<ObjectEvent>,
    InnerServerState<S>: wayland_server::Dispatch<Server, Entity>,
{
    let entity = state.world.reserve_entity();
    let server = state
        .client
        .create_resource::<_, _, InnerServerState<S>>(&state.dh, 1, entity)
        .unwrap();
    let obj_key: &LateInitObjectKey<Client> = client.data().unwrap();
    obj_key.init(entity);
    (entity, server)
}

impl Event for zwp_tablet_pad_group_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let group = state.world.get::<&TabletPadGroupServer>(target).unwrap();
        match self {
            Self::Buttons { buttons } => group.buttons(buttons),
            Self::Ring { ring } => {
                let (e, s_ring) = from_client::<TabletPadRingServer, _, _>(&ring, state);
                group.ring(&s_ring);
                drop(group);
                state.world.spawn_at(e, (s_ring, ring));
            }
            Self::Strip { strip } => {
                let (e, s_strip) = from_client::<TabletPadStripServer, _, _>(&strip, state);
                group.strip(&s_strip);
                drop(group);
                state.world.spawn_at(e, (s_strip, strip));
            }
            Self::Modes { modes } => group.modes(modes),
            Self::ModeSwitch { time, serial, mode } => group.mode_switch(time, serial, mode),
            Self::Done => group.done(),
            _ => log::warn!(
                "unhandled {} event: {self:?}",
                std::any::type_name::<Self>()
            ),
        }
    }
}

impl Event for zwp_tablet_pad_ring_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let ring = state.world.get::<&TabletPadRingServer>(target).unwrap();
        simple_event_shunt! {
            ring, self => [
                Source { |source| convert_wenum(source) },
                Angle { degrees },
                Stop,
                Frame { time }
            ]
        }
    }
}

impl Event for zwp_tablet_pad_strip_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let strip = state.world.get::<&TabletPadStripServer>(target).unwrap();
        simple_event_shunt! {
            strip, self => [
                Source { |source| convert_wenum(source) },
                Position { position },
                Stop,
                Frame { time }
            ]
        }
    }
}
