mod pane_resizer;
mod tiled_pane_grid;

use crate::tab::{Pane, MIN_TERMINAL_HEIGHT, MIN_TERMINAL_WIDTH};
use tiled_pane_grid::{split, TiledPaneGrid};

use crate::{
    os_input_output::ServerOsApi, output::Output, panes::PaneId, ui::boundaries::Boundaries,
    ui::pane_contents_and_ui::PaneContentsAndUi, ClientId,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;
use zellij_utils::errors::prelude::*;
use zellij_utils::{
    data::{ModeInfo, Style},
    input::command::RunCommand,
    input::layout::SplitDirection,
    pane_size::{Offset, PaneGeom, Size, SizeInPixels, Viewport},
};

macro_rules! resize_pty {
    ($pane:expr, $os_input:expr) => {
        if let PaneId::Terminal(ref pid) = $pane.pid() {
            // FIXME: This `set_terminal_size_using_terminal_id` call would be best in
            // `TerminalPane::reflow_lines`
            $os_input.set_terminal_size_using_terminal_id(
                *pid,
                $pane.get_content_columns() as u16,
                $pane.get_content_rows() as u16,
            );
        }
    };
}

fn pane_content_offset(position_and_size: &PaneGeom, viewport: &Viewport) -> (usize, usize) {
    // (columns_offset, rows_offset)
    // if the pane is not on the bottom or right edge on the screen, we need to reserve one space
    // from its content to leave room for the boundary between it and the next pane (if it doesn't
    // draw its own frame)
    let columns_offset = if position_and_size.x + position_and_size.cols.as_usize() < viewport.cols
    {
        1
    } else {
        0
    };
    let rows_offset = if position_and_size.y + position_and_size.rows.as_usize() < viewport.rows {
        1
    } else {
        0
    };
    (columns_offset, rows_offset)
}

pub struct TiledPanes {
    pub panes: BTreeMap<PaneId, Box<dyn Pane>>,
    display_area: Rc<RefCell<Size>>,
    viewport: Rc<RefCell<Viewport>>,
    connected_clients: Rc<RefCell<HashSet<ClientId>>>,
    connected_clients_in_app: Rc<RefCell<HashSet<ClientId>>>,
    mode_info: Rc<RefCell<HashMap<ClientId, ModeInfo>>>,
    character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
    default_mode_info: ModeInfo,
    style: Style,
    session_is_mirrored: bool,
    active_panes: HashMap<ClientId, PaneId>,
    draw_pane_frames: bool,
    panes_to_hide: HashSet<PaneId>,
    fullscreen_is_active: bool,
    os_api: Box<dyn ServerOsApi>,
}

impl TiledPanes {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        display_area: Rc<RefCell<Size>>,
        viewport: Rc<RefCell<Viewport>>,
        connected_clients: Rc<RefCell<HashSet<ClientId>>>,
        connected_clients_in_app: Rc<RefCell<HashSet<ClientId>>>,
        mode_info: Rc<RefCell<HashMap<ClientId, ModeInfo>>>,
        character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
        session_is_mirrored: bool,
        draw_pane_frames: bool,
        default_mode_info: ModeInfo,
        style: Style,
        os_api: Box<dyn ServerOsApi>,
    ) -> Self {
        TiledPanes {
            panes: BTreeMap::new(),
            display_area,
            viewport,
            connected_clients,
            connected_clients_in_app,
            mode_info,
            character_cell_size,
            default_mode_info,
            style,
            session_is_mirrored,
            active_panes: HashMap::new(),
            draw_pane_frames,
            panes_to_hide: HashSet::new(),
            fullscreen_is_active: false,
            os_api,
        }
    }
    pub fn add_pane_with_existing_geom(&mut self, pane_id: PaneId, mut pane: Box<dyn Pane>) {
        if self.draw_pane_frames {
            pane.set_content_offset(Offset::frame(1));
        }
        self.panes.insert(pane_id, pane);
    }
    pub fn replace_active_pane(
        &mut self,
        pane: Box<dyn Pane>,
        client_id: ClientId,
    ) -> Option<Box<dyn Pane>> {
        let pane_id = pane.pid();
        // remove the currently active pane
        let previously_active_pane = self
            .active_panes
            .get(&client_id)
            .copied()
            .and_then(|active_pane_id| self.replace_pane(active_pane_id, pane));

        // move clients from the previously active pane to the new pane we just inserted
        if let Some(previously_active_pane) = previously_active_pane.as_ref() {
            let previously_active_pane_id = previously_active_pane.pid();
            self.move_clients_between_panes(previously_active_pane_id, pane_id);
        }
        previously_active_pane
    }
    pub fn replace_pane(
        &mut self,
        pane_id: PaneId,
        mut with_pane: Box<dyn Pane>,
    ) -> Option<Box<dyn Pane>> {
        let with_pane_id = with_pane.pid();
        if self.draw_pane_frames {
            with_pane.set_content_offset(Offset::frame(1));
        }
        let removed_pane = self.panes.remove(&pane_id).map(|removed_pane| {
            let with_pane_id = with_pane.pid();
            let removed_pane_geom = removed_pane.position_and_size();
            let removed_pane_geom_override = removed_pane.geom_override();
            with_pane.set_geom(removed_pane_geom);
            match removed_pane_geom_override {
                Some(geom_override) => with_pane.set_geom_override(geom_override),
                None => with_pane.reset_size_and_position_override(),
            };
            self.panes.insert(with_pane_id, with_pane);
            removed_pane
        });

        // move clients from the previously active pane to the new pane we just inserted
        self.move_clients_between_panes(pane_id, with_pane_id);
        removed_pane
    }
    pub fn insert_pane(&mut self, pane_id: PaneId, mut pane: Box<dyn Pane>) {
        let cursor_height_width_ratio = self.cursor_height_width_ratio();
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let pane_id_and_split_direction =
            pane_grid.find_room_for_new_pane(cursor_height_width_ratio);
        if let Some((pane_id_to_split, split_direction)) = pane_id_and_split_direction {
            // this unwrap is safe because floating panes should not be visible if there are no floating panes
            let pane_to_split = self.panes.get_mut(&pane_id_to_split).unwrap();
            let size_of_both_panes = pane_to_split.position_and_size();
            if let Some((first_geom, second_geom)) = split(split_direction, &size_of_both_panes) {
                pane_to_split.set_geom(first_geom);
                pane.set_geom(second_geom);
                self.panes.insert(pane_id, pane);
                self.relayout(!split_direction);
            }
        }
    }
    pub fn has_room_for_new_pane(&mut self) -> bool {
        let cursor_height_width_ratio = self.cursor_height_width_ratio();
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        pane_grid
            .find_room_for_new_pane(cursor_height_width_ratio)
            .is_some()
    }
    pub fn fixed_pane_geoms(&self) -> Vec<Viewport> {
        self.panes
            .values()
            .filter_map(|p| {
                let geom = p.position_and_size();
                if geom.cols.is_fixed() || geom.rows.is_fixed() {
                    Some(geom.into())
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn first_selectable_pane_id(&self) -> Option<PaneId> {
        self.panes
            .iter()
            .filter(|(_id, pane)| pane.selectable())
            .map(|(id, _)| id.to_owned())
            .next()
    }
    pub fn pane_ids(&self) -> impl Iterator<Item = &PaneId> {
        self.panes.keys()
    }
    pub fn relayout(&mut self, direction: SplitDirection) {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let result = match direction {
            SplitDirection::Horizontal => {
                pane_grid.layout(direction, (*self.display_area.borrow()).cols)
            },
            SplitDirection::Vertical => {
                pane_grid.layout(direction, (*self.display_area.borrow()).rows)
            },
        };
        if let Err(e) = &result {
            log::error!("{:?} relayout of the tab failed: {}", direction, e);
        }
        self.set_pane_frames(self.draw_pane_frames);
    }
    pub fn set_pane_frames(&mut self, draw_pane_frames: bool) {
        self.draw_pane_frames = draw_pane_frames;
        let viewport = *self.viewport.borrow();
        for pane in self.panes.values_mut() {
            if !pane.borderless() {
                pane.set_frame(draw_pane_frames);
            }

            #[allow(clippy::if_same_then_else)]
            if draw_pane_frames & !pane.borderless() {
                // there's definitely a frame around this pane, offset its contents
                pane.set_content_offset(Offset::frame(1));
            } else if draw_pane_frames && pane.borderless() {
                // there's no frame around this pane, and the tab isn't handling the boundaries
                // between panes (they each draw their own frames as they please)
                // this one doesn't - do not offset its content
                pane.set_content_offset(Offset::default());
            } else if !is_inside_viewport(&viewport, pane) {
                // this pane is outside the viewport and has no border - it should not have an offset
                pane.set_content_offset(Offset::default());
            } else {
                // no draw_pane_frames and this pane should have a separation to other panes
                // according to its position in the viewport (eg. no separation if its at the
                // viewport bottom) - offset its content accordingly
                let position_and_size = pane.current_geom();
                let (pane_columns_offset, pane_rows_offset) =
                    pane_content_offset(&position_and_size, &viewport);
                pane.set_content_offset(Offset::shift(pane_rows_offset, pane_columns_offset));
            }

            resize_pty!(pane, self.os_api);
        }
    }
    pub fn can_split_pane_horizontally(&mut self, client_id: ClientId) -> bool {
        if let Some(active_pane_id) = &self.active_panes.get(&client_id) {
            if let Some(active_pane) = self.panes.get_mut(active_pane_id) {
                let full_pane_size = active_pane.position_and_size();
                if full_pane_size.rows.as_usize() < MIN_TERMINAL_HEIGHT * 2 {
                    return false;
                } else {
                    return split(SplitDirection::Horizontal, &full_pane_size).is_some();
                }
            }
        }
        false
    }
    pub fn can_split_pane_vertically(&mut self, client_id: ClientId) -> bool {
        if let Some(active_pane_id) = &self.active_panes.get(&client_id) {
            if let Some(active_pane) = self.panes.get_mut(active_pane_id) {
                let full_pane_size = active_pane.position_and_size();
                if full_pane_size.cols.as_usize() < MIN_TERMINAL_WIDTH * 2 {
                    return false;
                }
                return split(SplitDirection::Vertical, &full_pane_size).is_some();
            }
        }
        false
    }
    pub fn split_pane_horizontally(
        &mut self,
        pid: PaneId,
        mut new_pane: Box<dyn Pane>,
        client_id: ClientId,
    ) {
        let active_pane_id = &self.active_panes.get(&client_id).unwrap();
        let active_pane = self.panes.get_mut(active_pane_id).unwrap();
        let full_pane_size = active_pane.position_and_size();
        if let Some((top_winsize, bottom_winsize)) =
            split(SplitDirection::Horizontal, &full_pane_size)
        {
            active_pane.set_geom(top_winsize);
            new_pane.set_geom(bottom_winsize);
            self.panes.insert(pid, new_pane);
            self.relayout(SplitDirection::Vertical);
        }
    }
    pub fn split_pane_vertically(
        &mut self,
        pid: PaneId,
        mut new_pane: Box<dyn Pane>,
        client_id: ClientId,
    ) {
        let active_pane_id = &self.active_panes.get(&client_id).unwrap();
        let active_pane = self.panes.get_mut(active_pane_id).unwrap();
        let full_pane_size = active_pane.position_and_size();
        if let Some((left_winsize, right_winsize)) =
            split(SplitDirection::Vertical, &full_pane_size)
        {
            active_pane.set_geom(left_winsize);
            new_pane.set_geom(right_winsize);
            self.panes.insert(pid, new_pane);
            self.relayout(SplitDirection::Horizontal);
        }
    }
    pub fn focus_pane(&mut self, pane_id: PaneId, client_id: ClientId) {
        self.active_panes.insert(client_id, pane_id);
        if self.session_is_mirrored {
            // move all clients
            let connected_clients: Vec<ClientId> =
                self.connected_clients.borrow().iter().copied().collect();
            for client_id in connected_clients {
                self.active_panes.insert(client_id, pane_id);
            }
        }
    }
    pub fn clear_active_panes(&mut self) {
        self.active_panes.clear();
    }
    pub fn first_active_pane_id(&self) -> Option<PaneId> {
        self.connected_clients
            .borrow()
            .iter()
            .next()
            .and_then(|first_client_id| self.active_panes.get(first_client_id).copied())
    }
    pub fn focused_pane_id(&self, client_id: ClientId) -> Option<PaneId> {
        self.active_panes.get(&client_id).copied()
    }
    // FIXME: Really not a fan of allowing this... Someone with more energy
    // than me should clean this up someday...
    #[allow(clippy::borrowed_box)]
    pub fn get_pane(&self, pane_id: PaneId) -> Option<&Box<dyn Pane>> {
        self.panes.get(&pane_id)
    }
    pub fn get_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Box<dyn Pane>> {
        self.panes.get_mut(&pane_id)
    }
    pub fn get_active_pane_id(&self, client_id: ClientId) -> Option<PaneId> {
        self.active_panes.get(&client_id).copied()
    }
    pub fn panes_contain(&self, pane_id: &PaneId) -> bool {
        self.panes.contains_key(pane_id)
    }
    pub fn set_force_render(&mut self) {
        for pane in self.panes.values_mut() {
            pane.set_should_render(true);
            pane.set_should_render_boundaries(true);
            pane.render_full_viewport();
        }
    }
    pub fn has_active_panes(&self) -> bool {
        !self.active_panes.is_empty()
    }
    pub fn has_panes(&self) -> bool {
        !self.panes.is_empty()
    }
    pub fn render(&mut self, output: &mut Output, floating_panes_are_visible: bool) -> Result<()> {
        let err_context = || "failed to render tiled panes";
        let connected_clients: Vec<ClientId> =
            { self.connected_clients.borrow().iter().copied().collect() };
        let multiple_users_exist_in_session = { self.connected_clients_in_app.borrow().len() > 1 };
        let mut client_id_to_boundaries: HashMap<ClientId, Boundaries> = HashMap::new();
        let active_panes = if self.session_is_mirrored || floating_panes_are_visible {
            HashMap::new()
        } else {
            self.active_panes
                .iter()
                .filter(|(client_id, _pane_id)| connected_clients.contains(client_id))
                .map(|(client_id, pane_id)| (*client_id, *pane_id))
                .collect()
        };
        for (kind, pane) in self.panes.iter_mut() {
            if !self.panes_to_hide.contains(&pane.pid()) {
                let mut pane_contents_and_ui = PaneContentsAndUi::new(
                    pane,
                    output,
                    self.style,
                    &active_panes,
                    multiple_users_exist_in_session,
                    None,
                );
                for client_id in &connected_clients {
                    let client_mode = self
                        .mode_info
                        .borrow()
                        .get(client_id)
                        .unwrap_or(&self.default_mode_info)
                        .mode;
                    let err_context =
                        || format!("failed to render tiled panes for client {client_id}");
                    if let PaneId::Plugin(..) = kind {
                        pane_contents_and_ui
                            .render_pane_contents_for_client(*client_id)
                            .with_context(err_context)?;
                    }
                    if self.draw_pane_frames {
                        pane_contents_and_ui
                            .render_pane_frame(*client_id, client_mode, self.session_is_mirrored)
                            .with_context(err_context)?;
                    } else {
                        let boundaries = client_id_to_boundaries
                            .entry(*client_id)
                            .or_insert_with(|| Boundaries::new(*self.viewport.borrow()));
                        pane_contents_and_ui.render_pane_boundaries(
                            *client_id,
                            client_mode,
                            boundaries,
                            self.session_is_mirrored,
                        );
                    }
                    pane_contents_and_ui.render_terminal_title_if_needed(*client_id, client_mode);
                    // this is done for panes that don't have their own cursor (eg. panes of
                    // another user)
                    pane_contents_and_ui
                        .render_fake_cursor_if_needed(*client_id)
                        .with_context(err_context)?;
                }
                if let PaneId::Terminal(..) = kind {
                    pane_contents_and_ui
                        .render_pane_contents_to_multiple_clients(connected_clients.iter().copied())
                        .with_context(err_context)?;
                }
            }
        }
        // render boundaries if needed
        for (client_id, boundaries) in &mut client_id_to_boundaries {
            // TODO: add some conditional rendering here so this isn't rendered for every character
            output.add_character_chunks_to_client(
                *client_id,
                boundaries.render().with_context(err_context)?,
                None,
            );
        }
        Ok(())
    }
    pub fn get_panes(&self) -> impl Iterator<Item = (&PaneId, &Box<dyn Pane>)> {
        self.panes.iter()
    }
    pub fn resize(&mut self, new_screen_size: Size) {
        // this is blocked out to appease the borrow checker
        {
            let mut display_area = self.display_area.borrow_mut();
            let mut viewport = self.viewport.borrow_mut();
            let Size { rows, cols } = new_screen_size;
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *display_area,
                *viewport,
            );
            match pane_grid.layout(SplitDirection::Horizontal, cols) {
                Ok(_) => {
                    let column_difference = cols as isize - display_area.cols as isize;
                    // FIXME: Should the viewport be an Offset?
                    viewport.cols = (viewport.cols as isize + column_difference) as usize;
                    display_area.cols = cols;
                },
                Err(e) => {
                    log::error!("Failed to horizontally resize the tab: {:?}", e);
                },
            };
            if pane_grid.layout(SplitDirection::Vertical, rows).is_ok() {
                let row_difference = rows as isize - display_area.rows as isize;
                viewport.rows = (viewport.rows as isize + row_difference) as usize;
                display_area.rows = rows;
            } else {
                log::error!("Failed to vertically resize the tab!!!");
            }
        }
        self.set_pane_frames(self.draw_pane_frames);
    }
    pub fn resize_active_pane_left(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.resize_pane_left(&active_pane_id);
            for pane in self.panes.values_mut() {
                resize_pty!(pane, self.os_api);
            }
        }
    }
    pub fn resize_active_pane_right(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.resize_pane_right(&active_pane_id);
            for pane in self.panes.values_mut() {
                resize_pty!(pane, self.os_api);
            }
        }
    }
    pub fn resize_active_pane_up(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.resize_pane_up(&active_pane_id);
            for pane in self.panes.values_mut() {
                resize_pty!(pane, self.os_api);
            }
        }
    }
    pub fn resize_active_pane_down(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.resize_pane_down(&active_pane_id);
            for pane in self.panes.values_mut() {
                resize_pty!(pane, self.os_api);
            }
        }
    }
    pub fn resize_active_pane_increase(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.resize_increase(&active_pane_id);
            for pane in self.panes.values_mut() {
                resize_pty!(pane, self.os_api);
            }
        }
    }
    pub fn resize_active_pane_decrease(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let mut pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            pane_grid.resize_decrease(&active_pane_id);
            for pane in self.panes.values_mut() {
                resize_pty!(pane, self.os_api);
            }
        }
    }
    pub fn focus_next_pane(&mut self, client_id: ClientId) {
        let connected_clients: Vec<ClientId> =
            { self.connected_clients.borrow().iter().copied().collect() };
        let active_pane_id = self.get_active_pane_id(client_id).unwrap();
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_active_pane_id = pane_grid.next_selectable_pane_id(&active_pane_id);
        for client_id in connected_clients {
            self.active_panes.insert(client_id, next_active_pane_id);
        }
        self.set_pane_active_at(next_active_pane_id);
    }
    pub fn focus_previous_pane(&mut self, client_id: ClientId) {
        let connected_clients: Vec<ClientId> =
            { self.connected_clients.borrow().iter().copied().collect() };
        let active_pane_id = self.get_active_pane_id(client_id).unwrap();
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let next_active_pane_id = pane_grid.previous_selectable_pane_id(&active_pane_id);
        for client_id in connected_clients {
            self.active_panes.insert(client_id, next_active_pane_id);
        }
        self.set_pane_active_at(next_active_pane_id);
    }
    fn set_pane_active_at(&mut self, pane_id: PaneId) {
        if let Some(pane) = self.get_pane_mut(pane_id) {
            pane.set_active_at(Instant::now());
        }
    }
    pub fn cursor_height_width_ratio(&self) -> Option<usize> {
        let character_cell_size = self.character_cell_size.borrow();
        character_cell_size.map(|size_in_pixels| {
            (size_in_pixels.height as f64 / size_in_pixels.width as f64).round() as usize
        })
    }
    pub fn move_focus_left(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid.next_selectable_pane_id_to_the_left(&active_pane_id);
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_focus_down(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid.next_selectable_pane_id_below(&active_pane_id);
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_focus_up(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid.next_selectable_pane_id_above(&active_pane_id);
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_focus_right(&mut self, client_id: ClientId) -> bool {
        match self.get_active_pane_id(client_id) {
            Some(active_pane_id) => {
                let pane_grid = TiledPaneGrid::new(
                    &mut self.panes,
                    &self.panes_to_hide,
                    *self.display_area.borrow(),
                    *self.viewport.borrow(),
                );
                let next_index = pane_grid.next_selectable_pane_id_to_the_right(&active_pane_id);
                match next_index {
                    Some(p) => {
                        // render previously active pane so that its frame does not remain actively
                        // colored
                        let previously_active_pane = self
                            .panes
                            .get_mut(self.active_panes.get(&client_id).unwrap())
                            .unwrap();

                        previously_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        previously_active_pane.render_full_viewport();

                        let next_active_pane = self.panes.get_mut(&p).unwrap();
                        next_active_pane.set_should_render(true);
                        // we render the full viewport to remove any ui elements that might have been
                        // there before (eg. another user's cursor)
                        next_active_pane.render_full_viewport();

                        self.focus_pane(p, client_id);
                        self.set_pane_active_at(p);

                        true
                    },
                    None => false,
                }
            },
            None => false,
        }
    }
    pub fn move_active_pane(&mut self, client_id: ClientId) {
        let active_pane_id = self.get_active_pane_id(client_id).unwrap();
        let pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        let new_position_id = pane_grid.next_selectable_pane_id(&active_pane_id);
        let current_position = self.panes.get(&active_pane_id).unwrap();
        let prev_geom = current_position.position_and_size();
        let prev_geom_override = current_position.geom_override();

        let new_position = self.panes.get_mut(&new_position_id).unwrap();
        let next_geom = new_position.position_and_size();
        let next_geom_override = new_position.geom_override();
        new_position.set_geom(prev_geom);
        if let Some(geom) = prev_geom_override {
            new_position.set_geom_override(geom);
        }
        resize_pty!(new_position, self.os_api);
        new_position.set_should_render(true);

        let current_position = self.panes.get_mut(&active_pane_id).unwrap();
        current_position.set_geom(next_geom);
        if let Some(geom) = next_geom_override {
            current_position.set_geom_override(geom);
        }
        resize_pty!(current_position, self.os_api);
        current_position.set_should_render(true);
    }
    pub fn move_active_pane_down(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            let next_index = pane_grid.next_selectable_pane_id_below(&active_pane_id);
            if let Some(p) = next_index {
                let active_pane_id = self.active_panes.get(&client_id).unwrap();
                let current_position = self.panes.get(active_pane_id).unwrap();
                let prev_geom = current_position.position_and_size();
                let prev_geom_override = current_position.geom_override();

                let new_position = self.panes.get_mut(&p).unwrap();
                let next_geom = new_position.position_and_size();
                let next_geom_override = new_position.geom_override();
                new_position.set_geom(prev_geom);
                if let Some(geom) = prev_geom_override {
                    new_position.set_geom_override(geom);
                }
                resize_pty!(new_position, self.os_api);
                new_position.set_should_render(true);

                let current_position = self.panes.get_mut(active_pane_id).unwrap();
                current_position.set_geom(next_geom);
                if let Some(geom) = next_geom_override {
                    current_position.set_geom_override(geom);
                }
                resize_pty!(current_position, self.os_api);
                current_position.set_should_render(true);
            }
        }
    }
    pub fn move_active_pane_left(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            let next_index = pane_grid.next_selectable_pane_id_to_the_left(&active_pane_id);
            if let Some(p) = next_index {
                let active_pane_id = self.active_panes.get(&client_id).unwrap();
                let current_position = self.panes.get(active_pane_id).unwrap();
                let prev_geom = current_position.position_and_size();
                let prev_geom_override = current_position.geom_override();

                let new_position = self.panes.get_mut(&p).unwrap();
                let next_geom = new_position.position_and_size();
                let next_geom_override = new_position.geom_override();
                new_position.set_geom(prev_geom);
                if let Some(geom) = prev_geom_override {
                    new_position.set_geom_override(geom);
                }
                resize_pty!(new_position, self.os_api);
                new_position.set_should_render(true);

                let current_position = self.panes.get_mut(active_pane_id).unwrap();
                current_position.set_geom(next_geom);
                if let Some(geom) = next_geom_override {
                    current_position.set_geom_override(geom);
                }
                resize_pty!(current_position, self.os_api);
                current_position.set_should_render(true);
            }
        }
    }
    pub fn move_active_pane_right(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            let next_index = pane_grid.next_selectable_pane_id_to_the_right(&active_pane_id);
            if let Some(p) = next_index {
                let active_pane_id = self.active_panes.get(&client_id).unwrap();
                let current_position = self.panes.get(active_pane_id).unwrap();
                let prev_geom = current_position.position_and_size();
                let prev_geom_override = current_position.geom_override();

                let new_position = self.panes.get_mut(&p).unwrap();
                let next_geom = new_position.position_and_size();
                let next_geom_override = new_position.geom_override();
                new_position.set_geom(prev_geom);
                if let Some(geom) = prev_geom_override {
                    new_position.set_geom_override(geom);
                }
                resize_pty!(new_position, self.os_api);
                new_position.set_should_render(true);

                let current_position = self.panes.get_mut(active_pane_id).unwrap();
                current_position.set_geom(next_geom);
                if let Some(geom) = next_geom_override {
                    current_position.set_geom_override(geom);
                }
                resize_pty!(current_position, self.os_api);
                current_position.set_should_render(true);
            }
        }
    }
    pub fn move_active_pane_up(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            let pane_grid = TiledPaneGrid::new(
                &mut self.panes,
                &self.panes_to_hide,
                *self.display_area.borrow(),
                *self.viewport.borrow(),
            );
            let next_index = pane_grid.next_selectable_pane_id_above(&active_pane_id);
            if let Some(p) = next_index {
                let active_pane_id = self.active_panes.get(&client_id).unwrap();
                let current_position = self.panes.get(active_pane_id).unwrap();
                let prev_geom = current_position.position_and_size();
                let prev_geom_override = current_position.geom_override();

                let new_position = self.panes.get_mut(&p).unwrap();
                let next_geom = new_position.position_and_size();
                let next_geom_override = new_position.geom_override();
                new_position.set_geom(prev_geom);
                if let Some(geom) = prev_geom_override {
                    new_position.set_geom_override(geom);
                }
                resize_pty!(new_position, self.os_api);
                new_position.set_should_render(true);

                let current_position = self.panes.get_mut(active_pane_id).unwrap();
                current_position.set_geom(next_geom);
                if let Some(geom) = next_geom_override {
                    current_position.set_geom_override(geom);
                }
                resize_pty!(current_position, self.os_api);
                current_position.set_should_render(true);
            }
        }
    }
    pub fn move_clients_out_of_pane(&mut self, pane_id: PaneId) {
        let active_panes: Vec<(ClientId, PaneId)> = self
            .active_panes
            .iter()
            .map(|(cid, pid)| (*cid, *pid))
            .collect();
        match self
            .panes
            .iter()
            .filter(|(p_id, _)| !self.panes_to_hide.contains(p_id))
            .find(|(p_id, p)| **p_id != pane_id && p.selectable())
            .map(|(p_id, _p)| p_id)
        {
            Some(next_active_pane) => {
                for (client_id, active_pane_id) in active_panes {
                    if active_pane_id == pane_id {
                        self.active_panes.insert(client_id, *next_active_pane);
                    }
                }
            },
            None => self.active_panes.clear(),
        }
    }
    pub fn extract_pane(&mut self, pane_id: PaneId) -> Option<Box<dyn Pane>> {
        self.panes.remove(&pane_id)
    }
    pub fn remove_pane(&mut self, pane_id: PaneId) -> Option<Box<dyn Pane>> {
        let mut pane_grid = TiledPaneGrid::new(
            &mut self.panes,
            &self.panes_to_hide,
            *self.display_area.borrow(),
            *self.viewport.borrow(),
        );
        if pane_grid.fill_space_over_pane(pane_id) {
            // successfully filled space over pane
            let closed_pane = self.panes.remove(&pane_id);
            self.move_clients_out_of_pane(pane_id);
            self.set_pane_frames(self.draw_pane_frames); // recalculate pane frames and update size
            closed_pane
        } else {
            self.panes.remove(&pane_id);
            // this is a bit of a roundabout way to say: this is the last pane and so the tab
            // should be destroyed
            self.active_panes.clear();
            None
        }
    }
    pub fn hold_pane(
        &mut self,
        pane_id: PaneId,
        exit_status: Option<i32>,
        run_command: RunCommand,
    ) {
        self.panes
            .get_mut(&pane_id)
            .map(|p| p.hold(exit_status, run_command));
    }
    pub fn panes_to_hide_contains(&self, pane_id: PaneId) -> bool {
        self.panes_to_hide.contains(&pane_id)
    }
    pub fn fullscreen_is_active(&self) -> bool {
        self.fullscreen_is_active
    }
    pub fn unset_fullscreen(&mut self) {
        if self.fullscreen_is_active {
            let first_client_id = {
                let connected_clients = self.connected_clients.borrow();
                *connected_clients.iter().next().unwrap()
            };
            let active_pane_id = self.get_active_pane_id(first_client_id).unwrap();
            let panes_to_hide: Vec<_> = self.panes_to_hide.iter().copied().collect();
            for pane_id in panes_to_hide {
                let pane = self.get_pane_mut(pane_id).unwrap();
                pane.set_should_render(true);
                pane.set_should_render_boundaries(true);
            }
            let viewport_pane_ids: Vec<_> = self
                .panes
                .keys()
                .copied()
                .into_iter()
                .filter(|id| {
                    !is_inside_viewport(&*self.viewport.borrow(), self.get_pane(*id).unwrap())
                })
                .collect();
            for pid in viewport_pane_ids {
                let viewport_pane = self.get_pane_mut(pid).unwrap();
                viewport_pane.reset_size_and_position_override();
            }
            self.panes_to_hide.clear();
            let active_terminal = self.get_pane_mut(active_pane_id).unwrap();
            active_terminal.reset_size_and_position_override();
            self.set_force_render();
            let display_area = *self.display_area.borrow();
            self.resize(display_area);
            self.fullscreen_is_active = false;
        }
    }
    pub fn toggle_active_pane_fullscreen(&mut self, client_id: ClientId) {
        if let Some(active_pane_id) = self.get_active_pane_id(client_id) {
            if self.fullscreen_is_active {
                self.unset_fullscreen();
            } else {
                let pane_ids_to_hide = self.panes.iter().filter_map(|(&id, _pane)| {
                    if id != active_pane_id
                        && is_inside_viewport(&*self.viewport.borrow(), self.get_pane(id).unwrap())
                    {
                        Some(id)
                    } else {
                        None
                    }
                });
                self.panes_to_hide = pane_ids_to_hide.collect();
                if self.panes_to_hide.is_empty() {
                    // nothing to do, pane is already as fullscreen as it can be, let's bail
                    return;
                } else {
                    // For all of the panes outside of the viewport staying on the fullscreen
                    // screen, switch them to using override positions as well so that the resize
                    // system doesn't get confused by viewport and old panes that no longer line up
                    let viewport_pane_ids: Vec<_> = self
                        .panes
                        .keys()
                        .copied()
                        .into_iter()
                        .filter(|id| {
                            !is_inside_viewport(
                                &*self.viewport.borrow(),
                                self.get_pane(*id).unwrap(),
                            )
                        })
                        .collect();
                    for pid in viewport_pane_ids {
                        let viewport_pane = self.get_pane_mut(pid).unwrap();
                        viewport_pane.set_geom_override(viewport_pane.position_and_size());
                    }
                    let viewport = { *self.viewport.borrow() };
                    let active_terminal = self.get_pane_mut(active_pane_id).unwrap();
                    let full_screen_geom = PaneGeom {
                        x: viewport.x,
                        y: viewport.y,
                        ..Default::default()
                    };
                    active_terminal.set_geom_override(full_screen_geom);
                }
                let connected_client_list: Vec<ClientId> =
                    { self.connected_clients.borrow().iter().copied().collect() };
                for client_id in connected_client_list {
                    self.focus_pane(active_pane_id, client_id);
                }
                self.set_force_render();
                let display_area = *self.display_area.borrow();
                self.resize(display_area);
                self.fullscreen_is_active = true;
            }
        }
    }

    pub fn switch_next_pane_fullscreen(&mut self, client_id: ClientId) {
        self.unset_fullscreen();
        self.focus_next_pane(client_id);
        self.toggle_active_pane_fullscreen(client_id);
    }

    pub fn switch_prev_pane_fullscreen(&mut self, client_id: ClientId) {
        self.unset_fullscreen();
        self.focus_previous_pane(client_id);
        self.toggle_active_pane_fullscreen(client_id);
    }

    pub fn panes_to_hide_count(&self) -> usize {
        self.panes_to_hide.len()
    }
    pub fn add_to_hidden_panels(&mut self, pid: PaneId) {
        self.panes_to_hide.insert(pid);
    }
    pub fn remove_from_hidden_panels(&mut self, pid: PaneId) {
        self.panes_to_hide.remove(&pid);
    }
    fn move_clients_between_panes(&mut self, from_pane_id: PaneId, to_pane_id: PaneId) {
        let clients_in_pane: Vec<ClientId> = self
            .active_panes
            .iter()
            .filter(|(_cid, pid)| **pid == from_pane_id)
            .map(|(cid, _pid)| *cid)
            .collect();
        for client_id in clients_in_pane {
            self.active_panes.remove(&client_id);
            self.active_panes.insert(client_id, to_pane_id);
        }
    }
}

#[allow(clippy::borrowed_box)]
pub fn is_inside_viewport(viewport: &Viewport, pane: &Box<dyn Pane>) -> bool {
    let pane_position_and_size = pane.current_geom();
    pane_position_and_size.y >= viewport.y
        && pane_position_and_size.y + pane_position_and_size.rows.as_usize()
            <= viewport.y + viewport.rows
}

pub fn pane_geom_is_inside_viewport(viewport: &Viewport, geom: &PaneGeom) -> bool {
    geom.y >= viewport.y
        && geom.y + geom.rows.as_usize() <= viewport.y + viewport.rows
        && geom.x >= viewport.x
        && geom.x + geom.cols.as_usize() <= viewport.x + viewport.cols
}
