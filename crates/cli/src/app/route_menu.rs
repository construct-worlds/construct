use super::*;

/// The modeline's model indicator is the affordance for routing: clicking
/// it opens this menu (spec 0114 — the substitution is shown where the
/// model is shown, so the two are never confused).
///
/// Two steps, because a target and a model are separate choices: pick where
/// the traffic goes, then which model to ask for. `No redirect` is the
/// first row of the first step and needs no second one — it is the absence
/// of a route, not a target with a model.
impl App {
    pub(super) async fn open_route_menu(&mut self, session_id: String, col: u16, row: u16) {
        let listed = match self.client.list_routes(Some(&session_id)).await {
            Ok(l) => l,
            Err(e) => {
                self.set_status(format!("redirect unavailable: {e}"));
                return;
            }
        };
        // One floating surface at a time.
        self.fleet_panel = None;
        let active = listed.active.clone();
        // Live pin model/effort come from the session record, not the
        // route option defaults (list_routes does not rewrite those).
        let (active_model, active_effort) = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .and_then(|s| s.route.as_ref())
            .map(|r| (Some(r.model.clone()), r.effort.clone()))
            .unwrap_or((None, None));
        let native = self.session_native_selection(&session_id);
        // Open on the armed target so the current state is where the eye
        // already is; with no pin, a live native pick is the current state.
        let selected = listed
            .routes
            .iter()
            .position(|r| Some(&r.name) == active.as_ref())
            .or_else(|| {
                let (route, _) = native.as_ref()?;
                listed.routes.iter().position(|r| &r.name == route)
            })
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut menu = RouteMenu {
            session_id,
            area: ratatui::layout::Rect::default(),
            routes: listed.routes,
            unavailable_reason: listed.unavailable_reason,
            active,
            active_model,
            active_effort,
            native,
            focus: RouteFocus::Targets,
            selected,
            model_selected: 0,
            effort_selected: 0,
            target_scroll: 0,
            model_scroll: 0,
            effort_scroll: 0,
            anchor: (col, row),
            target_col_w: 0,
            model_col_w: 0,
            desc_lines: 0,
            visible_body_rows: 0,
        };
        menu.model_selected = menu.active_model_index();
        menu.effort_selected = menu.active_effort_index();
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

    /// A click: target column selects and previews; model column selects
    /// (and commits when there is no effort scale); effort column commits.
    pub(super) async fn hit_route_menu(&mut self, hit: RouteHit) {
        match hit {
            RouteHit::Target(index) => self.select_route_target(index).await,
            RouteHit::Model(index) => self.select_or_arm_route_model(index).await,
            RouteHit::Effort(index) => self.arm_route_effort(index).await,
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
            self.apply_route(menu.session_id, None, None, None).await;
            return;
        }
        menu.selected = index;
        menu.model_selected = menu.active_model_index();
        menu.effort_selected = menu.active_effort_index();
        menu.model_scroll = 0;
        menu.effort_scroll = 0;
        menu.focus = RouteFocus::Targets;
        menu.ensure_target_visible();
        self.replace_route_menu(menu);
    }

    /// Model column: when the model has a real effort scale, preview it and
    /// move focus to the effort column; otherwise arm immediately.
    async fn select_or_arm_route_model(&mut self, index: usize) {
        let Some(mut menu) = self.route_menu.clone() else {
            return;
        };
        let Some(route) = menu.focused_target().cloned() else {
            return;
        };
        if let Some(reason) = route.unavailable_reason.as_deref() {
            // A login blocker is fixable in one step, so activating the
            // blocked target starts that step instead of restating it.
            if let Some(cmd) = route.login_command.clone() {
                let name = route.name.clone();
                self.route_menu = None;
                self.start_route_login(name, cmd).await;
                return;
            }
            self.set_status(format!("{}: {reason}", route.name));
            return;
        }
        let Some(model) = menu.models().get(index).cloned() else {
            return;
        };
        menu.model_selected = index;
        menu.effort_selected = menu.active_effort_index();
        menu.effort_scroll = 0;
        if menu.model_descends() {
            menu.focus = RouteFocus::Efforts;
            menu.ensure_effort_visible();
            self.replace_route_menu(menu);
            return;
        }
        let session = menu.session_id.clone();
        self.route_menu = None;
        self.apply_route(session, Some(route.name), Some(model), None)
            .await;
    }

    async fn arm_route_effort(&mut self, index: usize) {
        let Some(menu) = self.route_menu.clone() else {
            return;
        };
        let Some(route) = menu.focused_target() else {
            return;
        };
        if route.unavailable_reason.is_some() {
            return;
        }
        let Some(model) = menu.models().get(menu.model_selected).cloned() else {
            return;
        };
        let efforts = menu.efforts_for_selected_model();
        let Some(effort) = efforts.get(index).cloned() else {
            return;
        };
        let (session, name) = (menu.session_id.clone(), route.name.clone());
        self.route_menu = None;
        self.apply_route(session, Some(name), Some(model), Some(effort))
            .await;
    }

    /// Enter: targets → models → efforts (when present) → arm.
    pub(super) async fn activate_route_menu(&mut self) {
        let Some(menu) = self.route_menu.clone() else {
            return;
        };
        match menu.focus {
            RouteFocus::Targets => {
                if menu.selected == 0 {
                    self.select_route_target(0).await;
                } else if let Some(cmd) = menu.focused_login_command() {
                    // Enter on a target blocked only by a login starts the
                    // sign-in instead of dead-ending on a disabled row.
                    let name = menu
                        .focused_target()
                        .map(|r| r.name.clone())
                        .unwrap_or_default();
                    self.route_menu = None;
                    self.start_route_login(name, cmd).await;
                } else {
                    self.route_menu_focus_models();
                }
            }
            RouteFocus::Models => {
                self.select_or_arm_route_model(menu.model_selected).await;
            }
            RouteFocus::Efforts => self.arm_route_effort(menu.effort_selected).await,
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

    /// Right from models steps into efforts when the model has a scale.
    pub(super) fn route_menu_focus_efforts(&mut self) {
        let Some(menu) = self.route_menu.as_mut() else {
            return;
        };
        if menu.focus == RouteFocus::Models && menu.model_descends() {
            menu.focus = RouteFocus::Efforts;
            menu.effort_selected = menu.active_effort_index();
            menu.ensure_effort_visible();
        }
    }

    /// Left: efforts → models → targets → close.
    pub(super) fn route_menu_back(&mut self) {
        let Some(menu) = self.route_menu.as_mut() else {
            return;
        };
        match menu.focus {
            RouteFocus::Efforts => menu.focus = RouteFocus::Models,
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
                menu.effort_selected = menu.active_effort_index();
                menu.model_scroll = 0;
                menu.effort_scroll = 0;
                menu.ensure_target_visible();
            }
            RouteFocus::Models => {
                let len = menu.models().len();
                if len == 0 {
                    return;
                }
                menu.model_selected =
                    (menu.model_selected as isize + delta).rem_euclid(len as isize) as usize;
                menu.effort_selected = menu.active_effort_index();
                menu.effort_scroll = 0;
                menu.ensure_model_visible();
            }
            RouteFocus::Efforts => {
                let len = menu.efforts_for_selected_model().len();
                if len == 0 {
                    return;
                }
                menu.effort_selected =
                    (menu.effort_selected as isize + delta).rem_euclid(len as isize) as usize;
                menu.ensure_effort_visible();
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

    /// Sign in to a blocked subscription target by asking the daemon to
    /// run the owning CLI's login command in a new shell session
    /// (spec 0117: the owning tool stays the credential's only writer —
    /// Construct just makes reaching for it one keypress). The session is
    /// selected so the user lands in the login flow; the CLI opens its
    /// own browser page, and the daemon archives the session on its own
    /// the moment the credential lands.
    async fn start_route_login(&mut self, route: String, command: String) {
        let (cols, rows) = self.active_pane_size();
        let pty = construct_protocol::PtySize {
            cols: cols.max(20),
            rows: rows.max(5),
        };
        match self.client.router_login(&route, Some(pty)).await {
            Ok(id) => {
                if !self.histories.contains_key(&id) {
                    self.histories
                        .insert(id.clone(), crate::pty_render::ItemHistory::new());
                }
                self.select_created_session(id);
                self.sync_active_window_selection();
                self.focus = PaneFocus::View;
                self.set_status(format!(
                    "running `{command}` — sign in there; the session closes itself once the login lands"
                ));
            }
            Err(e) => self.set_status(format!("could not start `{command}`: {e}")),
        }
    }

    /// The route/model the session's harness itself picked from a
    /// Construct-published native catalog entry, decoded from the model it
    /// reports (spec 0157/0158). `None` when it is on a native model.
    pub(super) fn session_native_selection(&self, session_id: &str) -> Option<(String, String)> {
        let session = self.sessions.iter().find(|s| s.id == session_id)?;
        native_selection(session.model.as_deref())
    }

    async fn apply_route(
        &mut self,
        session_id: String,
        route: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    ) {
        // Status labels lead with the model, matching how a decoded native
        // pick reads everywhere else (`model · route`, spec 0158).
        let label = match (&route, &model, &effort) {
            (Some(r), Some(m), Some(e)) => format!("{m} ({e}) · {r}"),
            (Some(r), Some(m), None) => format!("{m} · {r}"),
            (Some(r), None, _) => r.clone(),
            (None, _, _) => String::new(),
        };
        let clearing = label.is_empty();
        // A redirect only applies to requests carrying a native model id.
        // While the harness is on a Construct catalog entry, every request
        // names its own route, so an armed redirect would otherwise appear
        // to do nothing (spec 0158): say so instead of reporting a silent
        // no-op.
        let inert = route
            .is_some()
            .then(|| self.session_native_selection(&session_id))
            .flatten();
        match self
            .client
            .set_route(&session_id, route, model, effort)
            .await
        {
            Ok(()) => match inert {
                Some((native_route, native_model)) => self.set_status(format!(
                    "redirect armed: {label} — idle while the harness addresses {native_model} · {native_route} directly"
                )),
                None if clearing => self.set_status("redirect off".to_string()),
                None => self.set_status(format!("redirecting to {label}")),
            },
            Err(e) => self.set_status(format!("redirect failed: {e}")),
        }
    }
}

/// Decode a harness-reported model that is a Construct-published catalog
/// id into its `(route, model)` pair (spec 0157). `None` for native model
/// ids and for malformed ids — display falls back to the raw value; the
/// proxy is where malformed ids fail closed.
pub fn native_selection(model: Option<&str>) -> Option<(String, String)> {
    construct_protocol::published_model::decode_published_model_id(model?)
        .ok()
        .flatten()
}

/// Leading text for a login blocker's model-column row — the clickable
/// word `login` is appended by the renderer. Expiry is the common case
/// that used to bury the action in a multi-clause reason string.
pub fn login_blocker_prefix(reason: &str) -> &'static str {
    if reason.to_ascii_lowercase().contains("expired") {
        "token expired. click here to "
    } else {
        "not logged in. click here to "
    }
}

/// One line on what a redirect does, shown once between "No redirect" and
/// the targets it contrasts with. Short on purpose: the picker is a menu,
/// not documentation, and the sentence has to earn its two rows — it must
/// fit two lines at the menu's minimum width, so the last word is never
/// silently dropped by the two-line cap.
pub const ROUTE_DESCRIPTION: &str =
    "Redirect model requests to another provider — transparent to the harness.";

/// Which column the keyboard is driving. Columns are always visible when
/// they have content; focus only decides what moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteFocus {
    Targets,
    Models,
    Efforts,
}

/// Where a click landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteHit {
    Target(usize),
    Model(usize),
    Effort(usize),
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
    /// Model currently armed on the pin, when a pin is active. Used to
    /// preselect the model column; `RouteOption::model` is only the default.
    pub active_model: Option<String>,
    /// Effort currently armed on the pin, when a pin chose one.
    pub active_effort: Option<String>,
    /// The `(route, model)` the harness itself is on right now via a
    /// Construct-published native catalog entry (spec 0157), decoded from
    /// the model it reports. Requests carrying that id select their own
    /// route, so while this is `Some` an armed pin is inert (spec 0158).
    pub native: Option<(String, String)>,
    pub focus: RouteFocus,
    /// Highlighted target row; row 0 is Default.
    pub selected: usize,
    /// Highlighted model row within the highlighted target.
    pub model_selected: usize,
    /// Highlighted effort row for the highlighted model.
    pub effort_selected: usize,
    /// First visible body row of the target list (Default is above the body
    /// and never scrolls).
    pub target_scroll: usize,
    /// First visible body row of the model list for the focused target.
    pub model_scroll: usize,
    /// First visible body row of the effort list for the focused model.
    pub effort_scroll: usize,
    anchor: (u16, u16),
    /// Width of the target column, including its padding. Set when the
    /// menu is placed so render and hit-testing agree on one number.
    pub target_col_w: u16,
    /// Width of the model column (between the two dividers).
    pub model_col_w: u16,
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

    /// Model string under the model highlight (empty when Default / no models).
    pub fn selected_model(&self) -> Option<String> {
        self.models().get(self.model_selected).cloned()
    }

    /// Effort levels for the currently highlighted model. Empty when the
    /// target has no real scale — the third column is omitted then.
    pub fn efforts_for_selected_model(&self) -> Vec<String> {
        let Some(model) = self.selected_model() else {
            return Vec::new();
        };
        self.efforts_for_model(&model)
    }

    pub fn efforts_for_model(&self, model: &str) -> Vec<String> {
        let Some(route) = self.focused_target() else {
            return Vec::new();
        };
        route.efforts.get(model).cloned().unwrap_or_default()
    }

    /// Whether any model on any target offers a multi-level effort scale —
    /// used so the menu reserves a third column width without jumping.
    pub fn any_effort_column(&self) -> bool {
        self.routes
            .iter()
            .any(|r| r.efforts.values().any(|levels| levels.len() > 1))
    }

    /// Tallest effort list any model on the focused target (or any target
    /// when sizing height) can show.
    pub fn max_efforts_len(&self) -> usize {
        self.routes
            .iter()
            .flat_map(|r| r.efforts.values().map(|v| v.len()))
            .max()
            .unwrap_or(0)
    }

    /// Rows above the two columns: "No redirect", a rule, and the
    /// description. It sits apart because it is the absence of a route,
    /// not one of the targets listed under it.
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

    /// Rows in the multi-column body — tall enough for every target, for
    /// the largest model list any target offers, the tallest effort list,
    /// and for a login-blocker action line, so switching columns does not
    /// resize the popup.
    pub fn body_rows(&self) -> usize {
        let blocker_rows = if self
            .routes
            .iter()
            .any(|r| r.unavailable_reason.is_some() && r.login_command.is_some())
        {
            // One short line: "token expired. click here to login".
            1
        } else {
            0
        };
        self.routes
            .len()
            .max(self.max_models_len())
            .max(self.max_efforts_len())
            .max(blocker_rows)
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
            // Named for its effect, not "pass through" or "default": from
            // the user's side this row means requests go where the harness
            // addresses them, unredirected.
            None if index == 0 => "No redirect".to_string(),
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
            // Default is only truthfully "current" when nothing overrides
            // pass-through: neither a pin nor a live native pick.
            None => index == 0 && self.active.is_none() && self.native.is_none(),
        }
    }

    /// Whether this target is the harness's own live pick from the native
    /// catalog — marked distinctly from the pin, because it is per-request
    /// state the harness owns rather than a pin this menu arms.
    pub fn target_is_native(&self, index: usize) -> bool {
        match (self.target_at(index), &self.native) {
            (Some(r), Some((route, _))) => &r.name == route,
            _ => false,
        }
    }

    pub fn model_is_native(&self, index: usize) -> bool {
        let Some((_, model)) = &self.native else {
            return false;
        };
        self.target_is_native(self.selected) && self.models().get(index).is_some_and(|m| m == model)
    }

    /// One line explaining the native marker, shown under the body while
    /// the harness is on a Construct catalog entry. Carries the decoded
    /// pair so the state is legible even when its route is not in the
    /// target list anymore.
    pub fn native_note(&self) -> Option<String> {
        let (route, model) = self.native.as_ref()?;
        Some(format!(
            "» {model} · {route} — picked in the harness's own model picker"
        ))
    }

    /// Whether this target has a model column to move into.
    pub fn target_descends(&self, index: usize) -> bool {
        index > 0 && self.target_enabled(index)
    }

    /// Whether the highlighted model has an effort column to move into.
    pub fn model_descends(&self) -> bool {
        self.efforts_for_selected_model().len() > 1
    }

    pub fn model_is_active(&self, index: usize) -> bool {
        if !self.target_is_active(self.selected) {
            return false;
        }
        let Some(active) = self.active_model.as_deref() else {
            // Fall back to the route option default when the pin model is
            // unknown (e.g. unit tests that only set `active`).
            let Some(route) = self.focused_target() else {
                return false;
            };
            return self.models().get(index).is_some_and(|m| *m == route.model);
        };
        self.models().get(index).is_some_and(|m| m == active)
    }

    /// Row of the model this target is currently armed on, so moving the
    /// highlight lands on the live choice rather than the top of the list.
    pub fn active_model_index(&self) -> usize {
        let models = self.models();
        if self.target_is_active(self.selected) {
            if let Some(active) = self.active_model.as_deref() {
                if let Some(i) = models.iter().position(|m| m == active) {
                    return i;
                }
            }
        }
        self.focused_target()
            .and_then(|r| models.iter().position(|m| *m == r.model))
            .unwrap_or(0)
    }

    pub fn effort_is_active(&self, index: usize) -> bool {
        if !self.model_is_active(self.model_selected) {
            return false;
        }
        let Some(active) = self.active_effort.as_deref() else {
            return false;
        };
        self.efforts_for_selected_model()
            .get(index)
            .is_some_and(|e| e == active)
    }

    /// Row of the effort currently armed on the pin for the selected model.
    pub fn active_effort_index(&self) -> usize {
        let efforts = self.efforts_for_selected_model();
        if self.model_is_active(self.model_selected) {
            if let Some(active) = self.active_effort.as_deref() {
                if let Some(i) = efforts.iter().position(|e| e == active) {
                    return i;
                }
            }
        }
        0
    }

    /// Reason the highlighted target cannot be used, shown in the model
    /// column where its models would otherwise be.
    pub fn focused_blocker(&self) -> Option<&str> {
        self.focused_target()?.unavailable_reason.as_deref()
    }

    /// The sign-in command to offer for the highlighted target — present
    /// only when the target is blocked *and* the blocker is a login the
    /// user can complete (spec 0117), never for key/dialect blockers.
    pub fn focused_login_command(&self) -> Option<String> {
        let route = self.focused_target()?;
        route.unavailable_reason.as_ref()?;
        route.login_command.clone()
    }

    /// Short prose that precedes the clickable `login` word in the model
    /// column for a login blocker. The daemon's full reason is kept for
    /// status messages; the menu only needs a one-line state + action.
    pub fn focused_login_blocker_prefix(&self) -> Option<&'static str> {
        let reason = self.focused_blocker()?;
        self.focused_login_command()?;
        Some(login_blocker_prefix(reason))
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
        let blocker_w = self
            .focused_login_blocker_prefix()
            .map(|p| p.chars().count() + "login".len())
            .or_else(|| self.focused_blocker().map(|b| b.chars().count().min(40)))
            .unwrap_or(0);
        let models_w = self
            .routes
            .iter()
            .flat_map(|r| r.models.iter().map(|m| m.chars().count()))
            .chain(self.routes.iter().map(|r| r.model.chars().count()))
            .max()
            .unwrap_or(0)
            .max(blocker_w)
            .saturating_add(4) as u16;
        // Effort column only when some target advertises a real scale.
        let efforts_w = if self.any_effort_column() {
            self.routes
                .iter()
                .flat_map(|r| r.efforts.values().flatten().map(|e| e.chars().count()))
                .max()
                .unwrap_or(0)
                .max(6) // "medium"
                .saturating_add(4) as u16
        } else {
            0
        };
        let dividers = if efforts_w > 0 { 2u16 } else { 1 };

        let max_w = size.width.saturating_sub(2).max(12);
        let mut target_col_w = targets_w;
        let mut model_col_w = models_w;
        // The session-level reason spans all columns, so it gets a say in
        // the width — otherwise the one message explaining why nothing can
        // be routed is the thing that gets truncated.
        let reason_w = self
            .unavailable_reason
            .as_deref()
            .map(|r| r.chars().count() + 3)
            .unwrap_or(0) as u16;
        // The native-pick note spans columns too, but a long decoded id
        // must not balloon the popup: it argues up to a cap and then
        // truncates like any other row.
        let native_w = self
            .native_note()
            .map(|n| n.chars().count().min(56) + 3)
            .unwrap_or(0) as u16;
        // The description spans columns, so it argues for width too —
        // two cramped lines read worse than one clear one.
        let mut width = targets_w
            .saturating_add(models_w)
            .saturating_add(efforts_w)
            .saturating_add(dividers)
            .saturating_add(2)
            .max(reason_w)
            .max(native_w)
            .max(if efforts_w > 0 { 56 } else { 46 });
        if width > max_w {
            // Give the target column its share first: model/effort can
            // truncate a long id more gracefully than a target name.
            width = max_w;
            let min_rest = 8u16.saturating_add(if efforts_w > 0 { 8 } else { 0 });
            target_col_w = targets_w.min(width.saturating_sub(min_rest));
            let rest = width
                .saturating_sub(target_col_w)
                .saturating_sub(dividers);
            if efforts_w > 0 {
                model_col_w = models_w.min(rest.saturating_mul(2) / 3).max(8);
            } else {
                model_col_w = rest;
            }
        }
        self.target_col_w = target_col_w;
        self.model_col_w = model_col_w;

        self.desc_lines = self.description(width.saturating_sub(2)).len() as u16;
        let reason_rows = if self.unavailable_reason.is_some() {
            1
        } else {
            0
        };
        let native_rows = if self.native.is_some() { 1 } else { 0 };
        // Cap popup height independently of content so a huge model catalog
        // cannot cover the whole frame; content still sizes up to this.
        const ROUTE_MENU_MAX_HEIGHT: u16 = 18;
        let height = (self.rows() as u16)
            .saturating_add(2)
            .saturating_add(reason_rows)
            .saturating_add(native_rows)
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
        // Visible body = total height minus borders, header, optional
        // reason and native-pick rows.
        let chrome = 2u16
            .saturating_add(self.header_rows())
            .saturating_add(reason_rows)
            .saturating_add(native_rows);
        self.visible_body_rows = height.saturating_sub(chrome) as usize;
        // Clamp scroll so a shrink (frame resize, re-anchor) does not leave
        // the viewport past the end of either list.
        let targets = self.routes.len();
        let models = self.models().len();
        let efforts = self.efforts_for_selected_model().len();
        let vis = self.visible_body_rows;
        self.target_scroll = self.target_scroll.min(targets.saturating_sub(vis));
        self.model_scroll = self.model_scroll.min(models.saturating_sub(vis));
        self.effort_scroll = self.effort_scroll.min(efforts.saturating_sub(vis));
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

    pub fn ensure_effort_visible(&mut self) {
        let len = self.efforts_for_selected_model().len();
        let vis = self.visible_body_rows.max(1);
        if self.effort_selected < self.effort_scroll {
            self.effort_scroll = self.effort_selected;
        } else if self.effort_selected >= self.effort_scroll + vis {
            self.effort_scroll = self.effort_selected + 1 - vis;
        }
        self.effort_scroll = self.effort_scroll.min(len.saturating_sub(vis));
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
            RouteFocus::Efforts => {
                let len = self.efforts_for_selected_model().len();
                let max = len.saturating_sub(vis);
                if max == 0 {
                    return true;
                }
                let next = (self.effort_scroll as isize + delta).clamp(0, max as isize) as usize;
                self.effort_scroll = next;
            }
        }
        true
    }

    /// Column boundaries for hit-testing / scrolling. Returns
    /// `(target_end, model_end)` absolute x coordinates (inclusive of the
    /// cell just left of the next divider).
    pub fn column_bounds(&self) -> (u16, u16) {
        let inner_x = self.area.x.saturating_add(1);
        let target_end = inner_x.saturating_add(self.target_col_w);
        let model_end = target_end
            .saturating_add(1)
            .saturating_add(self.model_col_w);
        (target_end, model_end)
    }

    /// Scroll the column under `(col, row)`. Falls back to the focused
    /// column when the pointer is over chrome (header / border).
    pub fn scroll_at(&mut self, col: u16, row: u16, delta: isize) -> bool {
        if !self.contains(col, row) {
            return false;
        }
        let first = self.area.y.saturating_add(1);
        let body_start = first.saturating_add(self.header_rows());
        let last = self
            .area
            .y
            .saturating_add(self.area.height)
            .saturating_sub(1);
        if row >= body_start && row < last {
            let (target_end, model_end) = self.column_bounds();
            let prev = self.focus;
            if col <= target_end {
                self.focus = RouteFocus::Targets;
            } else if self.any_effort_column() && col > model_end {
                self.focus = RouteFocus::Efforts;
            } else {
                self.focus = RouteFocus::Models;
            }
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

    /// Which column and row a click landed on.
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
        let (target_end, model_end) = self.column_bounds();
        if col <= target_end {
            let index = visible + self.target_scroll;
            (index < self.routes.len()).then_some(RouteHit::Target(index + 1))
        } else if self.any_effort_column() && col > model_end {
            if self.focused_blocker().is_some() {
                return None;
            }
            let efforts = self.efforts_for_selected_model();
            if efforts.len() <= 1 {
                return None;
            }
            let index = visible + self.effort_scroll;
            (index < efforts.len()).then_some(RouteHit::Effort(index))
        } else {
            // A blocked focused target fills the model column with its
            // reason and action line instead of model rows: any click
            // there activates the blocker action (sign-in when the
            // blocker is a login, restating the reason otherwise).
            if self.focused_blocker().is_some() {
                return Some(RouteHit::Model(0));
            }
            let index = visible + self.model_scroll;
            (index < self.models().len()).then_some(RouteHit::Model(index))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(
        name: &str,
        models: &[&str],
        reason: Option<&str>,
    ) -> construct_protocol::RouteOption {
        construct_protocol::RouteOption {
            name: name.to_string(),
            dialect: "anthropic".to_string(),
            model: models.first().copied().unwrap_or_default().to_string(),
            models: models.iter().map(|m| m.to_string()).collect(),
            efforts: Default::default(),
            base_url: "https://example.invalid".to_string(),
            unavailable_reason: reason.map(str::to_string),
            login_command: None,
        }
    }

    fn option_with_efforts(
        name: &str,
        models: &[&str],
        efforts: &[(&str, &[&str])],
    ) -> construct_protocol::RouteOption {
        let mut route = option(name, models, None);
        for (model, levels) in efforts {
            route.efforts.insert(
                (*model).to_string(),
                levels.iter().map(|s| (*s).to_string()).collect(),
            );
        }
        route
    }

    fn menu(routes: Vec<construct_protocol::RouteOption>, active: Option<&str>) -> RouteMenu {
        let mut m = RouteMenu {
            session_id: "s1".into(),
            area: ratatui::layout::Rect::default(),
            routes,
            unavailable_reason: None,
            active: active.map(str::to_string),
            active_model: None,
            active_effort: None,
            native: None,
            focus: RouteFocus::Targets,
            selected: 0,
            model_selected: 0,
            effort_selected: 0,
            target_scroll: 0,
            model_scroll: 0,
            effort_scroll: 0,
            anchor: (40, 23),
            target_col_w: 0,
            model_col_w: 0,
            desc_lines: 0,
            visible_body_rows: 0,
        };
        m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 30));
        m
    }

    #[test]
    fn effort_column_appears_when_a_model_advertises_levels() {
        let mut m = menu(
            vec![option_with_efforts(
                "codex-oauth",
                &["gpt-5.6-sol", "gpt-5.5"],
                &[
                    ("gpt-5.6-sol", &["low", "medium", "high"]),
                    ("gpt-5.5", &["low", "medium", "high"]),
                ],
            )],
            None,
        );
        m.selected = 1;
        assert!(m.any_effort_column());
        assert_eq!(m.efforts_for_selected_model(), vec!["low", "medium", "high"]);
        assert!(m.model_descends());
        let m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 30));
        let first = m.area.y + 1;
        let body = first + m.header_rows();
        let (target_end, model_end) = m.column_bounds();
        assert_eq!(m.hit_at(target_end.saturating_sub(1), body), Some(RouteHit::Target(1)));
        assert_eq!(m.hit_at(target_end + 2, body), Some(RouteHit::Model(0)));
        assert_eq!(m.hit_at(model_end + 2, body), Some(RouteHit::Effort(0)));
        assert_eq!(m.hit_at(model_end + 2, body + 2), Some(RouteHit::Effort(2)));
    }

    #[test]
    fn no_effort_column_when_levels_are_absent() {
        let mut m = menu(vec![option("kimi", &["kimi-k2.5"], None)], None);
        m.selected = 1;
        assert!(!m.any_effort_column());
        assert!(m.efforts_for_selected_model().is_empty());
        assert!(!m.model_descends());
    }

    #[test]
    fn the_first_target_is_default_and_always_selectable() {
        let m = menu(vec![option("kimi", &["kimi-k2.5"], Some("no key"))], None);
        assert_eq!(m.target_label(0), "No redirect");
        assert!(m.target_enabled(0));
        assert!(!m.target_descends(0), "Default has no models to move into");
        assert!(!m.target_enabled(1), "an unusable target is not selectable");
        assert!(
            m.target_is_active(0),
            "no route armed means Default is current"
        );
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

    /// The sign-in offer follows the blocker: present for a blocked login
    /// target, absent for usable targets even if the field is set.
    #[test]
    fn a_login_blocker_offers_its_command_only_while_blocked() {
        let mut blocked = option(
            "kimi",
            &["kimi-k2.5"],
            Some("not logged in to kimi; run `kimi` and sign in"),
        );
        blocked.login_command = Some("kimi".into());
        let mut healthy = option("codex-oauth", &["gpt-5.6-sol"], None);
        healthy.login_command = Some("codex login".into());
        let mut m = menu(vec![blocked, healthy], None);
        m.selected = 1;
        assert_eq!(m.focused_login_command().as_deref(), Some("kimi"));
        m.selected = 2;
        assert_eq!(
            m.focused_login_command(),
            None,
            "usable targets offer models, not sign-in"
        );
    }

    /// The model column condenses the daemon's long reason into a short
    /// state + "login" action. Expiry vs absence use different lead-ins so
    /// the user sees which case they are in without reading a multi-clause
    /// sentence.
    #[test]
    fn login_blocker_prefix_distinguishes_expiry_from_absence() {
        assert_eq!(
            login_blocker_prefix("claude login has expired; run `claude` once to renew it"),
            "token expired. click here to "
        );
        assert_eq!(
            login_blocker_prefix("not logged in to kimi; run `kimi` and sign in"),
            "not logged in. click here to "
        );
        let mut expired = option(
            "claude-oauth",
            &["sonnet"],
            Some("claude login has expired; run `claude` once to renew it"),
        );
        expired.login_command = Some("claude".into());
        let mut m = menu(vec![expired], None);
        m.selected = 1;
        assert_eq!(
            m.focused_login_blocker_prefix(),
            Some("token expired. click here to ")
        );
        // Key/dialect blockers keep the full reason and offer no prefix.
        m = menu(
            vec![option("glm", &["glm-5"], Some("GLM_API_KEY is not set"))],
            None,
        );
        m.selected = 1;
        assert_eq!(m.focused_login_blocker_prefix(), None);
    }

    /// The model column of a blocked target is one big action: any click
    /// there activates the sign-in (the underlined `login` word is the
    /// visual affordance; the whole right body remains the hit target).
    #[test]
    fn clicks_on_a_blocker_activate_it_anywhere_in_the_column() {
        let mut blocked = option("kimi", &["kimi-k2.5"], Some("not logged in"));
        blocked.login_command = Some("kimi".into());
        let mut m = menu(vec![blocked], None);
        m.selected = 1;
        let m = m.anchored(ratatui::layout::Rect::new(0, 0, 120, 30));
        let first = m.area.y + 1;
        let body = first + m.header_rows();
        let divider = m.area.x + 1 + m.target_col_w;
        assert_eq!(m.hit_at(divider + 2, body), Some(RouteHit::Model(0)));
        // Extra body rows (taller left column) still fire the same action.
        if m.visible_body_rows > 1 {
            assert_eq!(
                m.hit_at(divider + 2, body + 1),
                Some(RouteHit::Model(0)),
                "the rest of the right column is still the action"
            );
        }
    }

    /// A target that cannot be used shows why, in place of models.
    #[test]
    fn an_unusable_target_shows_its_reason_instead_of_models() {
        let mut m = menu(
            vec![option("glm", &["glm-5"], Some("GLM_API_KEY is not set"))],
            None,
        );
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

    /// A live native catalog pick (spec 0157) is current state the menu
    /// must show: its rows are marked, Default stops claiming to be
    /// current, and one note row explains the marker.
    #[test]
    fn a_native_catalog_pick_is_marked_and_default_stops_claiming_current() {
        let mut m = menu(
            vec![option("codex-oauth", &["gpt-5.6-sol", "gpt-5.5"], None)],
            None,
        );
        m.native = Some(("codex-oauth".into(), "gpt-5.5".into()));
        let frame = ratatui::layout::Rect::new(0, 0, 120, 30);
        let m2 = m.clone().anchored(frame);
        assert!(!m2.target_is_active(0), "Default is not the live state");
        assert!(m2.target_is_native(1));
        assert!(!m2.target_is_active(1), "native pick is not the pin");
        let note = m2.native_note().unwrap();
        assert!(
            note.contains("gpt-5.5") && note.contains("codex-oauth"),
            "{note}"
        );
        // The note occupies a real row: same menu without it is one shorter.
        let mut bare = m2.clone();
        bare.native = None;
        assert_eq!(bare.anchored(frame).area.height + 1, m2.area.height);
    }

    #[test]
    fn the_native_model_row_is_marked_under_its_own_target() {
        let mut m = menu(
            vec![
                option("codex-oauth", &["gpt-5.6-sol", "gpt-5.5"], None),
                option("kimi", &["kimi-k2.5"], None),
            ],
            None,
        );
        m.native = Some(("codex-oauth".into(), "gpt-5.5".into()));
        m.selected = 1;
        assert!(m.model_is_native(1));
        assert!(!m.model_is_native(0));
        // Under a different target the marker must not appear.
        m.selected = 2;
        assert!(!m.model_is_native(0));
    }

    /// With no pin armed, the menu opens on the native pick so the eye
    /// lands on the live state (mirrors opening on the armed target).
    #[test]
    fn a_pin_still_outranks_the_native_pick_for_the_opening_row() {
        let mut m = menu(
            vec![
                option("codex-oauth", &["gpt-5.6-sol"], None),
                option("kimi", &["kimi-k2.5"], None),
            ],
            Some("kimi"),
        );
        m.native = Some(("codex-oauth".into(), "gpt-5.6-sol".into()));
        // Both markers render truthfully at once.
        assert!(m.target_is_native(1));
        assert!(m.target_is_active(2));
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
        assert_eq!(m.hit_at(divider + 2, body), Some(RouteHit::Model(3)));
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
