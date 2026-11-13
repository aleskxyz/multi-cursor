//! Plan and usage details for an account, read from Cursor's servers.
//!
//! Cursor has no public API for this. These are the same (undocumented)
//! endpoints its IDE and dashboard use, so everything here fails softly: any
//! error degrades to the plan name stored in the local auth snapshot.
//!
//! Tokens are decrypted in memory only (see `safe_storage`), sent only to
//! Cursor, and never refreshed or written back. Refreshing would rotate the
//! refresh token and could sign the account out in Cursor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use crate::auth::{has_login_tokens, load_snapshot, read_auth_keys, AuthSnapshot};
use crate::paths::{env_data_dir, env_state_db, load_config};
use crate::safe_storage::{decrypt_token, TokenError};

const API: &str = "https://api2.cursor.sh";
const USAGE_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    /// `ok`, `expired` (token no longer valid), `locked` (Cursor's encryption
    /// key could not be unlocked, e.g. the user denied the prompt; not retried
    /// until a manual refresh) or `unavailable` (offline / API changed).
    pub status: String,
    /// Why the status is not `ok`, for the card's tooltip.
    pub detail: Option<String>,
    pub plan_name: Option<String>,
    pub subscription_status: Option<String>,
    /// Share of included usage spent, 0–100.
    pub used_percent: Option<f64>,
    pub included_used_cents: Option<f64>,
    pub included_limit_cents: Option<f64>,
    pub bonus_cents: Option<f64>,
    pub on_demand_enabled: Option<bool>,
    pub on_demand_used_cents: Option<f64>,
    pub on_demand_limit_cents: Option<f64>,
    /// When included usage resets (ISO 8601).
    pub resets_at: Option<String>,
    /// Prepaid ("additional usage") credit left.
    pub credit_available_cents: Option<f64>,
    /// Prepaid credit spent this calendar month.
    pub credit_spent_month_cents: Option<f64>,
    pub credit_auto_top_up: Option<bool>,
    /// Auto top-up: amount added, and the balance that triggers it.
    pub credit_top_up_amount_cents: Option<f64>,
    pub credit_top_up_threshold_cents: Option<f64>,
    /// Spend beyond the prepaid credit.
    pub credit_uncovered_cents: Option<f64>,
    /// When the monthly additional-usage figures reset (ISO 8601).
    pub monthly_resets_at: Option<String>,
    pub fetched_at: String,
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, AccountUsage)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, AccountUsage)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop an account's cached usage (it was deleted).
pub fn forget(account_id: &str) {
    if let Ok(mut map) = cache().lock() {
        map.remove(account_id);
    }
}

/// One client for the session, so connections to Cursor are reused.
fn http_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let client = reqwest::Client::builder()
        .user_agent(format!("multi-cursor/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(CLIENT.get_or_init(|| client))
}

/// Decoded JWT payload (no signature check: we only need `sub` and `exp`).
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `auth0|user_abc` → `user_abc`.
fn user_id_from_token(token: &str) -> Option<String> {
    let sub = jwt_claims(token)?.get("sub")?.as_str()?.to_string();
    let id = sub.rsplit('|').next().unwrap_or(&sub).trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn token_expired(token: &str) -> bool {
    let Some(exp) = jwt_claims(token).and_then(|c| c.get("exp").and_then(Value::as_i64)) else {
        return false;
    };
    exp <= chrono::Utc::now().timestamp()
}

/// "pro" → "Pro", "free_trial" → "Free Trial".
fn pretty_plan(raw: &str) -> Option<String> {
    let words: Vec<String> = raw
        .split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, key| cur.get(key))
}

/// A number at `path`; Connect JSON sends 64-bit integers as strings.
fn num(v: &Value, path: &[&str]) -> Option<f64> {
    let cur = at(v, path)?;
    cur.as_f64().or_else(|| cur.as_str()?.trim().parse().ok())
}

fn text(v: &Value, path: &[&str]) -> Option<String> {
    at(v, path)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Epoch milliseconds (number or string) → ISO 8601.
fn millis_to_iso(v: &Value, path: &[&str]) -> Option<String> {
    let ms = num(v, path)? as i64;
    if ms <= 0 {
        return None;
    }
    Some(chrono::DateTime::from_timestamp_millis(ms)?.to_rfc3339())
}

fn fill<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// `DashboardService/GetPlanInfo`: display name and the included-usage reset.
fn apply_plan_info(u: &mut AccountUsage, v: &Value) {
    fill(&mut u.plan_name, text(v, &["planInfo", "planName"]));
    fill(&mut u.resets_at, millis_to_iso(v, &["planInfo", "includedUsageResetsAt"]));
}

/// `DashboardService/GetCurrentPeriodUsage`: percent used and period end.
fn apply_current_period(u: &mut AccountUsage, v: &Value) {
    fill(&mut u.used_percent, num(v, &["planUsage", "totalPercentUsed"]));
    fill(&mut u.included_used_cents, num(v, &["planUsage", "includedSpend"]));
    fill(&mut u.included_limit_cents, num(v, &["planUsage", "limit"]));
    fill(&mut u.bonus_cents, num(v, &["planUsage", "bonusSpend"]).filter(|b| *b > 0.0));
    fill(&mut u.resets_at, millis_to_iso(v, &["billingCycleEnd"]));
}

/// `cursor.com/api/usage-summary`: dollar amounts and on-demand spend.
fn apply_usage_summary(u: &mut AccountUsage, v: &Value) {
    fill(&mut u.plan_name, text(v, &["membershipType"]).and_then(|p| pretty_plan(&p)));

    if let Some(plan) = v.pointer("/individualUsage/plan") {
        fill(
            &mut u.included_used_cents,
            num(plan, &["breakdown", "included"]).or(num(plan, &["used"])),
        );
        fill(&mut u.included_limit_cents, num(plan, &["limit"]));
        fill(&mut u.bonus_cents, num(plan, &["breakdown", "bonus"]).filter(|b| *b > 0.0));
        fill(&mut u.used_percent, num(plan, &["totalPercentUsed"]));
    }

    // Team members may only have a team-level on-demand pool.
    let individual = v.pointer("/individualUsage/onDemand");
    let on_demand = individual
        .filter(|o| o.get("enabled").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| v.pointer("/teamUsage/onDemand"))
        .or(individual);
    if let Some(od) = on_demand {
        fill(&mut u.on_demand_enabled, od.get("enabled").and_then(Value::as_bool));
        fill(&mut u.on_demand_used_cents, num(od, &["used"]));
        fill(&mut u.on_demand_limit_cents, num(od, &["limit"]));
    }
    // The calendar-month cycle: additional usage resets with it, and it is
    // the last-resort reset for included usage.
    fill(&mut u.monthly_resets_at, text(v, &["billingCycleEnd"]));
    fill(&mut u.resets_at, text(v, &["billingCycleEnd"]));
}

/// `DashboardService/GetUserPrepaidBilling`: prepaid "additional usage" credits.
/// Only for accounts in prepaid mode that have ever had credit.
fn apply_prepaid_billing(u: &mut AccountUsage, v: &Value) {
    let prepaid = text(v, &["billingMode"]).as_deref() == Some("PREPAID_BILLING_MODE_PREPAID");
    let funded = num(v, &["fundedCents"]).unwrap_or(0.0);
    let available = num(v, &["availableCents"]).unwrap_or(0.0);
    if !prepaid || (funded <= 0.0 && available <= 0.0) {
        return;
    }
    fill(&mut u.credit_available_cents, Some(available));
    fill(&mut u.credit_spent_month_cents, Some(num(v, &["spentThisMonthCents"]).unwrap_or(0.0)));
    let auto = v.pointer("/autoTopUp/enabled").and_then(Value::as_bool).unwrap_or(false);
    fill(&mut u.credit_auto_top_up, Some(auto));
    if auto {
        fill(&mut u.credit_top_up_amount_cents, num(v, &["autoTopUp", "topUpAmountCents"]));
        fill(&mut u.credit_top_up_threshold_cents, num(v, &["autoTopUp", "thresholdCents"]));
    }
    fill(&mut u.credit_uncovered_cents, num(v, &["uncoveredSpendCents"]).filter(|c| *c > 0.0));
}

/// `auth/full_stripe_profile`: subscription status (and plan, as a fallback).
fn apply_stripe_profile(u: &mut AccountUsage, v: &Value) {
    fill(&mut u.subscription_status, text(v, &["subscriptionStatus"]));
    fill(&mut u.plan_name, text(v, &["membershipType"]).and_then(|p| pretty_plan(&p)));
}

fn snapshot_key(snap: &AuthSnapshot, k: &str) -> Option<String> {
    snap.keys
        .get(k)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

enum Fetch {
    Ok(Value),
    Unauthorized,
    Failed,
}

async fn send_json(req: reqwest::RequestBuilder) -> Fetch {
    match req.send().await {
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => Fetch::Unauthorized,
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => Fetch::Ok(v),
            Err(_) => Fetch::Failed,
        },
        _ => Fetch::Failed,
    }
}

/// The account's auth keys and the Cursor data folder they belong to (its
/// `Local State` holds the Windows decryption key). The active account reads
/// Cursor's live database, since Cursor may have rotated the token since
/// Multi Cursor last saved it.
fn account_snapshot(account_id: &str) -> Result<(AuthSnapshot, PathBuf), String> {
    let cfg = load_config()?;
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| "Account not found".to_string())?;
    let active_env = cfg.active.env_id.as_deref();
    let data_dir = env_data_dir(&account.env_id, active_env)?;
    let is_active = cfg.active.account_id.as_deref() == Some(account_id)
        && active_env == Some(account.env_id.as_str());
    if is_active {
        if let Ok(live) = read_auth_keys(&env_state_db(&account.env_id, active_env)?) {
            if has_login_tokens(&live) {
                return Ok((live, data_dir));
            }
        }
    }
    Ok((load_snapshot(&account.env_id, account_id)?, data_dir))
}

async fn fetch_usage(snap: &AuthSnapshot, data_dir: &std::path::Path, retry_key: bool) -> AccountUsage {
    let mut u = AccountUsage {
        status: "unavailable".to_string(),
        fetched_at: crate::paths::now_iso(),
        ..Default::default()
    };
    let finish = |mut u: AccountUsage| {
        // Offline fallbacks from the snapshot Cursor wrote at sign-in.
        fill(
            &mut u.plan_name,
            snapshot_key(snap, "cursorAuth/stripeMembershipType").and_then(|p| pretty_plan(&p)),
        );
        fill(
            &mut u.subscription_status,
            snapshot_key(snap, "cursorAuth/stripeSubscriptionStatus"),
        );
        u
    };

    let Some(stored) = snapshot_key(snap, "cursorAuth/accessToken") else {
        u.detail = Some("Not signed in".to_string());
        return finish(u);
    };
    // Unlocking the key can block on a permission prompt: keep it off the async workers.
    let dir = data_dir.to_path_buf();
    let decrypted =
        tauri::async_runtime::spawn_blocking(move || decrypt_token(&stored, &dir, retry_key)).await;
    let token = match decrypted {
        Ok(Ok(t)) => t,
        Ok(Err(TokenError::KeyUnavailable(e))) => {
            u.status = "locked".to_string();
            u.detail = Some(e);
            return finish(u);
        }
        Ok(Err(TokenError::Invalid(e))) => {
            u.detail = Some(e);
            return finish(u);
        }
        Err(e) => {
            u.detail = Some(e.to_string());
            return finish(u);
        }
    };
    if token_expired(&token) {
        u.status = "expired".to_string();
        return finish(u);
    }

    let client = match http_client() {
        Ok(c) => c,
        Err(e) => {
            u.detail = Some(e.to_string());
            return finish(u);
        }
    };
    let rpc = |method: &str| {
        client
            .post(format!("{API}/aiserver.v1.DashboardService/{method}"))
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .body("{}")
    };

    let mut ok = false;
    // The IDE's own endpoint first: it also tells us if the token is dead,
    // so the rest aren't sent with a token Cursor rejects.
    match send_json(rpc("GetPlanInfo")).await {
        Fetch::Ok(v) => {
            apply_plan_info(&mut u, &v);
            ok = true;
        }
        Fetch::Unauthorized => {
            u.status = "expired".to_string();
            return finish(u);
        }
        Fetch::Failed => {}
    }

    // The rest are independent: run them together.
    let spawn = |req: reqwest::RequestBuilder| tauri::async_runtime::spawn(send_json(req));
    let prepaid = spawn(rpc("GetUserPrepaidBilling"));
    let period = spawn(rpc("GetCurrentPeriodUsage"));
    let summary = user_id_from_token(&token).map(|user_id| {
        spawn(
            client
                .get(USAGE_SUMMARY_URL)
                .header("Cookie", format!("WorkosCursorSessionToken={user_id}%3A%3A{token}"))
                .header("Accept", "application/json")
                .header("Referer", "https://cursor.com/dashboard"),
        )
    });
    let stripe = spawn(client.get(format!("{API}/auth/full_stripe_profile")).bearer_auth(&token));

    // Applied in a fixed order: earlier sources win when several report a field.
    type Apply = fn(&mut AccountUsage, &Value);
    let results: [(Option<_>, Apply); 4] = [
        (Some(prepaid), apply_prepaid_billing),
        (Some(period), apply_current_period),
        (summary, apply_usage_summary),
        (Some(stripe), apply_stripe_profile),
    ];
    for (task, apply) in results {
        let Some(task) = task else { continue };
        if let Ok(Fetch::Ok(v)) = task.await {
            apply(&mut u, &v);
            ok = true;
        }
    }

    if ok {
        u.status = "ok".to_string();
    } else {
        u.detail = Some("Could not reach Cursor".to_string());
    }
    finish(u)
}

/// Plan and usage for one account. Cached for a few minutes; `force` skips the cache.
#[tauri::command]
pub async fn account_usage(account_id: String, force: Option<bool>) -> Result<AccountUsage, String> {
    let force = force.unwrap_or(false);
    if !force {
        if let Ok(map) = cache().lock() {
            if let Some((at, usage)) = map.get(&account_id) {
                if at.elapsed() < CACHE_TTL {
                    return Ok(usage.clone());
                }
            }
        }
    }

    // Config and SQLite reads block: keep them off the async workers.
    let id = account_id.clone();
    let (snap, data_dir) = tauri::async_runtime::spawn_blocking(move || account_snapshot(&id))
        .await
        .map_err(|e| e.to_string())??;
    // A manual refresh is also the only thing that retries a denied key prompt.
    let usage = fetch_usage(&snap, &data_dir, force).await;
    // Only cache successes. "expired" must clear as soon as Cursor refreshes
    // the token, and "locked" is already remembered by `safe_storage`.
    if usage.status == "ok" {
        if let Ok(mut map) = cache().lock() {
            map.insert(account_id, (Instant::now(), usage.clone()));
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: &str) -> String {
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.sig", enc.encode(b"{\"alg\":\"HS256\"}"), enc.encode(payload))
    }

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn user_id_and_expiry_from_token() {
        let t = jwt(r#"{"sub":"auth0|user_01ABC","exp":4102444800}"#);
        assert_eq!(user_id_from_token(&t).as_deref(), Some("user_01ABC"));
        assert!(!token_expired(&t));
        let old = jwt(r#"{"sub":"user_x","exp":1000}"#);
        assert_eq!(user_id_from_token(&old).as_deref(), Some("user_x"));
        assert!(token_expired(&old));
        assert_eq!(user_id_from_token("not-a-jwt"), None);
        assert!(!token_expired("not-a-jwt"));
    }

    #[test]
    fn plan_names() {
        assert_eq!(pretty_plan("pro").as_deref(), Some("Pro"));
        assert_eq!(pretty_plan("free_trial").as_deref(), Some("Free Trial"));
        assert_eq!(pretty_plan("").as_deref(), None);
    }

    /// Shapes recorded from a real Premium account (Nov 2026), trimmed.
    #[test]
    fn combines_real_responses() {
        let mut u = AccountUsage::default();
        apply_plan_info(
            &mut u,
            &json(r#"{"planInfo":{"planName":"Premium","price":"","billingCycleEnd":"1794648460548",
                "includedUsagePeriod":"INCLUDED_USAGE_PERIOD_WEEKLY","includedUsageResetsAt":"1794994058385"}}"#),
        );
        apply_current_period(
            &mut u,
            &json(r#"{"billingCycleStart":"1794389258385","billingCycleEnd":"1794994058385",
                "planUsage":{"remainingBonus":false,"autoPercentUsed":0.2166,"totalPercentUsed":0.2166},
                "spendLimitUsage":{"limitType":"user"},"enabled":true}"#),
        );
        apply_usage_summary(
            &mut u,
            &json(r#"{"billingCycleStart":"2026-11-01T00:00:00.000Z","billingCycleEnd":"2026-12-01T00:00:00.000Z",
                "membershipType":"premium","individualUsage":{"plan":{"enabled":true,"used":0,"limit":4615,
                "remaining":4615,"breakdown":{"included":0,"bonus":0,"total":0},"totalPercentUsed":0},
                "onDemand":{"enabled":false,"used":0,"limit":null,"remaining":null}},"teamUsage":{}}"#),
        );
        apply_stripe_profile(
            &mut u,
            &json(r#"{"membershipType":"pro","subscriptionStatus":"active"}"#),
        );

        assert_eq!(u.plan_name.as_deref(), Some("Premium"));
        assert_eq!(u.subscription_status.as_deref(), Some("active"));
        // Weekly included-usage reset, not the monthly billing cycle.
        assert_eq!(u.resets_at.as_deref(), Some("2026-11-18T09:27:38.385+00:00"));
        assert_eq!(u.used_percent, Some(0.2166));
        assert_eq!(u.included_used_cents, Some(0.0));
        assert_eq!(u.included_limit_cents, Some(4615.0));
        assert_eq!(u.bonus_cents, None);
        assert_eq!(u.on_demand_enabled, Some(false));
        assert_eq!(u.on_demand_limit_cents, None);
    }

    /// Shape recorded from the same real account (prepaid credits, Nov 2026).
    #[test]
    fn prepaid_credits() {
        let mut u = AccountUsage::default();
        apply_prepaid_billing(
            &mut u,
            &json(r#"{"billingMode":"PREPAID_BILLING_MODE_PREPAID","eligible":true,"availableCents":"136",
                "fundedCents":"500","spentCents":"364","topUps":[{"ticket":"1","origin":"TOPUP_MANUAL",
                "amountCents":"500","status":"SUCCEEDED"}],"hasPaymentMethod":true,
                "spentThisMonthCents":"364","uncoveredSpendCents":"0"}"#),
        );
        apply_usage_summary(&mut u, &json(r#"{"billingCycleEnd":"2026-12-01T00:00:00.000Z"}"#));
        assert_eq!(u.credit_available_cents, Some(136.0));
        assert_eq!(u.credit_spent_month_cents, Some(364.0));
        assert_eq!(u.credit_auto_top_up, Some(false));
        assert_eq!(u.credit_uncovered_cents, None);
        assert_eq!(u.monthly_resets_at.as_deref(), Some("2026-12-01T00:00:00.000Z"));

        // Not prepaid, or never funded: no credit row.
        for body in [
            r#"{"billingMode":"PREPAID_BILLING_MODE_POSTPAID","availableCents":"136","fundedCents":"500"}"#,
            r#"{"billingMode":"PREPAID_BILLING_MODE_PREPAID","eligible":true}"#,
            "{}",
        ] {
            let mut u = AccountUsage::default();
            apply_prepaid_billing(&mut u, &json(body));
            assert_eq!(u.credit_available_cents, None, "{body}");
        }

        let mut u = AccountUsage::default();
        apply_prepaid_billing(
            &mut u,
            &json(r#"{"billingMode":"PREPAID_BILLING_MODE_PREPAID","availableCents":"0","fundedCents":"2000",
                "spentThisMonthCents":"2000","uncoveredSpendCents":"310",
                "autoTopUp":{"enabled":true,"thresholdCents":"500","topUpAmountCents":"2000"}}"#),
        );
        assert_eq!(u.credit_available_cents, Some(0.0));
        assert_eq!(u.credit_auto_top_up, Some(true));
        assert_eq!(u.credit_top_up_amount_cents, Some(2000.0));
        assert_eq!(u.credit_top_up_threshold_cents, Some(500.0));
        assert_eq!(u.credit_uncovered_cents, Some(310.0));
    }

    #[test]
    fn usage_summary_alone() {
        let mut u = AccountUsage::default();
        apply_usage_summary(
            &mut u,
            &json(r#"{"billingCycleEnd":"2026-12-01T00:00:00.000Z","membershipType":"pro_plus",
              "individualUsage":{"plan":{"used":1500,"limit":2000,"breakdown":{"included":1240,"bonus":260}},
              "onDemand":{"enabled":true,"used":310,"limit":5000}}}"#),
        );
        assert_eq!(u.plan_name.as_deref(), Some("Pro Plus"));
        assert_eq!(u.resets_at.as_deref(), Some("2026-12-01T00:00:00.000Z"));
        assert_eq!(u.included_used_cents, Some(1240.0));
        assert_eq!(u.bonus_cents, Some(260.0));
        assert_eq!(u.on_demand_enabled, Some(true));
        assert_eq!(u.on_demand_used_cents, Some(310.0));
        assert_eq!(u.on_demand_limit_cents, Some(5000.0));
    }

    #[test]
    fn tolerates_missing_fields() {
        let mut u = AccountUsage::default();
        apply_plan_info(&mut u, &json("{}"));
        apply_current_period(&mut u, &json(r#"{"planUsage":{}}"#));
        apply_usage_summary(&mut u, &json(r#"{"membershipType":"free"}"#));
        assert_eq!(u.plan_name.as_deref(), Some("Free"));
        assert_eq!(u.used_percent, None);
        assert_eq!(u.resets_at, None);
    }

    /// Manual check against the account signed in to Cursor on this machine:
    /// `cargo test live_cursor_account -- --ignored --nocapture`.
    /// Prints only the parsed plan/usage, never the token.
    #[test]
    #[ignore]
    fn live_cursor_account() {
        let data_dir = crate::paths::active_data_dir().unwrap();
        let db = data_dir.join("User").join("globalStorage").join("state.vscdb");
        let snap = read_auth_keys(&db).unwrap();
        let u = tauri::async_runtime::block_on(fetch_usage(&snap, &data_dir, false));
        println!("{}", serde_json::to_string_pretty(&u).unwrap());
        assert_eq!(u.status, "ok", "{:?}", u.detail);
    }

    #[test]
    fn offline_falls_back_to_snapshot_plan() {
        let mut snap = AuthSnapshot::default();
        snap.keys.insert("cursorAuth/stripeMembershipType".into(), "premium".into());
        let u = tauri::async_runtime::block_on(fetch_usage(&snap, &std::env::temp_dir(), false));
        assert_eq!(u.status, "unavailable");
        assert_eq!(u.plan_name.as_deref(), Some("Premium"));
        assert_eq!(u.detail.as_deref(), Some("Not signed in"));
    }
}
