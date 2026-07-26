use super::*;

/// The modeline's model indicator is the affordance for routing: clicking
/// it opens this menu (spec 0114 — the substitution is shown where the
/// model is shown, so the two are never confused).
///
/// Two steps, because a target and a model are separate choices: pick where
/// the traffic goes, then which model to ask for. `Default` is the first
/// row of the first step and needs no second one — it is the absence of a
/// route, not a target with a model.
impl App {
    pub(super) async fn open_route_menu(&mut self, session_id: String, col: u16, row: u16) {
        let listed = match self.client.list_routes(Some(&session_id)).await {
            Ok(l) => l,
            Err(e) => {
                self.set_status(format!("routes unavailable: {e}"));
                return;
            }
        };
        let active = listed.active.clone();
        // Open on the armed target so the current state is where the eye
        // already is.
        let selected = listed
            .routes
            .iter()
            .position(|r| Some(&r.name) == active.as_ref())
            .map(|i| i + 1)
            .unwrap_or(0);
        let menu = RouteMenu {
            session_id,
            area: ratatui::layout::Rect::default(),
            routes: listed.routes,
            unavailable_reason: listed.unavailable_reason,
            active,
            stage: RouteStage::Target,
            selected,
            anchor: (col, row),
        };
        self.route_menu = Some(menu.anchored(self.frame_area()));
    }

    fn frame_area(&self) -> ratatui::layout::Rect {
        self.layout
            .frame_area
            .unwrap_or(ratatui::layout::Rect::new(0, 0, 80, 24))
    }

    /// Activate the highlighted row: descend into a target's models, or
    /// arm the selection.
    pub(super) async fn activate_route_menu_row(&mut self, index: usize) {
        let Some(menu) = self.route_menu.clone() else {
            return;
        };
        match menu.stage {
            RouteStage::Target => {
                // Row 0 is Default: clearing needs no model, and can never
                // fail, so it arms straight away.
                let Some(route_index) = index.checked_sub(1) else {
                    self.route_menu = None;
                    self.apply_route(menu.session_id, None, None).await;
                    return;
                };
                let Some(route) = menu.routes.get(route_index) else {
                    return;
                };
                if let Some(reason) = route.unavailable_reason.as_deref() {
                    self.set_status(format!("{}: {reason}", route.name));
                    return;
                }
                if let Some(menu) = self.route_menu.as_mut() {
                    // Preselect the model already armed on this target, if
                    // any, so re-opening lands on the current choice.
                    menu.selected = 0;
                    menu.stage = RouteStage::Model { route_index };
                }
                let frame = self.frame_area();
                if let Some(menu) = self.route_menu.take() {
                    self.route_menu = Some(menu.anchored(frame));
                }
            }
            RouteStage::Model { route_index } => {
                let Some(route) = menu.routes.get(route_index) else {
                    return;
                };
                let Some(model) = menu.model_rows(route).get(index).cloned() else {
                    return;
                };
                self.route_menu = None;
                self.apply_route(menu.session_id, Some(route.name.clone()), Some(model))
                    .await;
            }
        }
    }

    async fn apply_route(&mut self, session_id: String, route: Option<String>, model: Option<String>) {
        let label = match (&route, &model) {
            (Some(r), Some(m)) => format!("{r} · {m}"),
            (Some(r), None) => r.clone(),
            (None, _) => "default".to_string(),
        };
        match self.client.set_route(&session_id, route, model).await {
            Ok(()) => self.set_status(format!("route: {label}")),
            Err(e) => self.set_status(format!("route failed: {e}")),
        }
    }

    /// Step back a stage, or close from the first. Returns false when the
    /// menu closed, so the caller knows the key was fully consumed.
    pub(super) fn route_menu_back(&mut self) {
        let Some(menu) = self.route_menu.as_mut() else {
            return;
        };
        match menu.stage {
            RouteStage::Model { route_index } => {
                menu.stage = RouteStage::Target;
                menu.selected = route_index + 1;
                let frame = self.frame_area();
                if let Some(menu) = self.route_menu.take() {
                    self.route_menu = Some(menu.anchored(frame));
                }
            }
            RouteStage::Target => self.route_menu = None,
        }
    }

    pub(super) fn move_route_menu_selection(&mut self, delta: isize) {
        let Some(menu) = self.route_menu.as_mut() else {
            return;
        };
        let len = menu.rows();
        if len == 0 {
            return;
        }
        menu.selected = (menu.selected as isize + delta).rem_euclid(len as isize) as usize;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStage {
    /// Pick where traffic goes: Default, then each target.
    Target,
    /// Pick which model to ask that target for.
    Model { route_index: usize },
}

#[derive(Debug, Clone)]
pub struct RouteMenu {
    pub session_id: String,
    pub area: ratatui::layout::Rect,
    pub routes: Vec<construct_protocol::RouteOption>,
    /// Why this session cannot be routed at all, if it cannot. The menu
    /// still opens and still offers Default — an empty popup would leave
    /// the user with no explanation (spec 0115).
    pub unavailable_reason: Option<String>,
    pub active: Option<String>,
    pub stage: RouteStage,
    pub selected: usize,
    anchor: (u16, u16),
}

impl RouteMenu {
    /// Models offered for a target. Always at least its default, so the
    /// second step never dead-ends.
    pub fn model_rows(&self, route: &construct_protocol::RouteOption) -> Vec<String> {
        if route.models.is_empty() {
            return vec![route.model.clone()];
        }
        route.models.clone()
    }

    fn current_route(&self) -> Option<&construct_protocol::RouteOption> {
        match self.stage {
            RouteStage::Model { route_index } => self.routes.get(route_index),
            RouteStage::Target => None,
        }
    }

    pub fn title(&self) -> String {
        match self.current_route() {
            Some(route) => format!(" {} ", route.name),
            None => " route ".to_string(),
        }
    }

    pub fn rows(&self) -> usize {
        match self.current_route() {
            Some(route) => self.model_rows(route).len(),
            // Default plus one row per target.
            None => 1 + self.routes.len(),
        }
    }

    pub fn label(&self, index: usize) -> String {
        if let Some(route) = self.current_route() {
            return self
                .model_rows(route)
                .get(index)
                .cloned()
                .unwrap_or_default();
        }
        match index.checked_sub(1) {
            // Not "pass through": from the user's side this is simply the
            // session's own model, unrouted.
            None => "Default".to_string(),
            Some(i) => match self.routes.get(i) {
                Some(r) => r.name.clone(),
                None => String::new(),
            },
        }
    }

    /// Trailing detail for a row — the dialect, or nothing in the model
    /// step where the row is already the whole answer.
    pub fn detail(&self, index: usize) -> Option<String> {
        if self.current_route().is_some() {
            return None;
        }
        let route = self.routes.get(index.checked_sub(1)?)?;
        Some(route.dialect.clone())
    }

    pub fn row_enabled(&self, index: usize) -> bool {
        if self.current_route().is_some() {
            return true;
        }
        match index.checked_sub(1) {
            None => true,
            Some(i) => self
                .routes
                .get(i)
                .is_some_and(|r| r.unavailable_reason.is_none()),
        }
    }

    /// Whether a row is what the session is on right now.
    pub fn is_active(&self, index: usize) -> bool {
        if let Some(route) = self.current_route() {
            return self
                .model_rows(route)
                .get(index)
                .is_some_and(|m| *m == route.model);
        }
        match index.checked_sub(1) {
            None => self.active.is_none(),
            Some(i) => self
                .routes
                .get(i)
                .is_some_and(|r| Some(&r.name) == self.active.as_ref()),
        }
    }

    /// Row 0 of the target step opens no submenu; every other target row
    /// does. Used to hint the descent in the render.
    pub fn row_descends(&self, index: usize) -> bool {
        self.current_route().is_none() && index > 0 && self.row_enabled(index)
    }

    /// Place the menu above its anchor when there isn't room below — the
    /// modeline sits at the bottom of the frame, so downward is almost
    /// never available.
    pub fn anchored(mut self, size: ratatui::layout::Rect) -> Self {
        let (col, row) = self.anchor;
        let width = self.desired_width().min(size.width.saturating_sub(2).max(8));
        let height = (self.rows() as u16)
            .saturating_add(2)
            .saturating_add(if self.unavailable_reason.is_some() { 1 } else { 0 })
            .min(size.height.max(3));
        let x = col.min(size.width.saturating_sub(width));
        let y = row
            .saturating_sub(height)
            .min(size.height.saturating_sub(height));
        self.area = ratatui::layout::Rect {
            x,
            y,
            width,
            height,
        };
        self
    }

    fn desired_width(&self) -> u16 {
        let widest = (0..self.rows())
            .map(|i| {
                self.label(i).chars().count()
                    + self.detail(i).map(|d| d.chars().count() + 3).unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        let reason = self
            .unavailable_reason
            .as_deref()
            .map(|r| r.chars().count())
            .unwrap_or(0);
        (widest.max(reason).max(14) as u16).saturating_add(6)
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.area.x
            && col < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }

    /// Which row a click landed on, if any.
    pub fn item_at(&self, col: u16, row: u16) -> Option<usize> {
        if !self.contains(col, row) {
            return None;
        }
        let first = self.area.y.saturating_add(1);
        let index = row.checked_sub(first)? as usize;
        (index < self.rows()).then_some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(name: &str, models: &[&str], reason: Option<&str>) -> construct_protocol::RouteOption {
        construct_protocol::RouteOption {
            name: name.to_string(),
            dialect: "anthropic".to_string(),
            model: models.first().copied().unwrap_or_default().to_string(),
            models: models.iter().map(|m| m.to_string()).collect(),
            base_url: "https://example.invalid".to_string(),
            unavailable_reason: reason.map(str::to_string),
        }
    }

    fn menu(routes: Vec<construct_protocol::RouteOption>, active: Option<&str>) -> RouteMenu {
        RouteMenu {
            session_id: "s1".into(),
            area: ratatui::layout::Rect::default(),
            routes,
            unavailable_reason: None,
            active: active.map(str::to_string),
            stage: RouteStage::Target,
            selected: 0,
            anchor: (40, 23),
        }
    }

    #[test]
    fn the_first_row_is_default_and_always_selectable() {
        let m = menu(vec![option("kimi", &["kimi-k2.5"], Some("no key"))], None);
        assert_eq!(m.label(0), "Default");
        assert!(m.row_enabled(0));
        assert!(!m.row_descends(0), "Default has no second step");
        assert!(!m.row_enabled(1), "an unusable target is not selectable");
        assert!(m.is_active(0), "no route armed means Default is current");
    }

    #[test]
    fn the_target_step_lists_default_plus_every_target() {
        let m = menu(
            vec![
                option("claude-oauth", &["claude-opus-5"], None),
                option("kimi", &["kimi-k2.5"], None),
            ],
            Some("kimi"),
        );
        assert_eq!(m.rows(), 3);
        assert_eq!(m.label(1), "claude-oauth");
        assert_eq!(m.detail(1).as_deref(), Some("anthropic"));
        assert!(m.is_active(2));
        assert!(m.row_descends(1), "a target opens its model step");
    }

    #[test]
    fn the_model_step_lists_that_targets_models() {
        let mut m = menu(
            vec![option("codex-oauth", &["gpt-5.6-sol", "gpt-5.5"], None)],
            None,
        );
        m.stage = RouteStage::Model { route_index: 0 };
        assert_eq!(m.rows(), 2);
        assert_eq!(m.label(0), "gpt-5.6-sol");
        assert_eq!(m.label(1), "gpt-5.5");
        assert_eq!(m.title(), " codex-oauth ");
        assert!(m.detail(0).is_none(), "the model row is the whole answer");
        assert!(
            m.is_active(0),
            "the target's current model is marked in the model step"
        );
    }

    /// A target that reports no model list still gets one row, so the
    /// second step never dead-ends.
    #[test]
    fn a_target_without_a_model_list_still_offers_its_default() {
        let mut route = option("bare", &[], None);
        route.model = "some-model".into();
        route.models.clear();
        let mut m = menu(vec![route], None);
        m.stage = RouteStage::Model { route_index: 0 };
        assert_eq!(m.rows(), 1);
        assert_eq!(m.label(0), "some-model");
    }

    #[test]
    fn anchors_above_the_modeline_and_stays_on_screen() {
        let size = ratatui::layout::Rect::new(0, 0, 80, 24);
        let mut m = menu(vec![option("kimi", &["kimi-k2.5"], None)], None);
        m.anchor = (70, 23);
        let m = m.anchored(size);
        assert!(m.area.y < 23, "menu must sit above its anchor row");
        assert!(
            m.area.x + m.area.width <= size.width,
            "menu must not overflow the frame: {:?}",
            m.area
        );
    }

    #[test]
    fn maps_clicks_to_rows_and_ignores_the_border() {
        let size = ratatui::layout::Rect::new(0, 0, 80, 24);
        let mut m = menu(vec![option("kimi", &["kimi-k2.5"], None)], None);
        m.anchor = (10, 23);
        let m = m.anchored(size);
        let first = m.area.y + 1;
        assert_eq!(m.item_at(m.area.x + 2, first), Some(0));
        assert_eq!(m.item_at(m.area.x + 2, first + 1), Some(1));
        assert_eq!(m.item_at(m.area.x + 2, m.area.y), None, "top border");
        assert_eq!(m.item_at(m.area.x.saturating_sub(1), first), None);
    }
}
