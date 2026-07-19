//! `stats` — per-model leaderboard from the event store.
//!
//! Aggregates sessions, ratings, costs, tokens, and failures by the selector's identity
//! (`canonical_key`). By default rows are keyed by the blind `model_token` so the user can compare
//! without learning identifying names. `--reveal` unmasks real slugs through the audited reveal gate
//! and journals each unmask to the `reveals` table.

use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::Args;

use alias::{Alias, RevealGate, RevealReason};
use backend::ErrorKind;
use config::Config;
use selector::{
    fold_track_record_with_failures, normalize_prices, value_score, Failure, Rating, TrackRecord,
    Tuneables,
};
use store::Store;

/// `blindcoder stats`: print a per-model quality/cost/value leaderboard.
#[derive(Args)]
pub struct StatsArgs {
    /// Sort column (default: value).
    #[arg(long, value_name = "COL", default_value = "value")]
    sort: String,
    /// Reverse the sort order.
    #[arg(long)]
    asc: bool,
    /// Show real model slugs instead of blind tokens and journal the reveal.
    #[arg(long)]
    reveal: bool,
}

/// One rendered row of the leaderboard.
struct StatsRow {
    display_name: String,
    alias: String,
    sessions: i64,
    rated: i64,
    avg_performance: Option<f64>,
    avg_difficulty: Option<f64>,
    quality: f64,
    value: f64,
    total_cost: f64,
    avg_cost: Option<f64>,
    prompt_tokens: i64,
    completion_tokens: i64,
    failures: Vec<(String, i64)>,
}

impl StatsRow {
    fn total_tokens(&self) -> i64 {
        self.prompt_tokens + self.completion_tokens
    }

    fn failure_string(&self) -> String {
        if self.failures.is_empty() {
            return String::new();
        }
        self.failures
            .iter()
            .map(|(k, n)| format!("{}:{}", k, n))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Open the default file-backed store.
fn open_store() -> Result<Store> {
    let dir = config::default_data_dir()
        .context("cannot determine data dir (set XDG_DATA_HOME or HOME)")?;
    Store::open(&dir.join("blindcoder.db"))
}

/// Blend input/output shelf prices into a single model price using the configured cost basis.
fn blended_price(input: Option<f64>, output: Option<f64>, basis: &config::CostBasis) -> f64 {
    input.unwrap_or(0.0) * basis.input_weight + output.unwrap_or(0.0) * basis.output_weight
}

/// Raw price per canonical_key, averaged over the latest provider observations for that key.
fn raw_prices_for_rows(
    latest: &[store::PricePoint],
    keys: &[String],
    basis: &config::CostBasis,
) -> Vec<f64> {
    let mut sums: HashMap<String, (f64, usize)> = HashMap::new();
    for p in latest {
        let blended = blended_price(p.input_per_mtok, p.output_per_mtok, basis);
        let entry = sums.entry(p.canonical_key.clone()).or_insert((0.0, 0));
        entry.0 += blended;
        entry.1 += 1;
    }
    keys.iter()
        .map(|k| match sums.get(k) {
            Some((sum, n)) if *n > 0 => sum / *n as f64,
            _ => 0.0,
        })
        .collect()
}

/// Build the failure-weight vector for one canonical key from the store's aged failures.
fn failures_for_key<'a>(
    failures: &'a [store::AgedFailure],
    key: &'a str,
) -> impl Iterator<Item = Failure> + 'a {
    failures
        .iter()
        .filter(move |f| f.canonical_key == key)
        .map(|f| {
            let loss_weight = ErrorKind::from_wire(&f.error_kind)
                .unwrap_or(ErrorKind::Unknown)
                .loss_weight();
            Failure {
                loss_weight,
                age_days: f.age_days,
            }
        })
}

/// Fold the effective ratings + failures for one canonical key into the selector's current belief.
fn track_record_for_key(
    ratings: &[store::AgedRating],
    failures: &[store::AgedFailure],
    key: &str,
    t: &Tuneables,
) -> TrackRecord {
    let key_ratings: Vec<Rating> = ratings
        .iter()
        .filter(|r| r.canonical_key == key)
        .map(|r| Rating {
            performance_points: r.performance_points,
            difficulty_points: r.difficulty_points,
            age_days: r.age_days,
        })
        .collect();
    let key_failures: Vec<Failure> = failures_for_key(failures, key).collect();

    if key_ratings.is_empty() && key_failures.is_empty() {
        TrackRecord::blank()
    } else {
        fold_track_record_with_failures(&key_ratings, &key_failures, t)
    }
}

/// Collect and compute leaderboard rows from the store.
fn gather_rows(store: &Store, cfg: &Config) -> Result<Vec<StatsRow>> {
    let aggregates = store.model_aggregates()?;
    if aggregates.is_empty() {
        return Ok(Vec::new());
    }

    let t = cfg.tuneables();
    let aged_ratings = store.effective_ratings_aged()?;
    let aged_failures = store.effective_failures_aged()?;

    let keys: Vec<String> = aggregates.iter().map(|a| a.canonical_key.clone()).collect();
    let raw_prices = raw_prices_for_rows(&store.latest_prices()?, &keys, &cfg.cost_basis);
    let normalized = normalize_prices(&raw_prices);

    let mut rows = Vec::with_capacity(aggregates.len());
    for (i, agg) in aggregates.into_iter().enumerate() {
        let track = track_record_for_key(&aged_ratings, &aged_failures, &agg.canonical_key, &t);
        let quality = track.mean();
        let value = value_score(quality, normalized[i], &t);

        rows.push(StatsRow {
            display_name: if agg.model_token.is_empty() {
                "—".to_string()
            } else {
                agg.model_token
            },
            alias: agg.alias,
            sessions: agg.sessions,
            rated: agg.rated,
            avg_performance: agg.avg_performance,
            avg_difficulty: agg.avg_difficulty,
            quality,
            value,
            total_cost: agg.total_cost,
            avg_cost: agg.avg_cost,
            prompt_tokens: agg.total_prompt_tokens,
            completion_tokens: agg.total_completion_tokens,
            failures: agg
                .failures
                .into_iter()
                .map(|f| (f.error_kind, f.count))
                .collect(),
        });
    }
    Ok(rows)
}

fn sort_rows(rows: &mut [StatsRow], sort: &str, asc: bool) {
    let cmp: fn(&StatsRow, &StatsRow) -> std::cmp::Ordering = match sort {
        "value" => |a, b| {
            b.value
                .partial_cmp(&a.value)
                .unwrap_or(std::cmp::Ordering::Equal)
        },
        "quality" => |a, b| {
            b.quality
                .partial_cmp(&a.quality)
                .unwrap_or(std::cmp::Ordering::Equal)
        },
        "cost" => |a, b| {
            b.total_cost
                .partial_cmp(&a.total_cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        },
        "sessions" => |a, b| b.sessions.cmp(&a.sessions),
        _ => |a, b| {
            b.value
                .partial_cmp(&a.value)
                .unwrap_or(std::cmp::Ordering::Equal)
        },
    };
    rows.sort_by(|a, b| {
        let ord = cmp(a, b);
        if asc {
            ord.reverse()
        } else {
            ord
        }
    });
}

fn format_num<T: std::fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "—".to_string(),
    }
}

/// Render the rows as a plain aligned text table. The model column remains blind unless reveal.
fn print_table(rows: &[StatsRow], reveal: bool) {
    let model_header = if reveal { "Model (revealed)" } else { "Model" };
    let headers = [
        model_header,
        "Sessions",
        "Rated",
        "Avg perf",
        "Avg diff",
        "Quality",
        "Value",
        "Total $",
        "Avg $",
        "Tokens",
        "Failures",
    ];

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    let col_strings: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.display_name.clone(),
                r.sessions.to_string(),
                r.rated.to_string(),
                format_num(r.avg_performance.map(|v| format!("{v:.1}"))),
                format_num(r.avg_difficulty.map(|v| format!("{v:.1}"))),
                format!("{:.3}", r.quality),
                format!("{:.3}", r.value),
                format!("{:.4}", r.total_cost),
                format_num(r.avg_cost.map(|v| format!("{v:.4}"))),
                r.total_tokens().to_string(),
                r.failure_string(),
            ]
        })
        .collect();

    for cols in &col_strings {
        for (i, c) in cols.iter().enumerate() {
            widths[i] = widths[i].max(c.len());
        }
    }

    let mut sep_parts = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        sep_parts.push(format!("{:-<width$}", "", width = widths[i].max(h.len())));
    }
    let sep = sep_parts.join("+");

    let print_row = |cells: &[String]| {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:>width$}", width = widths[i]))
            .collect();
        println!("{}", padded.join(" "));
    };

    print_row(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    println!("{sep}");
    for cols in &col_strings {
        print_row(cols);
    }
}

fn alias_from_display(display: &str) -> Result<Alias> {
    let (provider, model) = display
        .split_once(':')
        .with_context(|| format!("stored alias {display} is missing ':' separator"))?;
    Ok(Alias {
        provider_token: provider.to_string(),
        model_token: model.to_string(),
    })
}

/// Run the `stats` subcommand.
pub fn run(args: &StatsArgs, _cfg: &Config) -> Result<()> {
    let store = open_store()?;
    let mut rows = gather_rows(&store, _cfg)?;

    if rows.is_empty() {
        println!("No sessions recorded yet.");
        return Ok(());
    }

    if args.reveal {
        // Unmask through the reveal gate — the single audited crossing point. The gate's lookup
        // resolves the real slug from the catalog; we display only what the gate hands back (never
        // a direct read that bypasses it), and journal each actual crossing.
        let slugs: HashMap<String, Option<String>> = store
            .model_aggregates()?
            .into_iter()
            .map(|a| (a.alias, a.real_slug))
            .collect();
        let gate = RevealGate;
        for r in &mut rows {
            let alias = alias_from_display(&r.alias)?;
            if let Some(slug) = gate.reveal(&alias, RevealReason::Stats, |a| {
                slugs.get(&a.display()).cloned().flatten()
            }) {
                r.display_name = slug;
                store.record_reveal(&r.alias, None, "stats")?;
            }
        }
    }

    sort_rows(&mut rows, &args.sort, args.asc);
    print_table(&rows, args.reveal);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::CostBasis;
    use std::path::PathBuf;
    use store::Store;

    fn seeded_store(store: &Store) {
        store
            .upsert_provider("prov", "http://p.test/v1", "openai")
            .unwrap();
        store
            .upsert_model("acme/cheap", "prov", "prov/cheap")
            .unwrap();
        store
            .upsert_model("acme/pricey", "prov", "prov/pricey")
            .unwrap();

        // aliases with distinct model tokens
        store
            .conn
            .execute(
                "INSERT INTO aliases (alias, provider_token, model_token, canonical_key, provider_slug)
                 VALUES ('pt:cheap', 'pt', 'cheap', 'acme/cheap', 'prov')",
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO aliases (alias, provider_token, model_token, canonical_key, provider_slug)
                 VALUES ('pt:pricey', 'pt', 'pricey', 'acme/pricey', 'prov')",
                [],
            )
            .unwrap();

        // Seed latest prices.
        store
            .record_price_if_changed("acme/cheap", "prov", Some(0.1), Some(0.4))
            .unwrap();
        store
            .record_price_if_changed("acme/pricey", "prov", Some(1.0), Some(4.0))
            .unwrap();

        let sid_c1 = store
            .record_session_start("pt:cheap", Some("opencode"), None, "metadata")
            .unwrap();
        let sid_c2 = store
            .record_session_start("pt:cheap", Some("opencode"), None, "metadata")
            .unwrap();
        let sid_p = store
            .record_session_start("pt:pricey", Some("opencode"), None, "metadata")
            .unwrap();

        // cheap model: two sessions, two positive ratings.
        store.record_rating(sid_c1, 2, 2, None).unwrap();
        store.record_rating(sid_c2, 1, 1, None).unwrap();
        // pricey model: one failure, no ratings.
        store
            .record_session_end(
                sid_p,
                Some(5.0),
                Some("provider"),
                Some(1000),
                Some(500),
                Some("too_large"),
                Some(413),
                None,
            )
            .unwrap();
        // cheap sessions end cheaply.
        for sid in [sid_c1, sid_c2] {
            store
                .record_session_end(
                    sid,
                    Some(0.1),
                    Some("provider"),
                    Some(100),
                    Some(50),
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
    }

    #[test]
    fn gather_orders_by_value_desc_by_default() {
        let store = Store::open_in_memory().unwrap();
        seeded_store(&store);

        let cfg = Config {
            cost_basis: CostBasis {
                input_weight: 0.7,
                output_weight: 0.3,
            },
            ..Config::default()
        };
        let mut rows = gather_rows(&store, &cfg).unwrap();
        sort_rows(&mut rows, "value", false);
        assert_eq!(rows.len(), 2);
        let names: Vec<&str> = rows.iter().map(|r| r.display_name.as_str()).collect();
        assert_eq!(names, vec!["cheap", "pricey"]);
        assert!(rows[0].value > rows[1].value);
        assert_eq!(rows[0].sessions, 2);
        assert_eq!(rows[1].sessions, 1);
        assert_eq!(rows[1].failures.len(), 1);
        assert_eq!(rows[1].failures[0].0, "too_large");
    }

    #[test]
    fn empty_store_returns_no_rows() {
        let store = Store::open_in_memory().unwrap();
        let rows = gather_rows(&store, &Config::default()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn model_aggregates_and_reveals_work_on_file_db() {
        let dir = tempfile::tempdir().unwrap();
        let path: PathBuf = dir.path().join("stats_test.db");
        let store = Store::open(&path).unwrap();
        seeded_store(&store);

        let cfg = Config {
            cost_basis: CostBasis {
                input_weight: 0.7,
                output_weight: 0.3,
            },
            ..Config::default()
        };
        let mut rows = gather_rows(&store, &cfg).unwrap();
        sort_rows(&mut rows, "value", false);

        // Resolve and journal reveals.
        let agg = store.model_aggregates().unwrap();
        let by_alias: HashMap<String, Option<String>> =
            agg.into_iter().map(|a| (a.alias, a.real_slug)).collect();
        for r in &mut rows {
            if let Some(Some(slug)) = by_alias.get(&r.alias) {
                r.display_name = slug.clone();
            }
            let alias = alias_from_display(&r.alias).unwrap();
            RevealGate.reveal(&alias, RevealReason::Stats, |_| Some(()));
            store.record_reveal(&r.alias, None, "stats").unwrap();
        }

        let reveals: i64 = store
            .conn
            .query_row("SELECT count(*) FROM reveals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(reveals, 2);

        // Re-opening the same file DB should see the data again.
        drop(store);
        let store2 = Store::open(&path).unwrap();
        let rows2 = gather_rows(&store2, &cfg).unwrap();
        assert_eq!(rows2.len(), 2);
    }
}
