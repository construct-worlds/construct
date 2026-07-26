use super::*;

/// The modeline's model indicator is the affordance for routing: clicking
/// it opens this menu (spec 0114 — the substitution is shown where the
/// model is shown, so the two are never confused).
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
            selected,
        };
        let frame = self
            .layout
            .frame_area
            .unwrap_or(ratatui::layout::Rect::new(0, 0, 80, 24));
        self.route_menu = Some(menu.anchored(col, row, frame));
    }

    pub(super) async fn apply_route_menu_selection(&mut self, index: usize) {
        let Some(menu) = self.route_menu.clone() else {
            return;
        };
        // Row 0 is always "pass through": clearing a route can never fail,
        // so it stays selectable even when every configured route is not.
        let choice = match index.checked_sub(1) {
            None => None,
            Some(i) => match menu.routes.get(i) {
                Some(r) => {
                    if let Some(reason) = r.unavailable_reason.as_deref() {
                        self.set_status(format!("{}: {reason}", r.name));
                        return;
                    }
                    Some(r.name.clone())
                }
                None => return,
            },
        };
        self.route_menu = None;
        let label = choice.clone().unwrap_or_else(|| "pass through".to_string());
        match self.client.set_route(&menu.session_id, choice).await {
            Ok(()) => self.set_status(format!("route: {label}")),
            Err(e) => self.set_status(format!("route failed: {e}")),
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
        let next = (menu.selected as isize + delta).rem_euclid(len as isize);
        menu.selected = next as usize;
    }
}

#[derive(Debug, Clone)]
pub struct RouteMenu {
    pub session_id: String,
    pub area: ratatui::layout::Rect,
    pub routes: Vec<construct_protocol::RouteOption>,
    /// Why this session cannot be routed at all, if it cannot. The menu
    /// still opens and still offers pass-through — an empty popup would
    /// leave the user with no explanation (spec 0115).
    pub unavailable_reason: Option<String>,
    pub active: Option<String>,
    pub selected: usize,
}

impl RouteMenu {
    /// Total selectable rows: pass-through, plus one per configured route.
    pub fn rows(&self) -> usize {
        1 + self.routes.len()
    }

    pub fn label(&self, index: usize) -> String {
        match index.checked_sub(1) {
            None => "pass through".to_string(),
            Some(i) => match self.routes.get(i) {
                Some(r) => format!("{}  ({})", r.name, r.model),
                None => String::new(),
            },
        }
    }

    pub fn row_enabled(&self, index: usize) -> bool {
        match index.checked_sub(1) {
            None => true,
            Some(i) => self
                .routes
                .get(i)
                .is_some_and(|r| r.unavailable_reason.is_none()),
        }
    }

    pub fn is_active(&self, index: usize) -> bool {
        match index.checked_sub(1) {
            None => self.active.is_none(),
            Some(i) => self
                .routes
                .get(i)
                .is_some_and(|r| Some(&r.name) == self.active.as_ref()),
        }
    }

    /// Place the menu above its anchor when there isn't room below —
    /// the modeline sits at the bottom of the frame, so downward is
    /// almost never available.
    pub fn anchored(mut self, col: u16, row: u16, size: ratatui::layout::Rect) -> Self {
        let width = self
            .desired_width()
            .min(size.width.saturating_sub(2).max(8));
        let height = (self.rows() as u16)
            .saturating_add(2)
            .saturating_add(if self.unavailable_reason.is_some() { 1 } else { 0 })
            .min(size.height.max(3));
        let x = col.min(size.width.saturating_sub(width));
        let y = row.saturating_sub(height).min(size.height.saturating_sub(height));
        self.area = ratatui::layout::Rect {
            x,
            y,
            width,
            height,
        };
        self
    }

    fn desired_width(&self) -> u16 {
        let widest_row = (0..self.rows())
            .map(|i| self.label(i).chars().count())
            .max()
            .unwrap_or(0);
        let reason = self
            .unavailable_reason
            .as_deref()
            .map(|r| r.chars().count())
            .unwrap_or(0);
        (widest_row.max(reason).max(14) as u16).saturating_add(4)
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

    fn option(name: &str, reason: Option<&str>) -> construct_protocol::RouteOption {
        construct_protocol::RouteOption {
            name: name.to_string(),
            dialect: "anthropic".to_string(),
            model: format!("{name}-model"),
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
            selected: 0,
        }
    }

    #[test]
    fn pass_through_is_always_the_first_row_and_always_selectable() {
        let m = menu(vec![option("kimi", Some("no key"))], None);
        assert_eq!(m.label(0), "pass through");
        assert!(m.row_enabled(0));
        assert!(!m.row_enabled(1), "a route with a reason is not selectable");
        assert!(m.is_active(0), "no route armed means pass-through is active");
    }

    #[test]
    fn rows_cover_pass_through_plus_every_route() {
        let m = menu(vec![option("kimi", None), option("glm", None)], Some("glm"));
        assert_eq!(m.rows(), 3);
        assert_eq!(m.label(1), "kimi  (kimi-model)");
        assert!(m.is_active(2));
        assert!(!m.is_active(0));
    }

    #[test]
    fn anchors_above_the_modeline_and_stays_on_screen() {
        let size = ratatui::layout::Rect::new(0, 0, 80, 24);
        let m = menu(vec![option("kimi", None)], None).anchored(70, 23, size);
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
        let m = menu(vec![option("kimi", None)], None).anchored(10, 23, size);
        let first = m.area.y + 1;
        assert_eq!(m.item_at(m.area.x + 2, first), Some(0));
        assert_eq!(m.item_at(m.area.x + 2, first + 1), Some(1));
        assert_eq!(m.item_at(m.area.x + 2, m.area.y), None, "top border");
        assert_eq!(m.item_at(m.area.x.saturating_sub(1), first), None);
    }
}
