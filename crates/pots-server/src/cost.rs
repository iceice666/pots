//! 成本報表與單價表（`proposal/security-and-operations.md#可觀測性`）。
//!
//! 用量以 `agent_runs` 為唯一來源：內建 agent 在每一輪 `Hook::after_turn` 記帳，Observer／
//! Reflector 每次呼叫也寫一列，Claude Code 另外記 CLI 回報的 `total_cost_usd`。報表只做
//! 彙總與估價，不是帳單；訂閱用量不算金額，API 金額用單價表估算，CLI 費用標明是估計值。
//!
//! 單價表以 models.dev 的 provider／model 為鍵，`RefreshPricing` 重新抓取；手動設定的
//! `manual` 不會被覆蓋。`*` 是所有 provider 的後備單價。

use std::collections::HashMap;

use pots_proto::{
    Billing, CostCategory, CostReport, CostRow, CostTotals, GoalId, ModelPrice, PricingInfo,
    ProjectId, Timestamp,
};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;

use crate::Error;
use crate::store::Store;

/// models.dev 的完整資料；單價表只留模型與價錢。
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// 下載上限；目前約 6 MB，留很大的餘裕但不讓壞掉的端點吃光記憶體。
const MAX_PRICING_BYTES: usize = 32 * 1024 * 1024;
/// 預設報表區間：七天。
pub const DEFAULT_REPORT_SECS: i64 = 7 * 24 * 3600;

/// 產生一份成本報表。`since`、`until` 是 UNIX 秒，`project` 為 `None` 時包含所有 project
/// （含總管的內部 project）。
pub async fn report(
    store: &Store,
    project: Option<ProjectId>,
    since: Timestamp,
    until: Timestamp,
) -> Result<CostReport, Error> {
    let mut conn = store.acquire().await?;
    let now = until.0;
    let sql = "SELECT project_id, goal_id, agent, \
         COALESCE(provider, '未知') AS provider, \
         COALESCE(billing, 'unknown') AS billing, \
         COALESCE(pricing_provider, '') AS pricing_provider, \
         COALESCE(runtime, 'builtin') AS runtime, \
         COALESCE(model, '未知') AS model, \
         COUNT(*) AS runs, \
         COALESCE(SUM(input_tokens), 0) AS input_tokens, \
         COALESCE(SUM(output_tokens), 0) AS output_tokens, \
         COALESCE(SUM(CASE WHEN MIN(COALESCE(finished_at, ?), ?) > started_at \
             THEN MIN(COALESCE(finished_at, ?), ?) - started_at ELSE 0 END), 0) AS duration_secs, \
         COALESCE(SUM(cost_usd), 0) AS cost_usd \
         FROM agent_runs \
         WHERE started_at >= ? AND started_at <= ? AND state != 'queued' \
         AND (? IS NULL OR project_id = ?) \
         GROUP BY project_id, goal_id, agent, provider, billing, pricing_provider, runtime, model \
          ORDER BY project_id, goal_id, agent, provider, model";
    let project_filter = project.map(|p| p.to_string());
    let rows = sqlx::query(sql)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(since.0)
        .bind(until.0)
        .bind(&project_filter)
        .bind(&project_filter)
        .fetch_all(&mut *conn)
        .await?;

    let projects = project_names(&mut conn).await?;
    let goals = goal_titles(&mut conn).await?;
    let prices = load_prices(&mut conn).await?;
    let mut out = Vec::with_capacity(rows.len());
    let mut totals = CostTotals::default();
    for row in &rows {
        let project: ProjectId = crate::store::parse_id(row.try_get("project_id")?)?;
        let goal: Option<GoalId> = row
            .try_get::<Option<String>, _>("goal_id")?
            .map(crate::store::parse_id)
            .transpose()?;
        let agent: String = row.try_get("agent")?;
        let category = if matches!(agent.as_str(), "observer" | "reflector") {
            CostCategory::Om
        } else {
            CostCategory::Work
        };
        let provider: String = row.try_get("provider")?;
        let billing = match row.try_get::<String, _>("billing")?.as_str() {
            "subscription" => Billing::Subscription,
            "api" => Billing::Api,
            "cli" => Billing::Cli,
            _ => Billing::Unknown,
        };
        let pricing_provider: String = row.try_get("pricing_provider")?;
        let runtime: String = row.try_get("runtime")?;
        let model: String = row.try_get("model")?;
        let runs = row.try_get::<i64, _>("runs")?.max(0) as u64;
        let input = row.try_get::<i64, _>("input_tokens")?.max(0) as u64;
        let output = row.try_get::<i64, _>("output_tokens")?.max(0) as u64;
        let tokens = input.saturating_add(output);
        let duration = row.try_get::<i64, _>("duration_secs")?.max(0) as u64;
        let reported = row.try_get::<f64, _>("cost_usd").unwrap_or(0.0).max(0.0);
        // 訂閱用量不是帳單，不用單價換算；CLI 只顯示 runner 回報的估計；舊的 run
        // 沒有 billing 也不猜。
        let price = match billing {
            Billing::Api => prices.lookup(&pricing_provider, &model),
            _ => None,
        };
        let estimated = price.map(|p| {
            (input as f64 / 1_000_000.0) * p.input_per_mtok
                + (output as f64 / 1_000_000.0) * p.output_per_mtok
        });
        let priced = if estimated.is_some() { tokens } else { 0 };
        let row = CostRow {
            project,
            project_name: projects
                .get(&project)
                .cloned()
                .unwrap_or_else(|| "未知 project".into()),
            goal,
            goal_title: goal.and_then(|g| goals.get(&g).cloned()),
            agent: agent.clone(),
            category,
            provider,
            billing,
            runtime,
            model,
            runs,
            input_tokens: input,
            output_tokens: output,
            duration_secs: duration,
            reported_cost_usd: (reported > 0.0).then_some(reported),
            estimated_cost_usd: estimated,
            priced_tokens: priced,
            unpriced_tokens: tokens.saturating_sub(priced),
        };
        totals.runs += row.runs;
        totals.input_tokens += row.input_tokens;
        totals.output_tokens += row.output_tokens;
        totals.duration_secs += row.duration_secs;
        totals.reported_cost_usd += row.reported_cost_usd.unwrap_or(0.0);
        totals.estimated_cost_usd += row.estimated_cost_usd.unwrap_or(0.0);
        totals.priced_tokens += row.priced_tokens;
        totals.unpriced_tokens += row.unpriced_tokens;
        match row.billing {
            Billing::Subscription => totals.subscription_tokens += tokens,
            Billing::Api => totals.api_tokens += tokens,
            Billing::Cli => totals.cli_tokens += tokens,
            Billing::Unknown => {}
        }
        match row.category {
            CostCategory::Om => totals.om_tokens += tokens,
            CostCategory::Work => totals.work_tokens += tokens,
        }
        out.push(row);
    }
    let pricing = pricing_info_conn(&mut conn).await?;
    Ok(CostReport {
        since,
        until,
        generated_at: Timestamp::now(),
        rows: out,
        totals,
        pricing,
    })
}

async fn project_names(conn: &mut SqliteConnection) -> Result<HashMap<ProjectId, String>, Error> {
    let rows = sqlx::query("SELECT id, name, kind FROM projects")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = HashMap::new();
    for row in &rows {
        let id: ProjectId = crate::store::parse_id(row.try_get("id")?)?;
        let kind: String = row.try_get("kind")?;
        let name: String = row.try_get("name")?;
        out.insert(
            id,
            if kind == "overseer" {
                "總管（全域）".into()
            } else {
                name
            },
        );
    }
    Ok(out)
}

async fn goal_titles(conn: &mut SqliteConnection) -> Result<HashMap<GoalId, String>, Error> {
    let rows = sqlx::query("SELECT id, title FROM goals")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = HashMap::new();
    for row in &rows {
        out.insert(
            crate::store::parse_id(row.try_get("id")?)?,
            row.try_get("title")?,
        );
    }
    Ok(out)
}

struct Price {
    input_per_mtok: f64,
    output_per_mtok: f64,
}

#[derive(Default)]
struct Prices {
    exact: HashMap<(String, String), Price>,
    wildcard: HashMap<String, Price>,
    by_model: HashMap<String, Option<Price>>,
}

impl Prices {
    fn lookup(&self, provider: &str, model: &str) -> Option<&Price> {
        if let Some(p) = self.exact.get(&(provider.to_string(), model.to_string())) {
            return Some(p);
        }
        if let Some(p) = self.wildcard.get(model) {
            return Some(p);
        }
        // 自訂的 OpenAI 相容端點可能對不到 models.dev 的 provider；只有唯一一個模型時才用。
        self.by_model.get(model).and_then(|p| p.as_ref())
    }
}

async fn load_prices(conn: &mut SqliteConnection) -> Result<Prices, Error> {
    let rows = sqlx::query(
        "SELECT provider, model, input_per_mtok, output_per_mtok FROM model_prices \
         ORDER BY provider, model",
    )
    .fetch_all(conn)
    .await?;
    let mut prices = Prices::default();
    for row in rows {
        let provider: String = row.try_get("provider")?;
        let model: String = row.try_get("model")?;
        let price = Price {
            input_per_mtok: row.try_get("input_per_mtok")?,
            output_per_mtok: row.try_get("output_per_mtok")?,
        };
        if provider == "*" {
            prices.wildcard.insert(model.clone(), price);
        } else {
            prices
                .exact
                .insert((provider.clone(), model.clone()), price);
        }
    }
    // 同一個模型出現在多個 provider 時不能猜：只有唯一一個 provider 有紀錄才算。
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_, model) in prices.exact.keys() {
        *counts.entry(model.clone()).or_default() += 1;
    }
    prices.by_model.clear();
    for ((_, model), price) in &prices.exact {
        if counts.get(model).copied() == Some(1) {
            prices.by_model.insert(
                model.clone(),
                Some(Price {
                    input_per_mtok: price.input_per_mtok,
                    output_per_mtok: price.output_per_mtok,
                }),
            );
        } else {
            prices.by_model.insert(model.clone(), None);
        }
    }
    Ok(prices)
}

/// 單價表的摘要。
pub async fn pricing_info(store: &Store) -> Result<PricingInfo, Error> {
    let mut conn = store.acquire().await?;
    pricing_info_conn(&mut conn).await
}

async fn pricing_info_conn(conn: &mut SqliteConnection) -> Result<PricingInfo, Error> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS models, MAX(updated_at) AS updated_at, \
         COALESCE(SUM(CASE WHEN source = 'manual' THEN 1 ELSE 0 END), 0) AS manual, \
         COALESCE(SUM(CASE WHEN source = 'models.dev' THEN 1 ELSE 0 END), 0) AS fetched \
         FROM model_prices",
    )
    .fetch_one(conn)
    .await?;
    let models: i64 = row.try_get("models")?;
    let manual: i64 = row.try_get("manual")?;
    let fetched: i64 = row.try_get("fetched")?;
    Ok(PricingInfo {
        source: match (manual > 0, fetched > 0) {
            (true, true) => "models.dev + manual".into(),
            (true, false) => "manual".into(),
            (false, true) => "models.dev".into(),
            (false, false) => String::new(),
        },
        models: models.max(0) as u64,
        updated_at: row.try_get::<Option<i64>, _>("updated_at")?.map(Timestamp),
    })
}

/// 新增或覆寫一筆手動單價。
pub async fn set_price(store: &Store, price: &ModelPrice) -> Result<(), String> {
    check_price(price)?;
    let mut tx = store.begin().await.map_err(db_err)?;
    upsert_price(&mut tx, price, "manual", Timestamp::now()).await?;
    tx.commit().await.map_err(db)
}

/// 刪除一筆單價。
pub async fn delete_price(store: &Store, provider: &str, model: &str) -> Result<(), String> {
    let mut tx = store.begin().await.map_err(db_err)?;
    sqlx::query("DELETE FROM model_prices WHERE provider = ? AND model = ?")
        .bind(provider.trim())
        .bind(model.trim())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)
}

fn check_price(price: &ModelPrice) -> Result<(), String> {
    let provider = price.provider.trim();
    let model = price.model.trim();
    if provider.is_empty() || provider.len() > 100 {
        return Err("provider 不能是空的".into());
    }
    if model.is_empty() || model.len() > 200 {
        return Err("model 不能是空的".into());
    }
    for (name, value) in [
        ("input", price.input_per_mtok),
        ("output", price.output_per_mtok),
    ] {
        if !value.is_finite() || !(0.0..=1_000_000.0).contains(&value) {
            return Err(format!("{name} 單價不合理"));
        }
    }
    if let Some(cache) = price.cache_read_per_mtok
        && (!cache.is_finite() || !(0.0..=1_000_000.0).contains(&cache))
    {
        return Err("cache_read 單價不合理".into());
    }
    Ok(())
}

async fn upsert_price(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    price: &ModelPrice,
    source: &str,
    now: Timestamp,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO model_prices (provider, model, input_per_mtok, output_per_mtok, \
         cache_read_per_mtok, source, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (provider, model) DO UPDATE SET input_per_mtok = excluded.input_per_mtok, \
         output_per_mtok = excluded.output_per_mtok, cache_read_per_mtok = excluded.cache_read_per_mtok, \
         source = excluded.source, updated_at = excluded.updated_at",
    )
    .bind(price.provider.trim())
    .bind(price.model.trim())
    .bind(price.input_per_mtok)
    .bind(price.output_per_mtok)
    .bind(price.cache_read_per_mtok)
    .bind(source)
    .bind(now.0)
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    Ok(())
}

fn db(e: sqlx::Error) -> String {
    tracing::warn!(error = %e, "單價表寫入失敗");
    "伺服器內部錯誤".into()
}

fn db_err(e: Error) -> String {
    tracing::warn!(error = %e, "單價表寫入失敗");
    "伺服器內部錯誤".into()
}

/// 從 models.dev 重新抓單價表。只保留有 `cost` 的模型；舊的 `models.dev` 資料會換掉，
/// 手動設定的 `manual` 保留。
pub async fn refresh_pricing(store: &Store) -> Result<PricingInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("無法建立 HTTP client：{e}"))?;
    let resp = client
        .get(MODELS_DEV_URL)
        .send()
        .await
        .map_err(|e| format!("無法取得 models.dev：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("models.dev 回應 {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("讀取 models.dev 失敗：{e}"))?;
    if bytes.len() > MAX_PRICING_BYTES {
        return Err("models.dev 的回應太大".into());
    }
    let prices = parse_models_dev(&bytes)?;
    let now = Timestamp::now();
    let mut tx = store.begin().await.map_err(db_err)?;
    // 手動覆寫的不會被 models.dev 蓋掉（重新整理只換 models.dev 的資料）。
    let manual: std::collections::HashSet<(String, String)> =
        sqlx::query("SELECT provider, model FROM model_prices WHERE source = 'manual'")
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?
            .iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("provider")?,
                    row.try_get::<String, _>("model")?,
                ))
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(db)?;
    sqlx::query("DELETE FROM model_prices WHERE source = 'models.dev'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    for price in &prices {
        if manual.contains(&(price.provider.clone(), price.model.clone())) {
            continue;
        }
        upsert_price(&mut tx, price, "models.dev", now).await?;
    }
    tx.commit().await.map_err(db)?;
    let mut conn = store.acquire().await.map_err(db_err)?;
    pricing_info_conn(&mut conn).await.map_err(|e| {
        tracing::warn!(error = %e, "無法讀取單價表摘要");
        "伺服器內部錯誤".into()
    })
}

/// 解析 models.dev 的 JSON。公開給測試與 `RefreshPricing` 共用。
pub fn parse_models_dev(bytes: &[u8]) -> Result<Vec<ModelPrice>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("models.dev 的 JSON 格式不對：{e}"))?;
    let Some(providers) = value.as_object() else {
        return Err("models.dev 的格式不對".into());
    };
    let mut out = Vec::new();
    for (provider, data) in providers {
        let Some(models) = data.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        for (model, data) in models {
            let Some(cost) = data.get("cost") else {
                continue;
            };
            let (Some(input), Some(output)) = (
                cost.get("input").and_then(|v| v.as_f64()),
                cost.get("output").and_then(|v| v.as_f64()),
            ) else {
                continue;
            };
            if !input.is_finite() || !output.is_finite() || input < 0.0 || output < 0.0 {
                continue;
            }
            out.push(ModelPrice {
                provider: provider.clone(),
                model: model.clone(),
                input_per_mtok: input,
                output_per_mtok: output,
                cache_read_per_mtok: cost.get("cache_read").and_then(|v| v.as_f64()),
            });
        }
    }
    Ok(out)
}
