//! Functionality for comparing
//! This is mainly used to build the triage report and the perf
//! comparison endpoints

use crate::api;
use crate::db::{ArtifactId, Benchmark, Profile, Scenario};
use crate::github;
use crate::load::SiteCtxt;
use crate::selector::{self, Tag};

use collector::Bound;
use log::debug;
use serde::Serialize;

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::hash::Hash;
use std::sync::Arc;

type BoxedError = Box<dyn Error + Send + Sync>;

pub async fn handle_triage(
    body: api::triage::Request,
    ctxt: &SiteCtxt,
) -> Result<api::triage::Response, BoxedError> {
    let start = body.start;
    let end = body.end;
    let master_commits = collector::master_commits().await?;

    let start_artifact = ctxt
        .artifact_id_for_bound(start.clone(), true)
        .ok_or(format!("could not find start commit for bound {:?}", start))?;
    let mut next = next_commit(&start_artifact, &master_commits)
        .map(|c| Bound::Commit(c.sha.clone()))
        .unwrap(); // TODO: handle no next commit

    let mut report = HashMap::new();
    let mut before = start.clone();

    loop {
        let comparison = match compare_given_commits(
            before,
            next.clone(),
            "instructions:u".to_owned(),
            ctxt,
            &master_commits,
        )
        .await?
        {
            Some(c) => c,
            None => {
                log::info!(
                    "No data found for end bound {:?}. Ending comparison...",
                    next
                );
                break;
            }
        };
        log::info!(
            "Comparing {} to {}",
            comparison.b.artifact,
            comparison.a.artifact
        );

        // handle results of comparison
        populate_report(&comparison, &mut report).await;

        // Check that there is a next commit and that the
        // after commit is not equal to `end`
        match comparison.next(&master_commits).map(Bound::Commit) {
            Some(n) if Some(&next) != end.as_ref() => {
                before = next;
                next = n;
            }
            _ => break,
        }
    }
    let end = end.unwrap_or(next);

    let report = generate_report(&start, &end, report).await;
    Ok(api::triage::Response(report))
}

pub async fn handle_compare(
    body: api::comparison::Request,
    ctxt: &SiteCtxt,
) -> Result<api::comparison::Response, BoxedError> {
    let master_commits = collector::master_commits().await?;
    let end = body.end;
    let comparison =
        compare_given_commits(body.start, end.clone(), body.stat, ctxt, &master_commits)
            .await?
            .ok_or_else(|| format!("could not find end commit for bound {:?}", end))?;

    let conn = ctxt.conn().await;
    let prev = comparison.prev(&master_commits);
    let next = comparison.next(&master_commits);
    let is_contiguous = comparison.is_contiguous(&*conn, &master_commits).await;
    let comparisons = comparison
        .statistics
        .into_iter()
        .map(|comparison| api::comparison::Comparison {
            benchmark: comparison.benchmark.to_string(),
            profile: comparison.profile.to_string(),
            scenario: comparison.scenario.to_string(),
            is_dodgy: comparison
                .variance
                .as_ref()
                .map(|v| v.is_dodgy())
                .unwrap_or(false),
            is_significant: comparison.is_significant(),
            historical_statistics: comparison.variance.map(|v| v.data),
            statistics: comparison.results,
        })
        .collect();

    Ok(api::comparison::Response {
        prev,
        a: comparison.a.into(),
        b: comparison.b.into(),
        comparisons,
        next,
        is_contiguous,
    })
}

async fn populate_report(comparison: &Comparison, report: &mut HashMap<Direction, Vec<String>>) {
    if let Some(summary) = ComparisonSummary::summarize_comparison(comparison) {
        if let Some(direction) = summary.direction() {
            let entry = report.entry(direction).or_default();

            entry.push(summary.write(comparison).await)
        }
    }
}

pub struct ComparisonSummary {
    hi: Option<BenchmarkComparison>,
    lo: Option<BenchmarkComparison>,
}

impl ComparisonSummary {
    pub fn summarize_comparison(comparison: &Comparison) -> Option<ComparisonSummary> {
        let mut benchmarks = comparison.get_benchmarks().collect::<Vec<_>>();
        // Skip empty commits, sometimes happens if there's a compiler bug or so.
        if benchmarks.len() == 0 {
            return None;
        }

        let cmp = |b1: &BenchmarkComparison, b2: &BenchmarkComparison| {
            b1.log_change()
                .partial_cmp(&b2.log_change())
                .unwrap_or(std::cmp::Ordering::Equal)
        };
        let lo = benchmarks
            .iter()
            .enumerate()
            .min_by(|&(_, b1), &(_, b2)| cmp(b1, b2))
            .filter(|(_, c)| c.is_significant() && !c.is_increase())
            .map(|(i, _)| i);
        let lo = lo.map(|lo| benchmarks.remove(lo)).cloned();
        let hi = benchmarks
            .iter()
            .enumerate()
            .max_by(|&(_, b1), &(_, b2)| cmp(b1, b2))
            .filter(|(_, c)| c.is_significant() && c.is_increase())
            .map(|(i, _)| i);
        let hi = hi.map(|hi| benchmarks.remove(hi)).cloned();

        Some(ComparisonSummary { hi, lo })
    }

    /// The direction of the changes
    pub fn direction(&self) -> Option<Direction> {
        let d = match (&self.hi, &self.lo) {
            (None, None) => return None,
            (Some(b), None) => b.direction(),
            (None, Some(b)) => b.direction(),
            (Some(a), Some(b)) if a.is_increase() == b.is_increase() => a.direction(),
            _ => Direction::Mixed,
        };
        Some(d)
    }

    /// The changes ordered by their signficance (most significant first)
    pub fn ordered_changes(&self) -> Vec<&BenchmarkComparison> {
        match (&self.hi, &self.lo) {
            (None, None) => Vec::new(),
            (Some(b), None) => vec![b],
            (None, Some(b)) => vec![b],
            (Some(a), Some(b))
                if b.log_change()
                    .abs()
                    .partial_cmp(&a.log_change().abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    == std::cmp::Ordering::Greater =>
            {
                vec![b, a]
            }
            (Some(a), Some(b)) => vec![a, b],
        }
    }

    async fn write(&self, comparison: &Comparison) -> String {
        use std::fmt::Write;

        let mut result = if let Some(pr) = comparison.b.pr {
            let title = github::pr_title(pr).await;
            format!(
                "{} [#{}](https://github.com/rust-lang/rust/issues/{})\n",
                title, pr, pr
            )
        } else {
            String::from("<Unknown Change>\n")
        };
        let start = &comparison.a.artifact;
        let end = &comparison.b.artifact;
        let link = &compare_link(start, end);

        for change in self.ordered_changes() {
            write!(result, "- ").unwrap();
            change.summary_line(&mut result, Some(link))
        }
        result
    }
}

/// Compare two bounds on a given stat
///
/// Returns Ok(None) when no data for the end bound is present
pub async fn compare(
    start: Bound,
    end: Bound,
    stat: String,
    ctxt: &SiteCtxt,
) -> Result<Option<Comparison>, BoxedError> {
    let master_commits = collector::master_commits().await?;
    compare_given_commits(start, end, stat, ctxt, &master_commits).await
}

/// Compare two bounds on a given stat
async fn compare_given_commits(
    start: Bound,
    end: Bound,
    stat: String,
    ctxt: &SiteCtxt,
    master_commits: &[collector::MasterCommit],
) -> Result<Option<Comparison>, BoxedError> {
    let a = ctxt
        .artifact_id_for_bound(start.clone(), true)
        .ok_or(format!("could not find start commit for bound {:?}", start))?;
    let b = match ctxt.artifact_id_for_bound(end.clone(), false) {
        Some(b) => b,
        None => return Ok(None),
    };
    let aids = Arc::new(vec![a.clone(), b.clone()]);

    // get all crates, cache, and profile combinations for the given stat
    let query = selector::Query::new()
        .set::<String>(Tag::Benchmark, selector::Selector::All)
        .set::<String>(Tag::Scenario, selector::Selector::All)
        .set::<String>(Tag::Profile, selector::Selector::All)
        .set(Tag::Metric, selector::Selector::One(stat.clone()));

    // `responses` contains series iterators. The first element in the iterator is the data
    // for `a` and the second is the data for `b`
    let mut responses = ctxt.statistic_series(query.clone(), aids).await?;

    let conn = ctxt.conn().await;
    let statistics_for_a = statistics_from_series(&mut responses);
    let statistics_for_b = statistics_from_series(&mut responses);

    let variances = BenchmarkVariances::calculate(ctxt, a.clone(), master_commits, stat).await?;
    let statistics = statistics_for_a
        .into_iter()
        .filter_map(|(test_case, a)| {
            statistics_for_b
                .get(&test_case)
                .map(|&b| BenchmarkComparison {
                    benchmark: test_case.0,
                    profile: test_case.1,
                    scenario: test_case.2,
                    variance: variances
                        .as_ref()
                        .and_then(|v| v.data.get(&test_case).cloned()),
                    results: (a, b),
                })
        })
        .collect();
    Ok(Some(Comparison {
        a: ArtifactDescription::for_artifact(&*conn, a.clone(), master_commits).await,
        b: ArtifactDescription::for_artifact(&*conn, b.clone(), master_commits).await,
        statistics,
    }))
}

fn previous_commits(
    mut from: ArtifactId,
    n: usize,
    master_commits: &[collector::MasterCommit],
) -> Vec<ArtifactId> {
    let mut prevs = Vec::with_capacity(n);
    while prevs.len() < n {
        match prev_commit(&from, master_commits) {
            Some(c) => {
                let new = ArtifactId::Commit(database::Commit {
                    sha: c.sha.clone(),
                    date: database::Date(c.time),
                });
                from = new.clone();
                prevs.push(new);
            }
            None => break,
        }
    }
    prevs
}

/// Detailed description of a specific artifact
#[derive(Debug, Clone)]
pub struct ArtifactDescription {
    /// The artifact in question
    pub artifact: ArtifactId,
    /// The pr of the artifact if known
    pub pr: Option<u32>,
    /// Bootstrap data in the form "$crate" -> nanoseconds
    pub bootstrap: HashMap<String, u64>,
}

type StatisticsMap = HashMap<TestCase, f64>;
type TestCase = (Benchmark, Profile, Scenario);

impl ArtifactDescription {
    /// For the given `ArtifactId`, consume the first datapoint in each of the given `SeriesResponse`
    ///
    /// It is assumed that the provided `ArtifactId` matches the artifact id of the next data
    /// point for all of `SeriesResponse<T>`. If this is not true, this function will panic.
    async fn for_artifact(
        conn: &dyn database::Connection,
        artifact: ArtifactId,
        master_commits: &[collector::MasterCommit],
    ) -> Self {
        let bootstrap = conn
            .get_bootstrap(&[conn.artifact_id(&artifact).await])
            .await;
        let bootstrap = bootstrap
            .into_iter()
            .filter_map(|(k, mut v)| {
                v.pop()
                    .unwrap_or_default()
                    // FIXME: if we're hovering right at the 1 second mark,
                    // this might mean we end up with a Some for one commit and
                    // a None for the other commit. Ultimately it doesn't matter
                    // that much -- we'll mostly just ignore such results.
                    // Anything less than a second in wall-time measurements is
                    // always going to be pretty high variance just from process
                    // startup overheads and such, though, so we definitely
                    // don't want to compare those values.
                    .filter(|v| v.as_secs() >= 1)
                    .map(|v| (k, v.as_nanos() as u64))
            })
            .collect::<HashMap<_, _>>();

        let pr = if let ArtifactId::Commit(c) = &artifact {
            if let Some(m) = master_commits.iter().find(|m| m.sha == c.sha) {
                m.pr
            } else {
                conn.pr_of(&c.sha).await
            }
        } else {
            None
        };

        Self {
            pr,
            artifact,
            bootstrap,
        }
    }
}

fn statistics_from_series<T>(series: &mut [selector::SeriesResponse<T>]) -> StatisticsMap
where
    T: Iterator<Item = (ArtifactId, Option<f64>)>,
{
    let mut stats: StatisticsMap = HashMap::new();
    for response in series {
        let (_, point) = response.series.next().expect("must have element");

        let value = if let Some(v) = point {
            v
        } else {
            continue;
        };
        let benchmark = *response.path.get::<Benchmark>().unwrap();
        let profile = *response.path.get::<Profile>().unwrap();
        let scenario = *response.path.get::<Scenario>().unwrap();
        stats.insert((benchmark, profile, scenario), value);
    }
    stats
}

impl From<ArtifactDescription> for api::comparison::ArtifactDescription {
    fn from(data: ArtifactDescription) -> Self {
        api::comparison::ArtifactDescription {
            commit: match data.artifact.clone() {
                ArtifactId::Commit(c) => c.sha,
                ArtifactId::Tag(t) => t,
            },
            date: if let ArtifactId::Commit(c) = &data.artifact {
                Some(c.date)
            } else {
                None
            },
            pr: data.pr,
            bootstrap: data.bootstrap,
        }
    }
}

// A comparison of two artifacts
pub struct Comparison {
    pub a: ArtifactDescription,
    pub b: ArtifactDescription,
    /// Statistics based on test case
    pub statistics: HashSet<BenchmarkComparison>,
}

impl Comparison {
    /// Gets the previous commit before `a`
    pub fn prev(&self, master_commits: &[collector::MasterCommit]) -> Option<String> {
        prev_commit(&self.a.artifact, master_commits).map(|c| c.sha.clone())
    }

    /// Determines if `a` and `b` are contiguous
    pub async fn is_contiguous(
        &self,
        conn: &dyn database::Connection,
        master_commits: &[collector::MasterCommit],
    ) -> bool {
        match (&self.a.artifact, &self.b.artifact) {
            (ArtifactId::Commit(a), ArtifactId::Commit(b)) => {
                if let Some(b) = master_commits.iter().find(|c| c.sha == b.sha) {
                    b.parent_sha == a.sha
                } else {
                    conn.parent_of(&b.sha).await.map_or(false, |p| p == a.sha)
                }
            }
            _ => false,
        }
    }

    /// Gets the sha of the next commit after `b`
    pub fn next(&self, master_commits: &[collector::MasterCommit]) -> Option<String> {
        next_commit(&self.b.artifact, master_commits).map(|c| c.sha.clone())
    }

    fn get_benchmarks(&self) -> impl Iterator<Item = &BenchmarkComparison> {
        self.statistics.iter().filter(|b| b.profile != Profile::Doc)
    }
}

/// A description of the amount of variance a certain benchmark is historically
/// experiencing at a given point in time.
pub struct BenchmarkVariances {
    /// Variance data on a per test case basis
    pub data: HashMap<(Benchmark, Profile, Scenario), BenchmarkVariance>,
}

impl BenchmarkVariances {
    const NUM_PREVIOUS_COMMITS: usize = 100;
    const MIN_PREVIOUS_COMMITS: usize = 50;

    async fn calculate(
        ctxt: &SiteCtxt,
        from: ArtifactId,
        master_commits: &[collector::MasterCommit],
        stat: String,
    ) -> Result<Option<Self>, BoxedError> {
        // get all crates, cache, and profile combinations for the given stat
        let query = selector::Query::new()
            .set::<String>(Tag::Benchmark, selector::Selector::All)
            .set::<String>(Tag::Scenario, selector::Selector::All)
            .set::<String>(Tag::Profile, selector::Selector::All)
            .set(Tag::Metric, selector::Selector::One(stat));

        let previous_commits = Arc::new(previous_commits(
            from,
            Self::NUM_PREVIOUS_COMMITS,
            master_commits,
        ));
        let mut previous_commit_series = ctxt
            .statistic_series(query, previous_commits.clone())
            .await?;

        let mut variance_data: HashMap<(Benchmark, Profile, Scenario), BenchmarkVariance> =
            HashMap::new();
        for _ in previous_commits.iter() {
            for (test_case, stat) in statistics_from_series(&mut previous_commit_series) {
                variance_data.entry(test_case).or_default().push(stat);
            }
        }
        if variance_data.len() < Self::MIN_PREVIOUS_COMMITS {
            return Ok(None);
        }

        for ((bench, _, _), results) in variance_data.iter_mut() {
            debug!("Calculating variance for: {}", bench);
            results.calculate_description();
        }
        Ok(Some(Self {
            data: variance_data,
        }))
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct BenchmarkVariance {
    data: Vec<f64>,
    description: BenchmarkVarianceDescription,
}

impl BenchmarkVariance {
    /// The ratio of change that we consider significant.
    const SIGNFICANT_DELTA_THRESHOLD: f64 = 0.01;
    /// The percentage of significant changes that we consider too high
    const SIGNFICANT_CHANGE_THRESHOLD: f64 = 5.0;
    /// The percentage of change that constitutes noisy data
    const NOISE_THRESHOLD: f64 = 0.1;

    fn push(&mut self, value: f64) {
        self.data.push(value);
    }

    fn mean(&self) -> f64 {
        self.data.iter().sum::<f64>() / self.data.len() as f64
    }

    fn calculate_description(&mut self) {
        self.description = BenchmarkVarianceDescription::Normal;

        let results_mean = self.mean();
        let mut deltas = self
            .data
            .windows(2)
            .map(|window| (window[0] - window[1]).abs())
            .collect::<Vec<_>>();
        deltas.sort_by(|d1, d2| d1.partial_cmp(d2).unwrap_or(std::cmp::Ordering::Equal));
        let non_significant = deltas
            .iter()
            .zip(self.data.iter())
            .take_while(|(&d, &r)| d / r < Self::SIGNFICANT_DELTA_THRESHOLD)
            .collect::<Vec<_>>();

        let percent_significant_changes =
            ((deltas.len() - non_significant.len()) as f64 / deltas.len() as f64) * 100.0;
        debug!(
            "Percent significant changes: {:.1}%",
            percent_significant_changes
        );

        if percent_significant_changes > Self::SIGNFICANT_CHANGE_THRESHOLD {
            self.description =
                BenchmarkVarianceDescription::HighlyVariable(percent_significant_changes);
            return;
        }

        let delta_mean =
            non_significant.iter().map(|(&d, _)| d).sum::<f64>() / (non_significant.len() as f64);
        let percent_change = (delta_mean / results_mean) * 100.0;
        debug!("Percent change: {:.3}%", percent_change);
        if percent_change > Self::NOISE_THRESHOLD {
            self.description = BenchmarkVarianceDescription::Noisy(percent_change);
        }
    }

    /// Whether we can trust this benchmark or not
    fn is_dodgy(&self) -> bool {
        matches!(
            self.description,
            BenchmarkVarianceDescription::Noisy(_)
                | BenchmarkVarianceDescription::HighlyVariable(_)
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", content = "percent")]
pub enum BenchmarkVarianceDescription {
    Normal,
    /// A highly variable benchmark that produces many significant changes.
    /// This might indicate a benchmark which is very sensitive to compiler changes.
    ///
    /// Cotains the percentage of significant changes.
    HighlyVariable(f64),
    /// A noisy benchmark which is likely to see changes in performance simply between
    /// compiler runs.
    ///
    /// Contains the percent change that happens on average
    Noisy(f64),
}

impl Default for BenchmarkVarianceDescription {
    fn default() -> Self {
        Self::Normal
    }
}

/// Gets the previous commit
pub fn prev_commit<'a>(
    artifact: &ArtifactId,
    master_commits: &'a [collector::MasterCommit],
) -> Option<&'a collector::MasterCommit> {
    match &artifact {
        ArtifactId::Commit(a) => {
            let current = master_commits.iter().find(|c| c.sha == a.sha)?;
            master_commits.iter().find(|c| c.sha == current.parent_sha)
        }
        ArtifactId::Tag(_) => None,
    }
}

/// Gets the next commit
pub fn next_commit<'a>(
    artifact: &ArtifactId,
    master_commits: &'a [collector::MasterCommit],
) -> Option<&'a collector::MasterCommit> {
    match artifact {
        ArtifactId::Commit(b) => master_commits.iter().find(|c| c.parent_sha == b.sha),
        ArtifactId::Tag(_) => None,
    }
}

// A single comparison based on benchmark and cache state
#[derive(Debug, Clone)]
pub struct BenchmarkComparison {
    benchmark: Benchmark,
    profile: Profile,
    scenario: Scenario,
    variance: Option<BenchmarkVariance>,
    results: (f64, f64),
}

const SIGNIFICANCE_THRESHOLD: f64 = 0.01;
impl BenchmarkComparison {
    fn log_change(&self) -> f64 {
        let (a, b) = self.results;
        (b / a).ln()
    }

    fn is_increase(&self) -> bool {
        let (a, b) = self.results;
        b > a
    }

    fn is_significant(&self) -> bool {
        // This particular test case frequently varies
        if &self.benchmark == "coercions"
            && self.profile == Profile::Debug
            && matches!(self.scenario, Scenario::IncrementalPatch(p) if &p == "println")
        {
            self.relative_change().abs() > 2.0
        } else {
            self.log_change().abs() > SIGNIFICANCE_THRESHOLD
        }
    }

    fn relative_change(&self) -> f64 {
        let (a, b) = self.results;
        (b - a) / a
    }

    fn direction(&self) -> Direction {
        if self.log_change() > 0.0 {
            Direction::Regression
        } else {
            Direction::Improvement
        }
    }

    pub fn summary_line(&self, summary: &mut String, link: Option<&str>) {
        use std::fmt::Write;
        let magnitude = self.log_change().abs();
        let size = if magnitude > 0.10 {
            "Very large"
        } else if magnitude > 0.05 {
            "Large"
        } else if magnitude > 0.01 {
            "Moderate"
        } else if magnitude > 0.005 {
            "Small"
        } else {
            "Very small"
        };

        let percent = self.relative_change() * 100.0;
        write!(
            summary,
            "{} {} in {}",
            size,
            self.direction(),
            match link {
                Some(l) => format!("[instruction counts]({})", l),
                None => "instruction counts".into(),
            }
        )
        .unwrap();
        writeln!(
            summary,
            " (up to {:.1}% on `{}` builds of `{}`)",
            percent, self.scenario, self.benchmark
        )
        .unwrap();
    }
}
impl std::cmp::PartialEq for BenchmarkComparison {
    fn eq(&self, other: &Self) -> bool {
        self.benchmark == other.benchmark
            && self.profile == other.profile
            && self.scenario == other.scenario
    }
}
impl std::cmp::Eq for BenchmarkComparison {}

impl std::hash::Hash for BenchmarkComparison {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.benchmark.hash(state);
        self.profile.hash(state);
        self.scenario.hash(state);
    }
}

// The direction of a performance change
#[derive(PartialEq, Eq, Hash)]
pub enum Direction {
    Improvement,
    Regression,
    Mixed,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let description = match self {
            Direction::Improvement => "improvement",
            Direction::Regression => "regression",
            Direction::Mixed => "mixed",
        };
        write!(f, "{}", description)
    }
}

async fn generate_report(
    start: &Bound,
    end: &Bound,
    mut report: HashMap<Direction, Vec<String>>,
) -> String {
    fn fmt_bound(bound: &Bound) -> String {
        match bound {
            Bound::Commit(s) => s.to_owned(),
            Bound::Date(s) => s.format("%Y-%m-%d").to_string(),
            _ => "???".to_owned(),
        }
    }
    let start = fmt_bound(start);
    let end = fmt_bound(end);
    let regressions = report.remove(&Direction::Regression).unwrap_or_default();
    let improvements = report.remove(&Direction::Improvement).unwrap_or_default();
    let mixed = report.remove(&Direction::Mixed).unwrap_or_default();
    let untriaged = match github::untriaged_perf_regressions().await {
        Ok(u) => u
            .iter()
            .map(|github::PullRequest { title, number }| {
                format!(
                    "- [#{} {}](https://github.com/rust-lang/rust/pull/{})",
                    number, title, number
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => format!(
            "An **error** occurred when finding the untriaged PRs: {}",
            e
        ),
    };
    format!(
        r#####"# {date} Triage Log

TODO: Summary

Triage done by **@???**.
Revision range: [{first_commit}..{last_commit}](https://perf.rust-lang.org/?start={first_commit}&end={last_commit}&absolute=false&stat=instructions%3Au)

{num_regressions} Regressions, {num_improvements} Improvements, {num_mixed} Mixed; ??? of them in rollups

#### Regressions

{regressions}

#### Improvements

{improvements}

#### Mixed

{mixed}

#### Untriaged Pull Requests

{untriaged}

#### Nags requiring follow up

TODO: Nags

"#####,
        date = chrono::Utc::today().format("%Y-%m-%d"),
        first_commit = start,
        last_commit = end,
        num_regressions = regressions.len(),
        num_improvements = improvements.len(),
        num_mixed = mixed.len(),
        regressions = regressions.join("\n\n"),
        improvements = improvements.join("\n\n"),
        mixed = mixed.join("\n\n"),
        untriaged = untriaged
    )
}

fn compare_link(start: &ArtifactId, end: &ArtifactId) -> String {
    let start = match &start {
        ArtifactId::Tag(a) => a,
        ArtifactId::Commit(c) => &c.sha,
    };
    let end = match &end {
        ArtifactId::Tag(a) => a,
        ArtifactId::Commit(c) => &c.sha,
    };
    format!(
        "https://perf.rust-lang.org/compare.html?start={}&end={}&stat=instructions:u",
        start, end
    )
}
