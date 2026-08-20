//! Global k9s-style breadcrumb bar. Computed from `AppState` and rendered as
//! a single dim line at the very top of the terminal, before every view.
//!
//! The breadcrumb shows the navigation path from the root (subscription scope)
//! down to whatever the user is currently looking at — list/detail/logs for
//! resources, account/container/blob for storage, apim drill-ins, and so on.
//! Separator is `" > "`. Non-leaf segments are dimmed (`theme.muted`); the
//! leaf segment uses the normal foreground so the current location stands out.
//!
//! The full string overflows long resource paths quickly, so the renderer
//! truncates from the *middle* (preserving root + leaf) using
//! [`truncate_middle`]. This is intentionally different from the
//! right-truncation used in row columns: for navigation, the user needs both
//! ends of the path to know "where am I, where did I come from".

#![allow(dead_code)]

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

/// Separator between breadcrumb segments. Space-arrow-space, same shape as
/// k9s' nav bar.
pub const SEP: &str = " > ";

/// Compute the breadcrumb path for the current view as a list of segments
/// (root → leaf). Returned as `Vec<String>` so callers can decide how to join /
/// style; see [`render`] for the styled form.
pub fn breadcrumb(state: &AppState) -> String {
    segments(state).join(SEP)
}

/// Segment list, root first. Public-in-crate so the renderer can style the
/// final element differently from the rest.
pub(crate) fn segments(state: &AppState) -> Vec<String> {
    match state.view {
        // Help and the subscription picker are modal-ish: showing the
        // previous-view chain would just be noise. Reduce to a single segment.
        View::Help => vec!["help".to_string()],
        View::Subscriptions => vec!["subscriptions".to_string()],

        View::List => vec![subscription_segment(state), "api resources".to_string()],

        View::Detail => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s
        }

        View::EnvVars => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("env vars".to_string());
            s
        }

        View::Logs => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("logs".to_string());
            s
        }

        View::LogDetail => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("logs".to_string());
            s.push("entry".to_string());
            s
        }

        View::AppGatewayBackends => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("backends".to_string());
            s
        }

        View::ApimApis => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("apis".to_string());
            s
        }

        View::ApimOperations => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("apis".to_string());
            if let Some(api) = state
                .apim
                .selected_api_id
                .as_deref()
                .and_then(short_api_segment)
            {
                s.push(api);
            }
            s.push("operations".to_string());
            s
        }

        View::ApimPolicy => {
            let mut s = vec![subscription_segment(state), "api resources".to_string()];
            if let Some(r) = state.selected_resource() {
                s.push(r.name.clone());
            }
            s.push("apis".to_string());
            if let Some(api) = state
                .apim
                .selected_api_id
                .as_deref()
                .and_then(short_api_segment)
            {
                s.push(api);
            }
            s.push("policy".to_string());
            s
        }

        View::StorageAccounts => vec![subscription_segment(state), "storage".to_string()],

        // The overview shares its breadcrumb shape with `StorageContainers`:
        // the leaf is the account name in both cases. They're sibling drill-in
        // panes for the same pinned account, so a stable breadcrumb is the
        // right user signal (the title bar inside each view differs).
        View::StorageAccountOverview => {
            let mut s = vec![subscription_segment(state), "storage".to_string()];
            if let Some(acc) = state.storage.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            s
        }

        View::StorageContainers => {
            let mut s = vec![subscription_segment(state), "storage".to_string()];
            if let Some(acc) = state.storage.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            s
        }

        View::StorageBlobs => {
            let mut s = vec![subscription_segment(state), "storage".to_string()];
            if let Some(acc) = state.storage.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            if let Some(container) = state.storage.selected_container.as_deref() {
                s.push(container.to_string());
            }
            s
        }

        View::StorageBlobDetail => {
            let mut s = vec![subscription_segment(state), "storage".to_string()];
            if let Some(acc) = state.storage.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            if let Some(container) = state.storage.selected_container.as_deref() {
                s.push(container.to_string());
            }
            if let Some(blob) = state.storage.selected_blob.as_deref() {
                s.push(blob.to_string());
            }
            s
        }

        View::StorageAccessLogs => {
            let mut s = vec![subscription_segment(state), "storage".to_string()];
            if let Some(acc) = state.storage.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            if let Some(container) = state.storage.access_scope.as_deref() {
                s.push(container.to_string());
            }
            s.push("access log".to_string());
            s
        }

        View::Registries => vec![subscription_segment(state), "registries".to_string()],

        View::RegistryRepositories => {
            let mut s = vec![subscription_segment(state), "registries".to_string()];
            if let Some(reg) = state.registry.selected_registry.as_ref() {
                s.push(reg.name.clone());
            }
            s
        }

        View::RegistryTags => {
            let mut s = vec![subscription_segment(state), "registries".to_string()];
            if let Some(reg) = state.registry.selected_registry.as_ref() {
                s.push(reg.name.clone());
            }
            if let Some(repo) = state.registry.selected_repository.as_deref() {
                s.push(repo.to_string());
            }
            s
        }

        View::RegistryAccessLogs => {
            let mut s = vec![subscription_segment(state), "registries".to_string()];
            if let Some(reg) = state.registry.selected_registry.as_ref() {
                s.push(reg.name.clone());
            }
            if let Some(repo) = state.registry.access_scope.as_deref() {
                s.push(repo.to_string());
            }
            s.push("access log".to_string());
            s
        }

        View::LogicApps => vec![subscription_segment(state), "logic apps".to_string()],

        View::LogicAppRuns => {
            let mut s = vec![subscription_segment(state), "logic apps".to_string()];
            if let Some(wf) = state.logic_apps.selected_workflow.as_ref() {
                s.push(wf.name.clone());
            }
            s.push("runs".to_string());
            s
        }

        View::LogicAppTriggerHistory => {
            let mut s = vec![subscription_segment(state), "logic apps".to_string()];
            if let Some(wf) = state.logic_apps.selected_workflow.as_ref() {
                s.push(wf.name.clone());
            }
            s.push("trigger history".to_string());
            s
        }

        View::LogicAppRunDetail => {
            let mut s = vec![subscription_segment(state), "logic apps".to_string()];
            if let Some(wf) = state.logic_apps.selected_workflow.as_ref() {
                s.push(wf.name.clone());
            }
            s.push("runs".to_string());
            if let Some(run) = state.logic_apps.selected_run.as_ref() {
                s.push(run.name.clone());
            }
            s
        }

        View::LogicAppContent => {
            let mut s = vec![subscription_segment(state), "logic apps".to_string()];
            if let Some(wf) = state.logic_apps.selected_workflow.as_ref() {
                s.push(wf.name.clone());
            }
            if let Some(src) = state.logic_apps.selected_content.as_ref() {
                s.push(src.title.clone());
            }
            s
        }

        View::CosmosAccounts => vec![subscription_segment(state), "cosmos".to_string()],

        View::CosmosDatabases => {
            let mut s = vec![subscription_segment(state), "cosmos".to_string()];
            if let Some(acc) = state.cosmos.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            s
        }

        View::CosmosContainers => {
            let mut s = vec![subscription_segment(state), "cosmos".to_string()];
            if let Some(acc) = state.cosmos.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            if let Some(db) = state.cosmos.selected_database.as_deref() {
                s.push(db.to_string());
            }
            s
        }

        View::CosmosItem => {
            let mut s = vec![subscription_segment(state), "cosmos".to_string()];
            if let Some(acc) = state.cosmos.selected_account.as_ref() {
                s.push(acc.name.clone());
            }
            if let Some(db) = state.cosmos.selected_database.as_deref() {
                s.push(db.to_string());
            }
            if let Some(coll) = state.cosmos.selected_container.as_deref() {
                s.push(coll.to_string());
            }
            s.push("items".to_string());
            s
        }

        View::KeyVaults => vec![subscription_segment(state), "key vaults".to_string()],

        View::KeyVaultItems => {
            let mut s = vec![subscription_segment(state), "key vaults".to_string()];
            if let Some(v) = state.key_vault.selected_vault.as_ref() {
                s.push(v.name.clone());
            }
            s.push(state.key_vault.items_kind.path_segment().to_string());
            s
        }

        View::KeyVaultAccessLogs => {
            let mut s = vec![subscription_segment(state), "key vaults".to_string()];
            if let Some(v) = state.key_vault.selected_vault.as_ref() {
                s.push(v.name.clone());
            }
            if let Some(scope) = state.key_vault.access_scope.as_ref() {
                s.push(scope.path());
            }
            s.push("access log".to_string());
            s
        }

        View::ServiceBusNamespaces => {
            vec![subscription_segment(state), "service bus".to_string()]
        }

        View::ServiceBusEntities => {
            let mut s = vec![subscription_segment(state), "service bus".to_string()];
            if let Some(ns) = state.service_bus.selected_namespace.as_ref() {
                s.push(ns.name.clone());
            }
            s.push(state.service_bus.entity_kind.label().to_string());
            s
        }

        View::ServiceBusSubscriptions => {
            let mut s = vec![subscription_segment(state), "service bus".to_string()];
            if let Some(ns) = state.service_bus.selected_namespace.as_ref() {
                s.push(ns.name.clone());
            }
            if let Some(topic) = state.service_bus.selected_topic.as_deref() {
                s.push(topic.to_string());
            }
            s.push("subscriptions".to_string());
            s
        }

        View::SqlResources => vec![subscription_segment(state), "azure sql".to_string()],

        View::SqlDetail => {
            let mut s = vec![subscription_segment(state), "azure sql".to_string()];
            if let Some(r) = state.sql.selected.as_ref() {
                s.push(format!("{}/{}", r.server, r.name));
            }
            s
        }

        View::SqlAuditPrincipals => {
            let mut s = vec![subscription_segment(state), "azure sql".to_string()];
            if let Some(target) = state.sql.audit.target.as_ref() {
                s.push(target.label());
            }
            s.push("audit".to_string());
            s
        }

        View::SqlAuditEvents | View::SqlAuditEventDetail => {
            let mut s = vec![subscription_segment(state), "azure sql".to_string()];
            if let Some(target) = state.sql.audit.target.as_ref() {
                s.push(target.label());
            }
            s.push("audit".to_string());
            if let Some(p) = state.sql.audit.selected_principal.as_deref() {
                s.push(p.to_string());
            }
            if state.view == View::SqlAuditEventDetail {
                s.push("event".to_string());
            }
            s
        }

        View::SqlSessions => {
            let mut s = vec![subscription_segment(state), "azure sql".to_string()];
            if let Some(target) = state.sql.sessions.target.as_ref() {
                s.push(target.label());
            }
            s.push("sessions".to_string());
            s
        }
    }
}

/// `sub:{name}` if a single subscription is pinned and we can resolve it to
/// a display name, `sub:{first-8-chars}` if it's pinned but the subscriptions
/// list hasn't loaded yet, and `all subscriptions` if nothing is pinned.
fn subscription_segment(state: &AppState) -> String {
    match state.selected_subscription.as_deref() {
        None => "all subscriptions".to_string(),
        Some(id) => {
            let name = state
                .subscriptions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.display_name.as_str());
            match name {
                Some(name) => format!("sub:{name}"),
                None => {
                    // Subscriptions haven't loaded yet (or this id isn't in the
                    // visible set). Show enough of the GUID to be useful.
                    let short: String = id.chars().take(8).collect();
                    format!("sub:{short}")
                }
            }
        }
    }
}

/// Pull the API name off the tail of `…/apis/{api}` so the breadcrumb reads
/// `my-api` rather than the full ARM id. Returns `None` if the shape doesn't
/// match (in which case the caller drops the segment).
fn short_api_segment(api_id: &str) -> Option<String> {
    api_id
        .trim_end_matches('/')
        .rsplit_once("/apis/")
        .map(|(_, tail)| tail.to_string())
        .filter(|s| !s.is_empty())
}

/// Render the breadcrumb into `area` (assumed to be a single-row strip).
/// Truncates from the middle if the joined string overflows the row width.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let segs = segments(state);
    let width = area.width as usize;

    let line = styled_line(&segs, width, theme);
    frame.render_widget(Paragraph::new(line), area);
}

/// Build the styled `Line` for the breadcrumb. Non-leaf segments and the
/// separator use `theme.muted`; the final (leaf) segment uses `theme.fg`.
/// If the joined string exceeds `max_width`, the whole thing is collapsed
/// into a single muted span produced by [`truncate_middle`] — preserving
/// styling per-segment after truncation would require character-by-character
/// reconstruction with no real visual benefit.
fn styled_line<'a>(segs: &[String], max_width: usize, theme: &Theme) -> Line<'a> {
    if segs.is_empty() || max_width == 0 {
        return Line::from(Span::raw(String::new()));
    }
    let full = segs.join(SEP);
    if full.chars().count() <= max_width {
        let mut spans: Vec<Span> = Vec::with_capacity(segs.len() * 2);
        let last = segs.len() - 1;
        for (i, seg) in segs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(SEP, Style::default().fg(theme.muted)));
            }
            let style = if i == last {
                Style::default().fg(theme.fg)
            } else {
                Style::default().fg(theme.muted)
            };
            spans.push(Span::styled(seg.clone(), style));
        }
        return Line::from(spans);
    }
    // Doesn't fit: collapse to a single middle-truncated span. The leaf is
    // still the "important" end, so the user sees both ends of the path.
    let truncated = truncate_middle(&full, max_width);
    Line::from(Span::styled(truncated, Style::default().fg(theme.muted)))
}

/// Truncate `s` from the middle to fit within `max` *characters*, inserting
/// a single `…` in place of the dropped chunk. If `s` already fits, returns
/// it unchanged. If `max == 0`, returns the empty string. If `max == 1`,
/// returns just `…`.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    // Reserve one cell for the ellipsis, split the rest between head and tail.
    // Bias the head one cell larger when the remainder is odd so the user
    // sees more of the *start* of the path (which is the navigation root —
    // the deepest leaf is usually a long blob name and is less identifying).
    let budget = max - 1;
    let tail_len = budget / 2;
    let head_len = budget - tail_len;
    let head: String = s.chars().take(head_len).collect();
    let tail: String = s.chars().skip(count - tail_len).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::azure::storage::StorageAccount;
    use crate::azure::subscriptions::Subscription;
    use crate::config::Config;

    fn fresh_state() -> AppState {
        AppState::new(Config::default())
    }

    fn sub_fixture() -> Subscription {
        Subscription {
            id: "00000000-1111-2222-3333-444444444444".into(),
            display_name: "my-sub".into(),
            state: "Enabled".into(),
            tenant_id: "t".into(),
        }
    }

    fn function_app_fixture() -> Resource {
        Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.Web/sites/my-functionapp"
                .into(),
            name: "my-functionapp".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "00000000-1111-2222-3333-444444444444".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }
    }

    #[test]
    fn subscription_picker_breadcrumb() {
        let mut state = fresh_state();
        state.view = View::Subscriptions;
        assert_eq!(breadcrumb(&state), "subscriptions");
    }

    #[test]
    fn help_breadcrumb_is_single_segment() {
        let mut state = fresh_state();
        state.view = View::Help;
        assert_eq!(breadcrumb(&state), "help");
    }

    #[test]
    fn resources_breadcrumb_with_selected_subscription() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.view = View::List;
        assert_eq!(breadcrumb(&state), "sub:my-sub > api resources");
    }

    #[test]
    fn resources_breadcrumb_with_no_subscription_pinned() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = None;
        state.view = View::List;
        assert_eq!(breadcrumb(&state), "all subscriptions > api resources");
    }

    #[test]
    fn resources_breadcrumb_falls_back_to_short_id_when_name_missing() {
        // Pinned subscription id is set but the subscriptions list hasn't
        // loaded yet — show the first 8 chars of the id so the user has
        // *something* useful instead of nothing.
        let mut state = fresh_state();
        state.subscriptions = Vec::new();
        state.selected_subscription = Some("abcdef0123456789-deadbeef".into());
        state.view = View::List;
        assert_eq!(breadcrumb(&state), "sub:abcdef01 > api resources");
    }

    #[test]
    fn function_app_detail_breadcrumb_includes_resource_name() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![function_app_fixture()];
        state.list_cursor = 0;
        state.view = View::Detail;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-functionapp"
        );
    }

    #[test]
    fn function_app_logs_breadcrumb() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![function_app_fixture()];
        state.list_cursor = 0;
        state.view = View::Logs;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-functionapp > logs"
        );
    }

    #[test]
    fn function_app_log_detail_breadcrumb_appends_entry() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![function_app_fixture()];
        state.list_cursor = 0;
        state.view = View::LogDetail;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-functionapp > logs > entry"
        );
    }

    #[test]
    fn appgw_backends_breadcrumb() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.Network/applicationGateways/my-appgw".into(),
            name: "my-appgw".into(),
            kind: ResourceKind::AppGateway,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: sub_fixture().id,
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::AppGatewayBackends;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-appgw > backends"
        );
    }

    #[test]
    fn apim_apis_breadcrumb() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim".into(),
            name: "my-apim".into(),
            kind: ResourceKind::Apim,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: sub_fixture().id,
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::ApimApis;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-apim > apis"
        );
    }

    #[test]
    fn apim_operations_breadcrumb_includes_api_name() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim".into(),
            name: "my-apim".into(),
            kind: ResourceKind::Apim,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: sub_fixture().id,
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.apim.selected_api_id = Some(
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim/apis/my-api".into(),
        );
        state.view = View::ApimOperations;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-apim > apis > my-api > operations"
        );
    }

    #[test]
    fn apim_policy_breadcrumb_includes_api_name() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim".into(),
            name: "my-apim".into(),
            kind: ResourceKind::Apim,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: sub_fixture().id,
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.apim.selected_api_id = Some(
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim/apis/my-api".into(),
        );
        state.apim.selected_operation_id = Some(
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim/apis/my-api/operations/get-thing".into(),
        );
        state.view = View::ApimPolicy;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > api resources > my-apim > apis > my-api > policy"
        );
    }

    fn storage_account_fixture() -> StorageAccount {
        StorageAccount {
            id: "/subs/X/rg/y/sa/my-account".into(),
            name: "my-account".into(),
            resource_group: "rg".into(),
            subscription_id: "00000000-1111-2222-3333-444444444444".into(),
            location: "westeurope".into(),
            kind: Some("StorageV2".into()),
            sku: None,
            access_tier: None,
            is_hns_enabled: None,
            https_only: None,
            allow_blob_public_access: None,
            created_at: None,
        }
    }

    #[test]
    fn storage_accounts_breadcrumb() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.view = View::StorageAccounts;
        assert_eq!(breadcrumb(&state), "sub:my-sub > storage");
    }

    #[test]
    fn storage_account_overview_breadcrumb_matches_containers() {
        // Overview and Containers are sibling drill-ins for the same account,
        // so they share the breadcrumb shape — the navigation context (which
        // *account* you're inside) is the same.
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.storage.selected_account = Some(storage_account_fixture());
        state.view = View::StorageAccountOverview;
        assert_eq!(breadcrumb(&state), "sub:my-sub > storage > my-account");
    }

    #[test]
    fn storage_containers_breadcrumb_includes_account() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.storage.selected_account = Some(storage_account_fixture());
        state.view = View::StorageContainers;
        assert_eq!(breadcrumb(&state), "sub:my-sub > storage > my-account");
    }

    #[test]
    fn storage_blobs_breadcrumb_includes_container() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.storage.selected_account = Some(storage_account_fixture());
        state.storage.selected_container = Some("my-container".into());
        state.view = View::StorageBlobs;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > storage > my-account > my-container"
        );
    }

    #[test]
    fn storage_blob_detail_breadcrumb_includes_blob_path() {
        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.storage.selected_account = Some(storage_account_fixture());
        state.storage.selected_container = Some("my-container".into());
        state.storage.selected_blob = Some("path/to/blob.json".into());
        state.view = View::StorageBlobDetail;
        assert_eq!(
            breadcrumb(&state),
            "sub:my-sub > storage > my-account > my-container > path/to/blob.json"
        );
    }

    #[test]
    fn truncate_middle_short_string_unchanged() {
        assert_eq!(truncate_middle("hello", 10), "hello");
        assert_eq!(truncate_middle("hello", 5), "hello");
    }

    #[test]
    fn truncate_middle_inserts_ellipsis() {
        // 11 chars in, 7 cells out → 3 head + 1 ellipsis + 3 tail = 7.
        let out = truncate_middle("abcdefghijk", 7);
        assert_eq!(out.chars().count(), 7);
        assert!(out.contains('…'));
        assert!(out.starts_with("abc"));
        assert!(out.ends_with("ijk"));
    }

    #[test]
    fn truncate_middle_zero_and_one() {
        assert_eq!(truncate_middle("anything", 0), "");
        assert_eq!(truncate_middle("anything", 1), "…");
    }

    #[test]
    fn truncate_middle_biases_head_when_odd() {
        // 12 chars in, 6 cells out → budget 5, tail 2, head 3.
        let out = truncate_middle("abcdefghijkl", 6);
        assert_eq!(out, "abc…kl");
    }

    #[test]
    fn segments_subscription_picker_is_root_only() {
        let mut state = fresh_state();
        state.view = View::Subscriptions;
        assert_eq!(segments(&state), vec!["subscriptions".to_string()]);
    }

    #[test]
    fn render_smoke_paints_path_into_frame() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.resources = vec![function_app_fixture()];
        state.list_cursor = 0;
        state.view = View::Logs;

        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 3);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let row = Rect {
                x: 0,
                y: 0,
                width: f.area().width,
                height: 1,
            };
            render(f, row, &state, &theme);
        })
        .unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("sub:my-sub"));
        assert!(buf.contains("api resources"));
        assert!(buf.contains("my-functionapp"));
        assert!(buf.contains("logs"));
    }

    #[test]
    fn render_smoke_middle_truncates_when_too_narrow() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = fresh_state();
        state.subscriptions = vec![sub_fixture()];
        state.selected_subscription = Some(sub_fixture().id);
        state.storage.selected_account = Some(storage_account_fixture());
        state.storage.selected_container = Some("logs".into());
        state.storage.selected_blob = Some(
            "very/deeply/nested/path/that/should/get/middle/truncated/because/it/is/too/long.json"
                .into(),
        );
        state.view = View::StorageBlobDetail;

        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(40, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        // The middle-truncate inserts an ellipsis, and the leaf's tail should
        // still be visible somewhere on the line.
        assert!(buf.contains('…'), "expected middle ellipsis in {buf}");
    }

    #[test]
    fn short_api_segment_extracts_tail() {
        assert_eq!(
            short_api_segment(
                "/subscriptions/X/resourceGroups/rg/providers/Microsoft.ApiManagement/service/my-apim/apis/my-api"
            ),
            Some("my-api".to_string())
        );
        assert_eq!(short_api_segment("no-apis-segment"), None);
    }
}
