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
        let mut menu = RouteMenu {
            session_id,
            area: ratatui::layout::Rect::default(),
            routes: listed.routes,
            unavailable_reason: listed.unavailable_reason,
            active,
            focus: RouteFocus::Targets,
            selected,
            model_selected: 0,
            target_scroll: 0,
            model_scroll: 0,
            anchor: (col, row),
            target_col_w: 0,
            desc_lines: 0,
            visible_body_rows: 0,
        };
        menu.model_selected = menu.active_model_index();
        self.route_menu = Some(menu.anchored(self.frame_area()));
    }

    fn frame_area(&self) -> ratatui::layout::Rect {
        self.layout
            .frame_area
            .unwrap_or(ratatui::layout::Rect::new(0, 0, 80, 24))
    }

    fn replace_route_menu(&mut self, menu: RouteMenu) {
        let frame = self.frame_area();
        self.route_menu = Some(menu.anchored(frame));
    }

    /// A click: in the target column it selects and previews; in the model
    /// column it commits. One click to look, one to choose.
    pub(super) async fn hit_route_menu(&mut self, hit: RouteHit) {
        match hit {
            RouteHit::Target(index) => self.select_route_target(index).await,
            RouteHit::Model(index) => self.arm_route_model(index).await,
        }
    }

    async fn select_route_target(&mut self, index: usize) {
        let Some(mut menu) = self.route_menu.clone() else {
            return;
        };
        // Default is the absence of a route: it has nothing to preview, so
        // selecting it arms straight away.
        if index == 0 {
            self.route_menu = None;
            self.apply_route(menu.session_id, None, None).await;
            return;
        }
        menu.selected = index;
        menu.model_selected = menu.active_model_index();
        menu.model_scroll = 0;
        menu.focus = RouteFocus::Targets;
        menu.ensure_target_visible();
        self.replace_route_menu(menu);
    }

    async fn arm_route_model(&mut self, index: usize) {
        let Some(menu) = self.route_menu.clone() else {
            return;
        };
        let Some(route) = menu.focused_target() else {
            return;
        };
        if let Some(reason) = route.unavailable_reason.as_deref() {
            self.set_status(format!("{}: {reason}", route.name));
            return;
        }
        let Some(model) = menu.models().get(index).cloned() else {
            return;
        };
        let (session, name) = (menu.session_id.clone(), route.name.clone());
        self.route_menu = None;
        self.apply_route(session, Some(name), Some(model)).await;
    }

    /// Enter: from the target column, step into the models; from the model
    /// column, arm the highlighted one.
    pub(super) async fn activate_route_menu(&mut self) {
        let Some(menu) = self.route_menu.clone() else {
            return;
        };
        match menu.focus {
            RouteFocus::Targets => {
                if menu.selected == 0 {
                    self.select_route_target(0).await;
                } else {
                    self.route_menu_focus_models();
                }
            }
            RouteFocus::Models => self.arm_route_model(menu.model_selected).await,
        }
    }

    pub(super) fn route_menu_focus_models(&mut self) {
        let Some(menu) = self.route_menu.as_mut() else {
            return;
        };
        if menu.target_descends(menu.selected) && !menu.models().is_empty() {
            menu.focus = RouteFocus::Models;
            menu.ensure_model_visible();
        }
    }

    /// Left: hand focus back to the targets, or close if it is already
    /// there.
    pub(super) fn route_menu_back(&mut self) {
        let Some(menu) = self.route_menu.as_mut() else {
            return;
        };
        match menu.focus {
            RouteFocus::Models => menu.focus = RouteFocus::Targets,
            RouteFocus::Targets => self.route_menu = None,
        }
    }

    pub(super) fn move_route_menu_selection(&mut self, delta: isize) {
        let Some(mut menu) = self.route_menu.clone() else {
            return;
        };
        match menu.focus {
            RouteFocus::Targets => {
                let len = menu.target_rows();
                if len == 0 {
                    return;
                }
                menu.selected = (menu.selected as isize + delta).rem_euclid(len as isize) as usize;
                // Moving the target repoints the preview, so the model
                // highlight follows it rather than pointing at a row that
                // now belongs to a different target.
                menu.model_selected = menu.active_model_index();
                menu.model_scroll = 0;
                menu.ensure_target_visible();
            }
            RouteFocus::Models => {
                let len = menu.models().len();
                if len == 0 {
                    return;
                }
                menu.model_selected =
                    (menu.model_selected as isize + delta).rem_euclid(len as isize) as usize;
                menu.ensure_model_visible();
            }
        }
        self.replace_route_menu(menu);
    }

    /// Mouse wheel over the route menu. Returns true when the event was
    /// consumed so the terminal scrollback under the popup stays put.
    pub(super) fn scroll_route_menu(&mut self, col: u16, row: u16, delta: isize) -> bool {
        let Some(mut menu) = self.route_menu.clone() else {
            return false;
        };
        if !menu.scroll_at(col, row, delta) {
            return false;
        }
        self.replace_route_menu(menu);
        true
    }

    async fn apply_route(
        &mut self,
        session_id: String,
        route: Option<String>,
        model: Option<String>,
    ) {
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
}

/// One line on what routing does, shown once between Default and the
/// targets it contrasts with. Short on purpose: the picker is a menu, not
/// documentation, and the sentence has to earn its two rows.
pub const ROUTE_DESCRIPTION: &str =
    "Send this session's model request to another provider. No restart required";

/// Which column the keyboard is driving. Both are always visible; focus
/// only decides what moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteFocus {
    Targets,
    Models,
}

/// Where a click landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteHit {
    Target(usize),
    Model(usize),
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
    pub focus: RouteFocus,
    /// Highlighted target row; row 0 is Default.
    pub selected: usize,
    /// Highlighted model row within the highlighted target.
    pub model_selected: usize,
    /// First visible body row of the target list (Default is above the body
    /// and never scrolls).
    pub target_scroll: usize,
    /// First visible body row of the model list for the focused target.
    pub model_scroll: usize,
    anchor: (u16, u16),
    /// Width of the target column, including its padding. Set when the
    /// menu is placed so render and hit-testing agree on one number.
    pub target_col_w: u16,
    /// Rows the description wraps to. Set when the menu is placed, for the
    /// same reason.
    pub desc_lines: u16,
    /// Visible body rows after height-cap / frame clamping. Set by
    /// `anchored` so render, hit-testing, and keyboard scroll agree.
    pub visible_body_rows: usize,
}

impl RouteMenu {
    /// Rows in the target column: Default, then one per target.
    pub fn target_rows(&self) -> usize {
        1 + self.routes.len()
    }

    pub fn target_at(&self, index: usize) -> Option<&construct_protocol::RouteOption> {
        self.routes.get(index.checked_sub(1)?)
    }

    /// The target the model column is currently showing.
    pub fn focused_target(&self) -> Option<&construct_protocol::RouteOption> {
        self.target_at(self.selected)
    }

    /// Models for the highlighted target. Empty for Default, which is the
    /// absence of a route rather than a target with models.
    pub fn models(&self) -> Vec<String> {
        let Some(route) = self.focused_target() else {
            return Vec::new();
        };
        if route.models.is_empty() {
            return vec![route.model.clone()];
        }
        route.models.clone()
    }

    /// Rows above the two columns: Default, a rule, and the description.
    /// Default sits apart because it is the absence of a route, not one of
    /// the targets listed under it.
    pub fn header_rows(&self) -> u16 {
        2 + self.desc_lines
    }

    /// Tallest model column across every target — used so the menu height
    /// does not jump when the target highlight moves between short and
    /// long model lists.
    pub fn max_models_len(&self) -> usize {
        self.routes
            .iter()
            .map(|route| {
                if route.models.is_empty() {
                    // Usable targets without a list still preview their
                    // default model (see `models()`).
                    usize::from(!route.model.is_empty())
                } else {
                    route.models.len()
                }
            })
            .max()
            .unwrap_or(0)
    }

    /// Rows in the two-column body — tall enough for every target and for
    /// the largest model list any target offers, so switching the left
    /// column does not resize the popup.
    pub fn body_rows(&self) -> usize {
        self.routes.len().max(self.max_models_len())
    }

    pub fn rows(&self) -> usize {
        self.header_rows() as usize + self.body_rows()
    }

    /// Wrap the description to `width`, capped at two lines so the menu
    /// stays a menu.
    pub fn description(&self, width: u16) -> Vec<String> {
        let width = width.max(8) as usize;
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        for word in ROUTE_DESCRIPTION.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if candidate.chars().count() > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
            if lines.len() == 2 {
                break;
            }
        }
        if lines.len() < 2 && !line.is_empty() {
            lines.push(line);
        }
        lines
    }

    pub fn target_label(&self, index: usize) -> String {
        match self.target_at(index) {
            Some(r) => r.name.clone(),
            // Not "pass through": from the user's side this is simply the
            // session's own model, unrouted.
            None if index == 0 => "Default".to_string(),
            None => String::new(),
        }
    }

    pub fn target_enabled(&self, index: usize) -> bool {
        match self.target_at(index) {
            Some(r) => r.unavailable_reason.is_none(),
            None => index == 0,
        }
    }

    pub fn target_is_active(&self, index: usize) -> bool {
        match self.target_at(index) {
            Some(r) => Some(&r.name) == self.active.as_ref(),
            None => index == 0 && self.active.is_none(),
        }
    }

    /// Whether this target has a model column to move into.
    pub fn target_descends(&self, index: usize) -> bool {
        index > 0 && self.target_enabled(index)
    }

    pub fn model_is_active(&self, index: usize) -> bool {
        let Some(route) = self.focused_target() else {
            return false;
        };
        self.target_is_active(self.selected)
            && self.models().get(index).is_some_and(|m| *m == route.model)
    }

    /// Row of the model this target is currently armed on, so moving the
    /// highlight lands on the live choice rather than the top of the list.
    pub fn active_model_index(&self) -> usize {
        let models = self.models();
        self.focused_target()
            .and_then(|r| models.iter().position(|m| *m == r.model))
            .unwrap_or(0)
    }

    /// Reason the highlighted target cannot be used, shown in the model
    /// column where its models would otherwise be.
    pub fn focused_blocker(&self) -> Option<&str> {
        self.focused_target()?.unavailable_reason.as_deref()
    }

    /// Place the menu above its anchor when there isn't room below — the
    /// modeline sits at the bottom of the frame, so downward is almost
    /// never available.
    pub fn anchored(mut self, size: ratatui::layout::Rect) -> Self {
        let targets_w = (0..self.target_rows())
            .map(|i| self.target_label(i).chars().count())
            .max()
            .unwrap_or(0)
            // marker, spaces, chevron
            .saturating_add(6) as u16;
        // The model column is sized for the widest model any target
        // offers, so the layout does not jump as the highlight moves.
        let models_w = self
            .routes
            .iter()
            .flat_map(|r| r.models.iter().map(|m| m.chars().count()))
            .chain(self.routes.iter().map(|r| r.model.chars().count()))
            .max()
            .unwrap_or(0)
            .max(self.focused_blocker().map(|b| b.chars().count()).unwrap_or(0).min(40))
            .saturating_add(4) as u16;

        let max_w = size.width.saturating_sub(2).max(12);
        let mut target_col_w = targets_w;
        // The session-level reason spans both columns, so it gets a say in
        // the width — otherwise the one message explaining why nothing can
        // be routed is the thing that gets truncated.
        let reason_w = self
            .unavailable_reason
            .as_deref()
            .map(|r| r.chars().count() + 3)
            .unwrap_or(0) as u16;
        // The description spans both columns, so it argues for width too —
        // two cramped lines read worse than one clear one.
        let mut width = targets_w
            .saturating_add(models_w)
            .saturating_add(3)
            .max(reason_w)
            .max(46);
        if width > max_w {
            // Give the target column its share first: the model column can
            // truncate a long id more gracefully than a target name.
            width = max_w;
            target_col_w = targets_w.min(width.saturating_sub(8));
        }
        self.target_col_w = target_col_w;

        self.desc_lines = self.description(width.saturating_sub(2)).len() as u16;
        let reason_rows = if self.unavailable_reason.is_some() { 1 } else { 0 };
        // Cap popup height independently of content so a huge model catalog
        // cannot cover the whole frame; content still sizes up to this.
        const ROUTE_MENU_MAX_HEIGHT: u16 = 18;
        let height = (self.rows() as u16)
            .saturating_add(2)
            .saturating_add(reason_rows)
            .min(ROUTE_MENU_MAX_HEIGHT)
            .min(size.height.max(3));
        let (col, row) = self.anchor;
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
        // Visible body = total height minus borders, header, optional reason.
        let chrome = 2u16.saturating_add(self.header_rows()).saturating_add(reason_rows);
        self.visible_body_rows = height.saturating_sub(chrome) as usize;
        // Clamp scroll so a shrink (frame resize, re-anchor) does not leave
        // the viewport past the end of either list.
        let targets = self.routes.len();
        let models = self.models().len();
        let vis = self.visible_body_rows;
        self.target_scroll = self.target_scroll.min(targets.saturating_sub(vis));
        self.model_scroll = self.model_scroll.min(models.saturating_sub(vis));
        self
    }

    /// Keep a body-list selection inside the scrolled viewport.
    pub fn ensure_target_visible(&mut self) {
        // selected 0 is Default (above the body); body indices are selected-1.
        if self.selected == 0 {
            return;
        }
        let body_idx = self.selected - 1;
        let len = self.routes.len();
        let vis = self.visible_body_rows.max(1);
        if body_idx < self.target_scroll {
            self.target_scroll = body_idx;
        } else if body_idx >= self.target_scroll + vis {
            self.target_scroll = body_idx + 1 - vis;
        }
        self.target_scroll = self.target_scroll.min(len.saturating_sub(vis));
    }

    pub fn ensure_model_visible(&mut self) {
        let len = self.models().len();
        let vis = self.visible_body_rows.max(1);
        if self.model_selected < self.model_scroll {
            self.model_scroll = self.model_selected;
        } else if self.model_selected >= self.model_scroll + vis {
            self.model_scroll = self.model_selected + 1 - vis;
        }
        self.model_scroll = self.model_scroll.min(len.saturating_sub(vis));
    }

    /// Scroll the focused column by `delta` rows. Returns true when the
    /// event was consumed (menu open and the wheel is for it).
    pub fn scroll_focused(&mut self, delta: isize) -> bool {
        let vis = self.visible_body_rows;
        if vis == 0 {
            return true;
        }
        match self.focus {
            RouteFocus::Targets => {
                let len = self.routes.len();
                let max = len.saturating_sub(vis);
                if max == 0 {
                    return true;
                }
                let next = (self.target_scroll as isize + delta).clamp(0, max as isize) as usize;
                self.target_scroll = next;
            }
            RouteFocus::Models => {
                let len = self.models().len();
                let max = len.saturating_sub(vis);
                if max == 0 {
                    return true;
                }
                let next = (self.model_scroll as isize + delta).clamp(0, max as isize) as usize;
                self.model_scroll = next;
            }
        }
        true
    }

    /// Scroll the column under `(col, row)`. Falls back to the focused
    /// column when the pointer is over chrome (header / border).
    pub fn scroll_at(&mut self, col: u16, row: u16, delta: isize) -> bool {
        if !self.contains(col, row) {
            return false;
        }
        let first = self.area.y.saturating_add(1);
        let body_start = first.saturating_add(self.header_rows());
        let last = self.area.y.saturating_add(self.area.height).saturating_sub(1);
        if row >= body_start && row < last {
            let divider = self.area.x.saturating_add(1).saturating_add(self.target_col_w);
            if col <= divider {
                // Temporarily treat as targets for this wheel.
                let prev = self.focus;
                self.focus = RouteFocus::Targets;
                self.scroll_focused(delta);
                self.focus = prev;
                return true;
            }
            let prev = self.focus;
            self.focus = RouteFocus::Models;
            self.scroll_focused(delta);
            self.focus = prev;
            return true;
        }
        self.scroll_focused(delta)
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.area.x
            && col < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }

    /// Which column and row a click landed on. A click in the target
    /// column selects a target; one in the model column commits it.
    pub fn hit_at(&self, col: u16, row: u16) -> Option<RouteHit> {
        if !self.contains(col, row) {
            return None;
        }
        let first = self.area.y.saturating_add(1);
        // Default is its own row above the rule; the rule and description
        // are not selectable.
        if row == first {
            return Some(RouteHit::Target(0));
        }
        let body_start = first.saturating_add(self.header_rows());
        let visible = row.checked_sub(body_start)? as usize;
        if self.visible_body_rows > 0 && visible >= self.visible_body_rows {
            return None;
        }
        let divider = self.area.x.saturating_add(1).saturating_add(self.target_col_w);
        if col <= divider {
            let index = visible + self.target_scroll;
            (index < self.routes.len()).then_some(RouteHit::Target(index + 1))
        } else {
            let index = visible + self.model_scroll;
            (index < self.models().len()).then_some(RouteHit::Model(index))
        }
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
        let mut m = RouteMenu {
            session_id: "s1".into(),
            area: ratatui::layout::Rect::default(),
            routes,
            unavailable_reason: None,
            active: active.map(str::to_string),
            focus: RouteFocus::Targets,
            selected: 0,
            model_selected: 0,
            target_scroll: 0,
            model_scroll: 0,
            anchor: (40, 23),
            target_col_w: 0,
            desc_lines: 0,
            visible_body_rows: 0,
        };
        m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 30));
        m
    }

    #[test]
    fn the_first_target_is_default_and_always_selectable() {
        let m = menu(vec![option("kimi", &["kimi-k2.5"], Some("no key"))], None);
        assert_eq!(m.target_label(0), "Default");
        assert!(m.target_enabled(0));
        assert!(!m.target_descends(0), "Default has no models to move into");
        assert!(!m.target_enabled(1), "an unusable target is not selectable");
        assert!(m.target_is_active(0), "no route armed means Default is current");
    }

    /// The model column previews the highlighted target without committing
    /// to it — that is the point of showing both at once.
    #[test]
    fn the_model_column_follows_the_highlighted_target() {
        let mut m = menu(
            vec![
                option("claude-oauth", &["sonnet", "opus"], None),
                option("codex-oauth", &["gpt-5.6-sol", "gpt-5.5"], None),
            ],
            None,
        );
        // Default highlighted: nothing to preview.
        assert!(m.models().is_empty());

        m.selected = 1;
        assert_eq!(m.models(), vec!["sonnet", "opus"]);
        m.selected = 2;
        assert_eq!(m.models(), vec!["gpt-5.6-sol", "gpt-5.5"]);
    }

    #[test]
    fn the_body_spans_the_taller_of_the_two_columns() {
        let mut m = menu(
            vec![option("codex-oauth", &["a", "b", "c", "d"], None)],
            None,
        );
        assert_eq!(m.target_rows(), 2, "Default plus one target");
        m.selected = 1;
        assert_eq!(m.body_rows(), 4, "the model column is taller here");
        assert_eq!(m.rows(), m.header_rows() as usize + 4);
    }

    /// Body height is the max model list across every target, not only the
    /// highlighted one — otherwise the popup jumps when the left column
    /// moves between short and long lists.
    #[test]
    fn body_height_uses_the_tallest_model_list_across_targets() {
        let mut m = menu(
            vec![
                option("short", &["a"], None),
                option("long", &["w", "x", "y", "z"], None),
            ],
            None,
        );
        // Default highlighted: no models for the focus, but body still
        // reserves room for the tallest target's list.
        assert_eq!(m.selected, 0);
        assert!(m.models().is_empty());
        assert_eq!(m.body_rows(), 4);
        let frame = ratatui::layout::Rect::new(0, 0, 120, 30);
        let h_default = m.clone().anchored(frame).area.height;

        m.selected = 1;
        assert_eq!(m.models().len(), 1);
        assert_eq!(m.body_rows(), 4);
        let h_short = m.clone().anchored(frame).area.height;

        m.selected = 2;
        assert_eq!(m.models().len(), 4);
        assert_eq!(m.body_rows(), 4);
        let h_long = m.anchored(frame).area.height;

        assert_eq!(h_default, h_short);
        assert_eq!(h_short, h_long);
    }

    /// A target that cannot be used shows why, in place of models.
    #[test]
    fn an_unusable_target_shows_its_reason_instead_of_models() {
        let mut m = menu(vec![option("glm", &["glm-5"], Some("GLM_API_KEY is not set"))], None);
        m.selected = 1;
        assert_eq!(m.focused_blocker(), Some("GLM_API_KEY is not set"));
    }

    #[test]
    fn the_armed_model_is_marked_and_preselected() {
        let mut route = option("codex-oauth", &["gpt-5.6-sol", "gpt-5.5"], None);
        route.model = "gpt-5.5".into();
        let mut m = menu(vec![route], Some("codex-oauth"));
        m.selected = 1;
        assert_eq!(m.active_model_index(), 1, "lands on the live choice");
        assert!(m.model_is_active(1));
        assert!(!m.model_is_active(0));
    }

    /// A target that reports no model list still previews its default, so
    /// the right column never sits empty for a usable target.
    #[test]
    fn a_target_without_a_model_list_still_previews_its_default() {
        let mut route = option("bare", &[], None);
        route.model = "some-model".into();
        route.models.clear();
        let mut m = menu(vec![route], None);
        m.selected = 1;
        assert_eq!(m.models(), vec!["some-model"]);
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

    /// Which column a click lands in decides what it means: the left
    /// selects, the right commits.
    #[test]
    fn clicks_are_routed_to_the_column_they_land_in() {
        let mut m = menu(vec![option("kimi", &["kimi-k2.5", "kimi-k2"], None)], None);
        m.selected = 1;
        let m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 30));
        let first = m.area.y + 1;
        let body = first + m.header_rows();
        let divider = m.area.x + 1 + m.target_col_w;

        // Default owns its own row above the rule.
        assert_eq!(m.hit_at(m.area.x + 2, first), Some(RouteHit::Target(0)));
        // The rule and description are not selectable.
        assert_eq!(m.hit_at(m.area.x + 2, first + 1), None, "the rule");
        assert_eq!(m.hit_at(m.area.x + 2, first + 2), None, "the description");
        // Targets and models sit side by side below them.
        assert_eq!(m.hit_at(m.area.x + 2, body), Some(RouteHit::Target(1)));
        assert_eq!(m.hit_at(divider + 2, body), Some(RouteHit::Model(0)));
        assert_eq!(m.hit_at(divider + 2, body + 1), Some(RouteHit::Model(1)));
        assert_eq!(m.hit_at(m.area.x + 2, m.area.y), None, "top border");
        assert_eq!(m.hit_at(m.area.x.saturating_sub(1), first), None);
    }

    /// Default is separated from the targets by a rule and one line about
    /// what those targets do — it is the absence of a route, not one of
    /// them.
    #[test]
    fn a_rule_and_a_description_separate_default_from_the_targets() {
        let m = menu(vec![option("kimi", &["kimi-k2.5"], None)], None);
        assert_eq!(m.header_rows(), 2 + m.desc_lines);
        assert!(m.desc_lines >= 1, "the description occupies real rows");
        assert_eq!(
            m.rows(),
            m.header_rows() as usize + m.body_rows(),
            "the header is counted in the height"
        );
    }

    #[test]
    fn the_description_wraps_to_at_most_two_lines() {
        let m = menu(vec![option("kimi", &["kimi-k2.5"], None)], None);
        for width in [20u16, 30, 46, 80] {
            let lines = m.description(width);
            assert!(!lines.is_empty(), "width {width} produced nothing");
            assert!(lines.len() <= 2, "width {width} wrapped to {lines:?}");
            for line in &lines {
                assert!(
                    line.chars().count() <= width.max(8) as usize,
                    "width {width}: {line:?} overflows"
                );
            }
        }
    }

    #[test]
    fn scrolled_hit_testing_maps_visible_rows_to_absolute_indices() {
        // Long model list so the body is taller than the height cap.
        let labels: Vec<String> = (0..30).map(|i| format!("m{i}")).collect();
        let mut route = option("long", &["x"], None);
        route.models = labels;
        let mut m = menu(vec![route], None);
        m.selected = 1;
        // Short frame forces a small visible body so scrolling is possible.
        m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 16));
        assert!(m.visible_body_rows > 0);
        assert!(
            m.models().len() > m.visible_body_rows,
            "need overflow to exercise scroll"
        );
        m.model_scroll = 3;
        m.target_scroll = 0;
        let first = m.area.y + 1;
        let body = first + m.header_rows();
        let divider = m.area.x + 1 + m.target_col_w;
        // Top visible model row is absolute index 3.
        assert_eq!(
            m.hit_at(divider + 2, body),
            Some(RouteHit::Model(3))
        );
        // Wheel over the model column advances model_scroll.
        assert!(m.scroll_at(divider + 2, body, 1));
        assert_eq!(m.model_scroll, 4);
    }

    /// The model column is sized for the widest model any target offers,
    /// so the layout does not jump as the highlight moves.
    #[test]
    fn the_layout_does_not_resize_as_the_highlight_moves() {
        let mut m = menu(
            vec![
                option("short", &["a"], None),
                option("long", &["a-very-long-model-identifier"], None),
            ],
            None,
        );
        let width_at_default = m.area.width;
        m.selected = 1;
        let m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 30));
        assert_eq!(m.area.width, width_at_default);
    }
}
