//! Window management.

use std::borrow::Cow;
use std::cell::{RefCell, RefMut};
use std::mem;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use smithay::backend::renderer::gles2::ffi::{self as gl, Gles2};
use smithay::backend::renderer::gles2::{Gles2Frame, Gles2Renderer};
use smithay::backend::renderer::Frame;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Logical, Physical, Point, Rectangle};
use smithay::wayland::compositor;
use smithay::wayland::shell::wlr_layer::{Layer, LayerSurface};
use smithay::wayland::shell::xdg::{PopupSurface, ToplevelSurface};

use crate::catacomb::{Catacomb, Damage};
use crate::drawing::{Graphics, MAX_DAMAGE_AGE};
use crate::geometry::SubtractRectFast;
use crate::input::{Gesture, TouchState};
use crate::layer::Layers;
use crate::orientation::Orientation;
use crate::output::{Canvas, Output, GESTURE_HANDLE_HEIGHT};
use crate::overview::{DragAction, DragAndDrop, Overview};
use crate::windows::layout::{LayoutPosition, Layouts};
use crate::windows::surface::{CatacombLayerSurface, OffsetSurface, Surface};
use crate::windows::window::Window;

pub mod layout;
pub mod surface;
pub mod window;

/// Maximum time before a transaction is cancelled.
const MAX_TRANSACTION_MILLIS: u64 = 1000;

/// Horizontal sensitivity of the application overview.
const OVERVIEW_HORIZONTAL_SENSITIVITY: f64 = 250.;

/// Relative size of gesture notch to the handle's whole width/height.
const GESTURE_NOTCH_PERCENTAGE: f64 = 0.2;

/// Gesture handle foreground color.
const GESTURE_HANDLE_NOTCH_COLOR: [f32; 4] = [1., 1., 1., 1.];

/// Gesture handle background color.
const GESTURE_HANDLE_COLOR: [f32; 4] = [0., 0., 0., 1.];

/// Global transaction timer in milliseconds.
static TRANSACTION_START: AtomicU64 = AtomicU64::new(0);

/// Start a new transaction.
///
/// This will reset the transaction start to the current system time if there's
/// no transaction pending, setting up the timeout for the transaction.
pub fn start_transaction() {
    // Skip when transaction is already active.
    if TRANSACTION_START.load(Ordering::Relaxed) != 0 {
        return;
    }

    let now = UNIX_EPOCH.elapsed().unwrap().as_millis() as u64;
    TRANSACTION_START.store(now, Ordering::Relaxed);
}

/// Check if there's an active transaction.
fn transaction_active() -> bool {
    TRANSACTION_START.load(Ordering::Relaxed) != 0
}

/// Container tracking all known clients.
#[derive(Debug)]
pub struct Windows {
    orphan_popups: Vec<Window<PopupSurface>>,
    layouts: Layouts,
    layers: Layers,
    view: View,

    event_loop: LoopHandle<'static, Catacomb>,
    activated: Option<ToplevelSurface>,
    transaction: Option<Transaction>,
    opaque_regions: OpaqueRegions,
    start_time: Instant,
    output: Output,

    /// Cached output state for rendering.
    ///
    /// This is used to tie the transactions to their respective output size and
    /// should be passed to anyone who doesn't communicate with clients about
    /// future updates, but instead tries to calculate things for the next
    /// rendered frame.
    canvas: Canvas,

    /// Orientation independent from [`Windows::orientation_locked`] state.
    unlocked_orientation: Orientation,
    orientation_locked: bool,

    /// Compositor damage beyond window-internal changes.
    fully_damaged: bool,
}

impl Windows {
    pub fn new(display: &DisplayHandle, event_loop: LoopHandle<'static, Catacomb>) -> Self {
        let output = Output::new_dummy(display);
        let canvas = *output.canvas();

        Self {
            event_loop,
            output,
            canvas,
            start_time: Instant::now(),
            orientation_locked: true,
            fully_damaged: true,
            unlocked_orientation: Default::default(),
            opaque_regions: Default::default(),
            orphan_popups: Default::default(),
            transaction: Default::default(),
            activated: Default::default(),
            layouts: Default::default(),
            layers: Default::default(),
            view: Default::default(),
        }
    }

    /// Add a new window.
    pub fn add(&mut self, surface: ToplevelSurface) {
        let window = Rc::new(RefCell::new(Window::new(surface)));
        self.layouts.create(&self.output, window);
    }

    /// Add a new layer shell window.
    pub fn add_layer(&mut self, layer: Layer, surface: impl Into<CatacombLayerSurface>) {
        let mut window = Window::new(surface.into());
        window.enter(&self.output);
        self.layers.add(layer, window);
    }

    /// Add a new popup window.
    pub fn add_popup(&mut self, popup: PopupSurface) {
        self.orphan_popups.push(Window::new(popup));
    }

    /// Find the XDG shell window responsible for a specific surface.
    pub fn find_xdg(&mut self, wl_surface: &WlSurface) -> Option<RefMut<Window>> {
        // Get root surface.
        let mut wl_surface = Cow::Borrowed(wl_surface);
        while let Some(surface) = compositor::get_parent(&wl_surface) {
            wl_surface = Cow::Owned(surface);
        }

        self.layouts.windows_mut().find(|window| window.surface().eq(wl_surface.as_ref()))
    }

    /// Handle a surface commit for any window.
    pub fn surface_commit(&mut self, surface: &WlSurface) {
        // Get the topmost surface for window comparison.
        let mut root_surface = Cow::Borrowed(surface);
        while let Some(parent) = compositor::get_parent(&root_surface) {
            root_surface = Cow::Owned(parent);
        }

        // Find a window matching the root surface.
        macro_rules! find_window {
            ($windows:expr) => {{
                $windows.find(|window| window.surface().eq(root_surface.as_ref()))
            }};
        }

        // Handle XDG surface commits.
        if let Some(mut window) = find_window!(self.layouts.windows_mut()) {
            window.surface_commit_common(surface, &self.output);
            return;
        }

        // Handle popup orphan adoption.
        self.orphan_surface_commit(&root_surface);

        // Apply popup surface commits.
        for mut window in self.layouts.windows_mut() {
            window.popup_surface_commit(&root_surface, surface, &self.output);
        }

        // Handle layer shell surface commits.
        let old_exclusive = *self.output.exclusive();
        if let Some(window) = find_window!(self.layers.iter_mut()) {
            window.surface_commit(surface, &mut self.output);
        }

        // Resize windows after exclusive zone change.
        if self.output.exclusive() != &old_exclusive {
            self.resize_all();
        }
    }

    /// Handle orphan popup surface commits.
    ///
    /// After the first surface commit, every popup should have a parent set.
    /// This function puts it at the correct location in the window tree
    /// below its parent.
    ///
    /// Popups will be dismissed if a surface commit is made for them without
    /// any parent set. They will also be dismissed if the parent is not
    /// currently visible.
    pub fn orphan_surface_commit(&mut self, root_surface: &WlSurface) -> Option<()> {
        let mut orphans = self.orphan_popups.iter();
        let index = orphans.position(|popup| popup.surface() == root_surface)?;
        let mut popup = self.orphan_popups.swap_remove(index);
        let parent = popup.parent()?;

        // Try and add it to the primary window.
        let active_layout = self.layouts.active();
        if let Some(primary) = active_layout.primary().as_ref() {
            popup = primary.borrow_mut().add_popup(popup, &parent)?;
        }

        // Try and add it to the secondary window.
        if let Some(secondary) = active_layout.secondary().as_ref() {
            popup = secondary.borrow_mut().add_popup(popup, &parent)?;
        }

        // Dismiss popup if it wasn't added to either of the visible windows.
        popup.surface.send_popup_done();

        Some(())
    }

    /// Import pending buffers for all windows.
    pub fn import_buffers(&mut self, renderer: &mut Gles2Renderer) {
        // Do not import buffers during a transaction.
        if transaction_active() {
            return;
        }

        for mut window in self.layouts.windows_mut() {
            window.import_buffers(renderer);
        }

        for window in self.layers.iter_mut() {
            window.import_buffers(renderer);
        }
    }

    /// Draw the current window state.
    pub fn draw(
        &mut self,
        frame: &mut Gles2Frame,
        graphics: &Graphics,
        damage: &mut Damage,
        buffer_age: u8,
    ) {
        // Reset global damage.
        let fully_damaged = mem::take(&mut self.fully_damaged);

        // Collect pending damage.
        let max_age = MAX_DAMAGE_AGE as u8;
        let damage = if buffer_age == 0
            || buffer_age > max_age
            || fully_damaged
            || !matches!(self.view, View::Workspace)
        {
            let resolution = self.output.size().to_physical(self.output.scale());
            damage.push(Rectangle::from_loc_and_size((0, 0), resolution));
            damage.take_since(1)
        } else {
            self.window_damage(damage);
            damage.take_since(buffer_age)
        };

        // Update the opaque regions.
        let workspace_active = matches!(self.view, View::Workspace);
        self.opaque_regions.update(&self.layouts, &self.layers, &self.canvas, workspace_active);

        // Clear the screen.
        let clear_damage = self.opaque_regions.filter_damage(damage);
        if !clear_damage.is_empty() {
            let _ = frame.clear([1., 0., 1., 1.], &clear_damage);
        }

        self.layers.draw_background(frame, &self.canvas, damage, &mut self.opaque_regions);

        match self.view {
            View::Workspace => {
                self.layouts.with_visible_mut(|window| {
                    window.draw(frame, &self.canvas, 1., None, damage, &mut self.opaque_regions);
                });
            },
            View::DragAndDrop(ref dnd) => {
                self.layouts.with_visible_mut(|window| {
                    window.draw(frame, &self.canvas, 1., None, damage, None);
                });
                dnd.draw(frame, &self.canvas, graphics);
            },
            View::Overview(ref mut overview) => {
                overview.draw(frame, &self.output, &self.canvas, &self.layouts);

                // Stage immediate redraw while overview animations are active.
                if overview.animating_drag(self.layouts.len()) {
                    self.fully_damaged = true;
                }
            },
        }

        // Only draw top/overlay windows in workspace view.
        if workspace_active {
            self.layers.draw_foreground(frame, &self.canvas, damage, &mut self.opaque_regions);
        }

        // Draw gesture handle in workspace view.
        let _ = frame.with_context(|gl| unsafe {
            self.draw_gesture_handle(gl, damage);
        });
    }

    /// Draw the gesture handle.
    unsafe fn draw_gesture_handle(&self, gl: &Gles2, damage: &[Rectangle<i32, Physical>]) {
        let handle_height = GESTURE_HANDLE_HEIGHT * self.canvas.scale();
        let output_size = self.canvas.physical_resolution();

        // Calculate handle rectangle.
        let (handle_loc, handle_size) = match self.orientation() {
            Orientation::Portrait => {
                ((0, output_size.h - handle_height), (output_size.w, handle_height))
            },
            Orientation::InversePortrait => ((0, 0), (output_size.w, handle_height)),
            Orientation::Landscape => ((0, 0), (handle_height, output_size.h)),
            Orientation::InverseLandscape => {
                ((output_size.w - handle_height, 0), (handle_height, output_size.h))
            },
        };
        let handle_rect = Rectangle::from_loc_and_size(handle_loc, handle_size);

        // Skip rendering without damage.
        if damage.iter().all(|damage| !damage.overlaps(handle_rect)) {
            return;
        }

        gl.Enable(gl::SCISSOR_TEST);

        // Draw Background.
        gl.Scissor(handle_rect.loc.x, handle_rect.loc.y, handle_rect.size.w, handle_rect.size.h);
        gl.ClearColor(
            GESTURE_HANDLE_COLOR[0],
            GESTURE_HANDLE_COLOR[1],
            GESTURE_HANDLE_COLOR[2],
            GESTURE_HANDLE_COLOR[3],
        );
        gl.Clear(gl::COLOR_BUFFER_BIT);

        // Draw handle notch.
        let notch_height = (handle_rect.size.h as f64 * GESTURE_NOTCH_PERCENTAGE) as i32;
        let notch_width = (handle_rect.size.w as f64 * GESTURE_NOTCH_PERCENTAGE) as i32;
        let notch_x = handle_rect.loc.x + (handle_rect.size.w - notch_width) / 2;
        let notch_y = handle_rect.loc.y + (handle_rect.size.h - notch_height) / 2;
        gl.Scissor(notch_x, notch_y, notch_width, notch_height);
        gl.ClearColor(
            GESTURE_HANDLE_NOTCH_COLOR[0],
            GESTURE_HANDLE_NOTCH_COLOR[1],
            GESTURE_HANDLE_NOTCH_COLOR[2],
            GESTURE_HANDLE_NOTCH_COLOR[3],
        );
        gl.Clear(gl::COLOR_BUFFER_BIT);

        gl.Disable(gl::SCISSOR_TEST);
    }

    /// Request new frames for all visible windows.
    pub fn request_frames(&mut self) {
        let runtime = self.runtime();
        self.layers.request_frames(runtime);

        if matches!(self.view, View::Workspace) {
            self.layouts.with_visible(|window| window.request_frame(runtime));
        }
    }

    /// Stage dead XDG shell window for reaping.
    pub fn reap_xdg(&mut self, surface: &ToplevelSurface) {
        self.layouts.reap(&self.output, surface);
    }

    /// Stage dead layer shell window for reaping.
    pub fn reap_layer(&mut self, surface: &LayerSurface) {
        // Start transaction to ensure window is reaped even without any resize.
        start_transaction();

        // Handle layer shell death.
        let old_exclusive = *self.output.exclusive();
        if let Some(window) = self.layers.iter().find(|layer| layer.surface.eq(surface)) {
            self.output.exclusive().reset(window.surface.anchor, window.surface.exclusive_zone);
        }

        // Resize windows if reserved layer space changed.
        if self.output.exclusive() != &old_exclusive {
            self.resize_all();
        }
    }

    /// Reap dead XDG popup windows.
    pub fn refresh_popups(&mut self) {
        for mut window in self.layouts.windows_mut() {
            window.refresh_popups();
        }
    }

    /// Start Overview window Drag & Drop.
    pub fn start_dnd(&mut self, layout_position: LayoutPosition) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            _ => return,
        };

        // Convert layout position to window.
        let window = match self.layouts.window(layout_position) {
            Some(window) => window.clone(),
            None => return,
        };

        let dnd = DragAndDrop::new(&self.output, overview, layout_position, window);
        self.view = View::DragAndDrop(dnd);
        self.fully_damaged = true;
    }

    /// Current window focus.
    pub fn focus(&mut self) -> Option<WlSurface> {
        let surface = match self.layouts.focus.as_ref().map(Weak::upgrade) {
            // Use focused surface if the window is still alive.
            Some(Some(window)) => Some(window.borrow().surface.clone()),
            // Fallback to primary if secondary perished.
            Some(None) => {
                let primary = self.layouts.active().primary();
                let surface = primary.map(|window| window.borrow().surface.clone());
                self.layouts.focus = primary.map(Rc::downgrade);
                surface
            },
            // Do not upgrade if toplevel is explicitly unfocused.
            None => None,
        };

        // Update window activation state.
        if self.activated != surface {
            // Clear old activated flag.
            if let Some(activated) = self.activated.take() {
                activated.set_state(|state| {
                    state.states.unset(State::Activated);
                });
            }

            // Set new activated flag.
            if let Some(surface) = &surface {
                surface.set_state(|state| {
                    state.states.set(State::Activated);
                });
            }
            self.activated = surface.clone();
        }

        surface.map(|surface| surface.surface().clone())
            // Check for layer-shell window focus.
            .or_else(|| self.layers.focus.clone())
    }

    /// Clear all window focus.
    fn clear_focus(&mut self) {
        self.layouts.focus = None;
        self.layers.focus = None;
    }

    /// Start a new transaction.
    fn start_transaction(&mut self) -> &mut Transaction {
        start_transaction();
        self.transaction.get_or_insert(Transaction::new())
    }

    /// Attempt to execute pending transactions.
    ///
    /// This will return the duration until the transaction should be timed out
    /// when there is an active transaction but it cannot be completed yet.
    pub fn update_transaction(&mut self) -> Option<Duration> {
        // Skip update if no transaction is active.
        let start = TRANSACTION_START.load(Ordering::Relaxed);
        if start == 0 {
            return None;
        }

        // Check if the transaction requires updating.
        let elapsed = UNIX_EPOCH.elapsed().unwrap().as_millis() as u64 - start;
        if elapsed <= MAX_TRANSACTION_MILLIS {
            // Check if all participants are ready.
            let finished = self.layouts.windows().all(|window| window.transaction_done())
                && self.layers.iter().all(Window::transaction_done);

            // Abort if the transaction is still pending.
            if !finished {
                let delta = MAX_TRANSACTION_MILLIS - elapsed;
                return Some(Duration::from_millis(delta));
            }
        }

        // Clear transaction timer.
        TRANSACTION_START.store(0, Ordering::Relaxed);

        // Apply layout/liveliness changes.
        self.layouts.apply_transaction(&self.output);

        // Update layer shell windows.
        self.layers.apply_transaction();

        // Apply window management changes.
        if let Some(view) = self.transaction.take().and_then(|transaction| transaction.view) {
            self.view = view;
        }
        self.canvas = *self.output.canvas();

        // Close overview if all layouts died.
        if self.layouts.is_empty() {
            self.view = View::Workspace;
        }

        self.fully_damaged = true;

        None
    }

    /// Resize all windows to their expected size.
    pub fn resize_all(&mut self) {
        // Resize XDG windows.
        self.layouts.resize_all(&self.output);

        // Resize layer shell windows.
        for window in self.layers.iter_mut() {
            window.update_dimensions(&mut self.output);
        }
    }

    /// Update output orientation.
    pub fn update_orientation(&mut self, orientation: Orientation) {
        self.unlocked_orientation = orientation;

        // Ignore orientation changes during orientation lock.
        if self.orientation_locked {
            return;
        }

        // Start transaction to ensure output transaction will be applied.
        start_transaction();

        // Update output orientation.
        self.output.set_orientation(orientation);

        // Resize all windows to new output size.
        self.resize_all();
    }

    /// Lock the output's orientation.
    pub fn lock_orientation(&mut self, orientation: Option<Orientation>) {
        // Change to the new locked orientation.
        if let Some(orientation) = orientation {
            self.update_orientation(orientation);
        }

        self.orientation_locked = true;
    }

    /// Unlock the output's orientation.
    pub fn unlock_orientation(&mut self) {
        self.orientation_locked = false;
        self.update_orientation(self.unlocked_orientation);
    }

    /// Get the current rendering orientation.
    pub fn orientation(&self) -> Orientation {
        self.canvas.orientation()
    }

    /// Check if any window was damaged since the last redraw.
    pub fn damaged(&mut self) -> bool {
        self.fully_damaged
            || self.layers.iter().any(Window::damaged)
            || self.layouts.windows().any(|window| window.damaged())
    }

    /// Window damage since last redraw.
    ///
    /// This function collects the damage for every window, without taking
    /// global damage into account. To avoid unnecessary work,
    /// [`Windows::fully_damaged`] should be called first.
    fn window_damage(&self, damage: &mut Damage) {
        let active_layout = self.layouts.active();
        let primary = active_layout.primary();
        let secondary = active_layout.secondary();

        let primary_damage = primary.and_then(|window| window.borrow().damage());
        let secondary_damage = secondary.and_then(|window| window.borrow().damage());
        let layer_damage = self.layers.iter().filter_map(Window::damage);

        for window_damage in layer_damage.chain(primary_damage).chain(secondary_damage) {
            damage.push(window_damage);
        }
    }

    /// Handle start of touch input.
    pub fn on_touch_start(&mut self, point: Point<f64, Logical>) {
        if let View::Overview(overview) = &mut self.view {
            // Hold on overview window stages it for D&D.
            if let Some(position) = overview.layout_position(&self.output, &self.layouts, point) {
                overview.start_hold(&self.event_loop, position);
            }

            overview.drag_action = DragAction::None;
            overview.last_drag_point = point;
            overview.y_offset = 0.;
        }
    }

    /// Hand quick touch input.
    pub fn on_tap(&mut self, point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::Workspace => {
                // Clear focus on gesture handle tap.
                if point.y >= (self.output.size().h - GESTURE_HANDLE_HEIGHT) as f64 {
                    self.clear_focus();
                }
                return;
            },
            View::DragAndDrop(_) => return,
        };

        overview.cancel_hold(&self.event_loop);

        // Click inside window opens it as new primary.
        if let Some(position) = overview.layout_position(&self.output, &self.layouts, point) {
            self.layouts.set_active(&self.output, Some(position.index));
        }

        // Return to workspace view.
        //
        // If the click was outside of the focused window, we just close out of the
        // Overview and return to the previous primary/secondary windows.
        self.set_view(View::Workspace);
    }

    /// Handle a touch drag.
    pub fn on_drag(&mut self, touch_state: &mut TouchState, mut point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::DragAndDrop(dnd) => {
                // Cancel velocity and clamp if touch position is outside the screen.
                let output_size = self.output.wm_size().to_f64();
                if point.x < 0.
                    || point.x > output_size.w
                    || point.y < 0.
                    || point.y > output_size.h
                {
                    point.x = point.x.clamp(0., output_size.w - 1.);
                    point.y = point.y.clamp(0., output_size.h - 1.);
                    touch_state.cancel_velocity();
                }

                let delta = point - mem::replace(&mut dnd.touch_position, point);
                dnd.window_position += delta;

                // Redraw when the D&D window is moved.
                self.fully_damaged = true;

                return;
            },
            View::Workspace => return,
        };

        let delta = point - mem::replace(&mut overview.last_drag_point, point);

        // Lock current drag direction if it hasn't been determined yet.
        if matches!(overview.drag_action, DragAction::None) {
            if delta.x.abs() < delta.y.abs() {
                overview.drag_action = overview
                    .layout_position(&self.output, &self.layouts, point)
                    .and_then(|position| self.layouts.window(position))
                    .map(|window| DragAction::Close(Rc::downgrade(window)))
                    .unwrap_or_default();
            } else {
                overview.drag_action = DragAction::Cycle;
            }
        }

        // Update drag action.
        match overview.drag_action {
            DragAction::Cycle => overview.x_offset += delta.x / OVERVIEW_HORIZONTAL_SENSITIVITY,
            DragAction::Close(_) => overview.y_offset += delta.y,
            DragAction::None => (),
        }

        // Cancel velocity once drag actions are completed.
        if overview.overdrag_limited(self.layouts.len()) {
            touch_state.cancel_velocity();
        }

        overview.last_animation_step = None;
        overview.cancel_hold(&self.event_loop);

        // Redraw when cycling through the overview.
        self.fully_damaged = true;
    }

    /// Handle touch drag release.
    pub fn on_drag_release(&mut self) {
        match &mut self.view {
            View::Overview(overview) => overview.last_animation_step = Some(Instant::now()),
            View::DragAndDrop(dnd) => {
                let (primary_bounds, secondary_bounds) = dnd.drop_bounds(&self.output);
                if primary_bounds.to_f64().contains(dnd.touch_position) {
                    if let Some(position) = self.layouts.position(&dnd.window) {
                        self.layouts.set_primary(&self.output, position);
                        self.set_view(View::Workspace);
                    }
                } else if secondary_bounds.to_f64().contains(dnd.touch_position) {
                    if let Some(position) = self.layouts.position(&dnd.window) {
                        self.layouts.set_secondary(&self.output, position);
                        self.set_view(View::Workspace);
                    }
                } else {
                    let overview = Overview::new(dnd.overview_x_offset);
                    self.set_view(View::Overview(overview));
                }
            },
            View::Workspace => (),
        }
    }

    /// Handle touch gestures.
    pub fn on_gesture(&mut self, gesture: Gesture) {
        match (gesture, &self.view) {
            (Gesture::Up, View::Overview(_)) => {
                self.layouts.set_active(&self.output, None);
                self.set_view(View::Workspace);
            },
            (Gesture::Up, _) if !self.layouts.is_empty() => {
                let overview = Overview::new(self.layouts.active_offset());
                self.set_view(View::Overview(overview));
            },
            (Gesture::Left, View::Workspace) => self.layouts.cycle_active(&self.output, 1),
            (Gesture::Right, View::Workspace) => self.layouts.cycle_active(&self.output, -1),
            (Gesture::Up | Gesture::Left | Gesture::Right, _) => (),
        }
    }

    /// Check which surface is at a specific touch point.
    ///
    /// If the window at the touch location accepts keyboard input, this
    /// function will also change focus to the root window associated with
    /// the touch surface.
    pub fn touch_surface_at(&mut self, position: Point<f64, Logical>) -> Option<OffsetSurface> {
        // Prevent window interaction in Overview/DnD.
        match self.view {
            View::Workspace => (),
            _ => return None,
        };

        // Search for topmost clicked surface.

        if let Some(window) = self.layers.foreground_window_at(position) {
            let surface = window.surface_at(position);

            if !window.deny_focus {
                self.layouts.focus = None;
                self.layers.focus = Some(window.surface().clone());
            }

            return surface;
        }

        let active_layout = self.layouts.active().clone();
        for window in active_layout.primary().iter().chain(&active_layout.secondary()) {
            let window_ref = window.borrow();
            if window_ref.contains(position) {
                self.layouts.focus = Some(Rc::downgrade(window));
                self.layers.focus = None;
                return window_ref.surface_at(position);
            }
        }

        if let Some(window) = self.layers.background_window_at(position) {
            let surface = window.surface_at(position);

            if !window.deny_focus {
                self.layouts.focus = None;
                self.layers.focus = Some(window.surface().clone());
            }

            return surface;
        }

        // Clear focus if touch wasn't on any surface.
        //
        // NOTE: We can't just always clear focus since a layer shell surface that
        // denies focus should still return the touched surface but not clear
        // the focus.
        self.clear_focus();

        None
    }

    /// Application runtime.
    pub fn runtime(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// Change the active view.
    fn set_view(&mut self, view: View) {
        self.start_transaction().view = Some(view);
    }

    /// Get immutable reference to the current output.
    pub fn output(&self) -> &Output {
        &self.output
    }

    /// Update the window manager's current output.
    pub fn set_output(&mut self, output: Output) {
        self.canvas = *output.canvas();
        self.output = output;
    }
}

/// Atomic changes to [`Windows`].
#[derive(Debug)]
struct Transaction {
    view: Option<View>,
}

impl Transaction {
    fn new() -> Self {
        Self { view: None }
    }
}

/// Compositor window arrangements.
#[derive(Debug)]
enum View {
    /// List of all open windows.
    Overview(Overview),
    /// Drag and drop for tiling windows.
    DragAndDrop(DragAndDrop),
    /// Currently active windows.
    Workspace,
}

impl Default for View {
    fn default() -> Self {
        View::Workspace
    }
}

/// List with all windows' opaque regions.
#[derive(Default, Debug)]
pub struct OpaqueRegions {
    opaque_regions: Vec<Rectangle<i32, Physical>>,

    /// Damage cache, to reduce allocations.
    ///
    /// This is only useful when the `OpaqueRegions` struct is also stored
    /// somewhere permanently, otherwise it will still reallocate.
    damage_cache: Vec<Rectangle<i32, Physical>>,
}

impl OpaqueRegions {
    /// Update the currently occluded regions.
    fn update(
        &mut self,
        layouts: &Layouts,
        layers: &Layers,
        canvas: &Canvas,
        workspace_active: bool,
    ) {
        self.opaque_regions.clear();

        for window in layers.background() {
            self.opaque_regions.extend_from_slice(window.opaque_region());
        }

        // Ignore layouts/foreground layer regions in overview and DnD.
        if workspace_active {
            layouts.with_visible(|window| {
                self.opaque_regions.extend_from_slice(window.opaque_region());
            });

            for window in layers.foreground() {
                self.opaque_regions.extend_from_slice(window.opaque_region());
            }
        }

        let canvas_size = canvas.physical_resolution();
        let handle_size = (canvas_size.w, GESTURE_HANDLE_HEIGHT * canvas.scale());
        let handle_loc = (0, canvas_size.h - handle_size.1);
        self.opaque_regions.push(Rectangle::from_loc_and_size(handle_loc, handle_size));
    }

    /// Filter out occluded damage rectangles.
    pub fn filter_damage(
        &mut self,
        damage: &[Rectangle<i32, Physical>],
    ) -> &[Rectangle<i32, Physical>] {
        self.damage_cache.clear();
        self.damage_cache.extend_from_slice(damage);

        for opaque_region in &self.opaque_regions {
            self.damage_cache.subtract_rect(*opaque_region);
        }

        &self.damage_cache
    }

    /// Pop `N` opaque region rectangles from the bottom of the stack.
    pub fn popn(&mut self, n: usize) {
        let original_len = self.opaque_regions.len();
        self.opaque_regions.rotate_left(n);
        self.opaque_regions.truncate(original_len.saturating_sub(n));
    }
}
