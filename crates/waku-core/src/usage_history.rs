//! Historical token and cost usage for the settings Usage page.
//!
//! The daemon appends one billed row per assistant HTTP response. This module
//! prices those rows against LiteLLM's model rate table and folds them into
//! the page snapshot. It never reads harness snapshots or CLI transcripts.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, NaiveDate, TimeZone as _, Utc};
use serde_json::Value;
use uuid::Uuid;

use waku_protocol::provider::ProviderId;

pub use waku_protocol::usage_history::{
    CostQuality, DaySlice, MONTHLY_WINDOW, ModelSlice, MonthSlice, PricingStatus, ProjectSlice,
    ProviderDay, ProviderSlice, TokenTotals, UsageHistory, UsageWindow, WINDOW_CHOICES,
};

const LITELLM_RATES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const RATES_CACHE_FILE: &str = "usage-model-rates.json";
const RATES_TTL: Duration = Duration::from_secs(24 * 3600);

/// One billed assistant HTTP response persisted by the daemon.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageEvent {
    pub event_id: Uuid,
    pub session_id: Uuid,
    pub project_path: String,
    pub provider: ProviderId,
    pub model: String,
    pub timestamp_ms: i64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: Option<u64>,
}

impl UsageEvent {
    pub fn token_total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    pub fn totals(&self) -> TokenTotals {
        TokenTotals {
            uncached_input: self.input,
            cached_input: self.cache_read,
            cache_creation: self.cache_write,
            output: self.output,
            reasoning: self.reasoning.unwrap_or(0),
        }
    }
}

pub fn window_millis(window: UsageWindow, today: NaiveDate) -> (i64, i64) {
    let (since_day, until_day) = window.bounds(today);
    (
        start_of_local_day_ms(since_day),
        end_of_local_day_ms(until_day),
    )
}

fn start_of_local_day_ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
        .map(|midnight| midnight.timestamp_millis())
        .unwrap_or(0)
}

fn end_of_local_day_ms(day: NaiveDate) -> i64 {
    day.and_hms_milli_opt(23, 59, 59, 999)
        .and_then(|end| Local.from_local_datetime(&end).latest())
        .map(|end| end.timestamp_millis())
        .unwrap_or(i64::MAX)
}

/* ------------------------------------------------------------------------- */
/* Pricing                                                                   */
/* ------------------------------------------------------------------------- */

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelRate {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
}

#[derive(Clone, Debug)]
pub struct RateTable {
    pub rates: HashMap<String, ModelRate>,
    pub status: PricingStatus,
}

impl RateTable {
    pub fn unavailable() -> Self {
        Self {
            rates: HashMap::new(),
            status: PricingStatus::Unavailable,
        }
    }
}

const UNPRICEABLE_MODELS: [&str; 6] = [
    "<synthetic>",
    "synthetic",
    "opus",
    "sonnet",
    "haiku",
    "fable",
];

fn normalize_model_name(model: &str) -> String {
    let trimmed = model.trim().to_ascii_lowercase();
    match trimmed.rfind('/') {
        Some(slash) => trimmed[slash + 1..].to_owned(),
        None => trimmed,
    }
}

fn lookup_rate<'a>(table: &'a RateTable, model: &str) -> Option<&'a ModelRate> {
    let normalized = normalize_model_name(model);
    if normalized.is_empty() || UNPRICEABLE_MODELS.contains(&normalized.as_str()) {
        return None;
    }
    table.rates.get(&normalized)
}

fn parse_rate_table(document: &Value) -> HashMap<String, ModelRate> {
    let mut table = HashMap::new();
    let Some(entries) = document.as_object() else {
        return table;
    };
    let finite = |value: Option<&Value>| value.and_then(Value::as_f64).filter(|v| v.is_finite());
    for (name, entry) in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(input) = finite(entry.get("input_cost_per_token")) else {
            continue;
        };
        let Some(output) = finite(entry.get("output_cost_per_token")) else {
            continue;
        };
        table.insert(
            normalize_model_name(name),
            ModelRate {
                input,
                output,
                cache_read: finite(entry.get("cache_read_input_token_cost")).unwrap_or(input),
                cache_creation: finite(entry.get("cache_creation_input_token_cost"))
                    .unwrap_or(input),
            },
        );
    }
    table
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn read_rates_cache(path: &Path) -> Option<(i64, HashMap<String, ModelRate>)> {
    let document: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let fetched_at_ms = document.get("fetched_at_ms")?.as_i64()?;
    let mut rates = HashMap::new();
    for (name, entry) in document.get("rates")?.as_object()? {
        let values = entry.as_array()?;
        let field = |index: usize| values.get(index).and_then(Value::as_f64);
        rates.insert(
            name.clone(),
            ModelRate {
                input: field(0)?,
                output: field(1)?,
                cache_read: field(2)?,
                cache_creation: field(3)?,
            },
        );
    }
    Some((fetched_at_ms, rates))
}

fn write_rates_cache(path: &Path, fetched_at_ms: i64, rates: &HashMap<String, ModelRate>) {
    let entries: serde_json::Map<String, Value> = rates
        .iter()
        .map(|(name, rate)| {
            (
                name.clone(),
                serde_json::json!([
                    rate.input,
                    rate.output,
                    rate.cache_read,
                    rate.cache_creation
                ]),
            )
        })
        .collect();
    let document = serde_json::json!({ "fetched_at_ms": fetched_at_ms, "rates": entries });
    let _ = std::fs::write(path, document.to_string());
}

pub fn load_rate_table(cache_dir: &Path) -> RateTable {
    let cache_path = cache_dir.join(RATES_CACHE_FILE);
    let disk = read_rates_cache(&cache_path).filter(|(_, rates)| !rates.is_empty());
    let now_ms = unix_time_ms();
    if let Some((fetched_at_ms, rates)) = &disk
        && now_ms.saturating_sub(*fetched_at_ms) < RATES_TTL.as_millis() as i64
    {
        return RateTable {
            rates: rates.clone(),
            status: PricingStatus::Cached,
        };
    }

    let fetched =
        crate::usage::http_get(LITELLM_RATES_URL, &["Accept: application/json".to_owned()])
            .ok()
            .filter(|(status, _)| *status == 200)
            .and_then(|(_, body)| serde_json::from_str::<Value>(&body).ok())
            .map(|document| parse_rate_table(&document))
            .filter(|rates| !rates.is_empty());

    match (fetched, disk) {
        (Some(rates), _) => {
            write_rates_cache(&cache_path, now_ms, &rates);
            RateTable {
                rates,
                status: PricingStatus::Fresh,
            }
        }
        (None, Some((_, rates))) => RateTable {
            rates,
            status: PricingStatus::Cached,
        },
        (None, None) => RateTable::unavailable(),
    }
}

/* ------------------------------------------------------------------------- */
/* Fold                                                                      */
/* ------------------------------------------------------------------------- */

enum CostSource {
    Priced,
    Unpriced,
}

#[derive(Default)]
struct Bucket {
    totals: TokenTotals,
    cost_usd: f64,
    cache_savings_usd: f64,
    records: u64,
    unpriced_records: u64,
}

#[derive(Default)]
struct ProjectAccumulator {
    cost_usd: f64,
    total_tokens: u64,
    by_provider: HashMap<ProviderId, ProviderDay>,
    sessions: HashSet<Uuid>,
    models: HashMap<String, f64>,
    last_day: Option<NaiveDate>,
}

struct Aggregator {
    buckets: HashMap<(NaiveDate, ProviderId, String), Bucket>,
    sessions: HashSet<Uuid>,
    month_sessions: HashSet<(NaiveDate, Uuid)>,
    projects: HashMap<String, ProjectAccumulator>,
}

impl Aggregator {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            sessions: HashSet::new(),
            month_sessions: HashSet::new(),
            projects: HashMap::new(),
        }
    }

    fn add(&mut self, event: &UsageEvent, rates: &RateTable) {
        if event.token_total() == 0 {
            return;
        }
        let Some(timestamp) = Utc.timestamp_millis_opt(event.timestamp_ms).single() else {
            return;
        };
        let day = timestamp.with_timezone(&Local).date_naive();
        let totals = event.totals();
        let (cost_usd, source) = match lookup_rate(rates, &event.model) {
            Some(rate) => (
                totals.uncached_input as f64 * rate.input
                    + totals.cached_input as f64 * rate.cache_read
                    + totals.cache_creation as f64 * rate.cache_creation
                    + totals.output as f64 * rate.output,
                CostSource::Priced,
            ),
            None => (0.0, CostSource::Unpriced),
        };
        let cache_savings_usd = lookup_rate(rates, &event.model)
            .map(|rate| totals.cached_input as f64 * (rate.input - rate.cache_read))
            .unwrap_or(0.0);

        let bucket = self
            .buckets
            .entry((day, event.provider.clone(), event.model.clone()))
            .or_default();
        bucket.totals.add(&totals);
        bucket.cost_usd += cost_usd;
        bucket.cache_savings_usd += cache_savings_usd;
        bucket.records += 1;
        if matches!(source, CostSource::Unpriced) {
            bucket.unpriced_records += 1;
        }
        self.sessions.insert(event.session_id);
        self.month_sessions
            .insert((first_of_month(day), event.session_id));

        let tokens = totals.total();
        let project = self.projects.entry(event.project_path.clone()).or_default();
        project.cost_usd += cost_usd;
        project.total_tokens += tokens;
        let day_row = project
            .by_provider
            .entry(event.provider.clone())
            .or_insert_with(|| ProviderDay {
                provider: event.provider.clone(),
                cost_usd: 0.0,
                total_tokens: 0,
            });
        day_row.cost_usd += cost_usd;
        day_row.total_tokens += tokens;
        project.sessions.insert(event.session_id);
        *project.models.entry(event.model.clone()).or_default() += cost_usd;
        project.last_day = Some(project.last_day.map_or(day, |last| last.max(day)));
    }
}

pub fn fold(rates: &RateTable, window: UsageWindow, events: &[UsageEvent]) -> UsageHistory {
    let (since_day, until_day) = window.bounds(Local::now().date_naive());
    let mut aggregator = Aggregator::new();
    for event in events {
        aggregator.add(event, rates);
    }
    derive_history(aggregator, window, since_day, until_day, rates.status)
}

fn derive_history(
    aggregator: Aggregator,
    window: UsageWindow,
    since_day: NaiveDate,
    until_day: NaiveDate,
    pricing: PricingStatus,
) -> UsageHistory {
    let mut totals = TokenTotals::default();
    let mut cost_usd = 0.0;
    let mut cache_savings_usd = 0.0;
    let mut records = 0;
    let mut unpriced_records = 0;
    let mut providers: HashMap<ProviderId, (f64, u64)> = HashMap::new();
    let mut models: HashMap<(ProviderId, String), (f64, u64)> = HashMap::new();
    let mut daily: HashMap<NaiveDate, DaySlice> = HashMap::new();
    let mut month_models: HashMap<(NaiveDate, String), f64> = HashMap::new();

    for ((day, provider, model), bucket) in &aggregator.buckets {
        let tokens = bucket.totals.total();
        totals.add(&bucket.totals);
        cost_usd += bucket.cost_usd;
        cache_savings_usd += bucket.cache_savings_usd;
        records += bucket.records;
        unpriced_records += bucket.unpriced_records;

        let provider_entry = providers.entry(provider.clone()).or_default();
        provider_entry.0 += bucket.cost_usd;
        provider_entry.1 += tokens;

        let model_entry = models.entry((provider.clone(), model.clone())).or_default();
        model_entry.0 += bucket.cost_usd;
        model_entry.1 += tokens;

        let day_entry = daily.entry(*day).or_insert_with(|| DaySlice {
            day: *day,
            cost_usd: 0.0,
            total_tokens: 0,
            by_provider: Vec::new(),
        });
        day_entry.cost_usd += bucket.cost_usd;
        day_entry.total_tokens += tokens;
        add_provider_day(
            &mut day_entry.by_provider,
            provider,
            bucket.cost_usd,
            tokens,
        );
        *month_models
            .entry((first_of_month(*day), model.clone()))
            .or_default() += bucket.cost_usd;
    }

    let total_tokens = totals.total();
    let share = |part: f64, whole: f64| if whole == 0.0 { 0.0 } else { part / whole };

    let mut provider_slices: Vec<ProviderSlice> = providers
        .into_iter()
        .map(
            |(provider, (provider_cost, provider_tokens))| ProviderSlice {
                provider,
                cost_usd: provider_cost,
                total_tokens: provider_tokens,
                cost_share: share(provider_cost, cost_usd),
                token_share: share(provider_tokens as f64, total_tokens as f64),
            },
        )
        .collect();
    provider_slices.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.total_tokens.cmp(&a.total_tokens))
            .then(a.provider.as_str().cmp(b.provider.as_str()))
    });

    let mut model_slices: Vec<ModelSlice> = models
        .into_iter()
        .map(
            |((provider, model), (model_cost, model_tokens))| ModelSlice {
                provider,
                model,
                cost_usd: model_cost,
                total_tokens: model_tokens,
                cost_share: share(model_cost, cost_usd),
            },
        )
        .collect();
    model_slices.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.total_tokens.cmp(&a.total_tokens))
    });

    let mut day_slices: Vec<DaySlice> = daily.into_values().collect();
    day_slices.sort_by_key(|slice| slice.day);
    for day in &mut day_slices {
        sort_provider_days(&mut day.by_provider);
    }

    let mut months: HashMap<NaiveDate, MonthSlice> = HashMap::new();
    for day in &day_slices {
        let month = months
            .entry(first_of_month(day.day))
            .or_insert_with(|| MonthSlice {
                first_day: first_of_month(day.day),
                cost_usd: 0.0,
                total_tokens: 0,
                by_provider: Vec::new(),
                sessions: 0,
                active_days: 0,
                top_models: Vec::new(),
            });
        month.cost_usd += day.cost_usd;
        month.total_tokens += day.total_tokens;
        for entry in &day.by_provider {
            add_provider_day(
                &mut month.by_provider,
                &entry.provider,
                entry.cost_usd,
                entry.total_tokens,
            );
        }
        if day.total_tokens > 0 {
            month.active_days += 1;
        }
    }
    for (first_day, _) in &aggregator.month_sessions {
        if let Some(month) = months.get_mut(first_day) {
            month.sessions += 1;
        }
    }
    for ((first_day, model), model_cost) in month_models {
        if let Some(month) = months.get_mut(&first_day) {
            month.top_models.push((model, model_cost));
        }
    }
    let mut month_slices: Vec<MonthSlice> = months.into_values().collect();
    month_slices.sort_by_key(|slice| slice.first_day);
    for month in &mut month_slices {
        month.top_models.sort_by(|a, b| b.1.total_cmp(&a.1));
        sort_provider_days(&mut month.by_provider);
    }

    let mut project_slices: Vec<ProjectSlice> = aggregator
        .projects
        .into_iter()
        .map(|(path, project)| {
            let mut top_models: Vec<(String, f64)> = project.models.into_iter().collect();
            top_models.sort_by(|a, b| b.1.total_cmp(&a.1));
            let mut by_provider: Vec<ProviderDay> = project.by_provider.into_values().collect();
            sort_provider_days(&mut by_provider);
            ProjectSlice {
                path,
                cost_usd: project.cost_usd,
                total_tokens: project.total_tokens,
                by_provider,
                sessions: project.sessions.len() as u64,
                cost_share: share(project.cost_usd, cost_usd),
                last_day: project.last_day,
                top_models,
            }
        })
        .collect();
    project_slices.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.total_tokens.cmp(&a.total_tokens))
    });

    let record_share = |part: u64| share(part as f64, records as f64);
    UsageHistory {
        window,
        since_day,
        until_day,
        totals,
        total_tokens,
        cost_usd,
        records,
        sessions: aggregator.sessions.len() as u64,
        providers: provider_slices,
        models: model_slices,
        daily: day_slices,
        months: month_slices,
        projects: project_slices,
        quality: CostQuality {
            provider_reported_share: 0.0,
            model_priced_share: record_share(records - unpriced_records),
            unpriced_share: record_share(unpriced_records),
            cache_savings_usd,
        },
        pricing,
    }
}

fn add_provider_day(
    rows: &mut Vec<ProviderDay>,
    provider: &ProviderId,
    cost_usd: f64,
    total_tokens: u64,
) {
    if let Some(existing) = rows.iter_mut().find(|row| &row.provider == provider) {
        existing.cost_usd += cost_usd;
        existing.total_tokens += total_tokens;
        return;
    }
    rows.push(ProviderDay {
        provider: provider.clone(),
        cost_usd,
        total_tokens,
    });
}

fn sort_provider_days(rows: &mut [ProviderDay]) {
    rows.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then(b.total_tokens.cmp(&a.total_tokens))
            .then(a.provider.as_str().cmp(b.provider.as_str()))
    });
}

pub fn enumerate_days(since_day: NaiveDate, until_day: NaiveDate) -> Vec<NaiveDate> {
    waku_protocol::usage_history::enumerate_days(since_day, until_day)
}

pub fn first_of_month(day: NaiveDate) -> NaiveDate {
    waku_protocol::usage_history::first_of_month(day)
}

pub fn enumerate_months(since_day: NaiveDate, until_day: NaiveDate) -> Vec<NaiveDate> {
    waku_protocol::usage_history::enumerate_months(since_day, until_day)
}

pub fn days_in_month(first_day: NaiveDate) -> u32 {
    waku_protocol::usage_history::days_in_month(first_day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT_RATE: ModelRate = ModelRate {
        input: 1e-6,
        output: 2e-6,
        cache_read: 1e-7,
        cache_creation: 1.25e-6,
    };

    fn rates() -> RateTable {
        RateTable {
            rates: HashMap::from([("claude-fable-5".into(), FLAT_RATE)]),
            status: PricingStatus::Fresh,
        }
    }

    fn event(id: u128, provider: &str, model: &str, input: u64, output: u64) -> UsageEvent {
        UsageEvent {
            event_id: Uuid::from_u128(id),
            session_id: Uuid::from_u128(id / 10 + 1),
            project_path: "/tmp/waku".into(),
            provider: ProviderId::new(provider),
            model: model.into(),
            timestamp_ms: Local::now().timestamp_millis(),
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: None,
        }
    }

    #[test]
    fn fold_prices_one_billed_event() {
        let history = fold(
            &rates(),
            UsageWindow::TrailingDays(30),
            &[event(10, "anthropic", "claude-fable-5", 10, 4)],
        );
        assert_eq!(history.records, 1);
        assert_eq!(history.total_tokens, 14);
        assert_eq!(history.providers[0].provider.as_str(), "anthropic");
        assert!((history.quality.model_priced_share - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_token_events_are_ignored_by_fold() {
        let history = fold(
            &rates(),
            UsageWindow::TrailingDays(30),
            &[event(11, "anthropic", "claude-fable-5", 0, 0)],
        );
        assert_eq!(history.records, 0);
    }

    #[test]
    fn token_total_saturates_instead_of_wrapping() {
        let mut event = event(12, "anthropic", "claude-fable-5", u64::MAX, u64::MAX);
        event.cache_read = u64::MAX;
        event.cache_write = u64::MAX;
        assert_eq!(event.token_total(), u64::MAX);
        assert_eq!(event.totals().total(), u64::MAX);
    }

    #[test]
    fn three_providers_emit_three_rows() {
        let history = fold(
            &RateTable::unavailable(),
            UsageWindow::TrailingDays(30),
            &[
                event(20, "anthropic", "claude-fable-5", 2, 0),
                event(21, "openai-responses", "gpt-5.3", 2, 0),
                event(22, "xai", "grok-4", 2, 0),
            ],
        );
        assert_eq!(history.providers.len(), 3);
        assert_eq!(history.daily[0].by_provider.len(), 3);
    }

    #[test]
    fn model_names_normalize_and_family_names_stay_unpriced() {
        assert_eq!(
            normalize_model_name("anthropic/Claude-Fable-5"),
            "claude-fable-5"
        );
        let table = rates();
        assert!(lookup_rate(&table, "Fable").is_none());
        assert!(lookup_rate(&table, "anthropic/claude-fable-5").is_some());
    }

    #[test]
    fn rate_tables_parse_and_round_trip_through_the_disk_cache() {
        let document: Value = serde_json::from_str(
            r#"{
                "claude-fable-5": {"input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
                "no-output-rate": {"input_cost_per_token": 1e-6}
            }"#,
        )
        .unwrap();
        let parsed = parse_rate_table(&document);
        assert_eq!(parsed.len(), 1);
        let dir = std::env::temp_dir().join(format!("waku-usage-rates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RATES_CACHE_FILE);
        write_rates_cache(&path, 1, &parsed);
        let (fetched_at_ms, restored) = read_rates_cache(&path).expect("cache");
        assert_eq!(fetched_at_ms, 1);
        assert_eq!(restored["claude-fable-5"], parsed["claude-fable-5"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
