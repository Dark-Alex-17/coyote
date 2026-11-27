//! Charts drawn from a small YAML or JSON spec with ratatui's own chart
//! widgets. [`render_chart`] reads and checks the spec before any widget
//! sees it, draws into a buffer of the block's size and survives a panic
//! in the widget.

use super::{RichStyle, buffer_lines};
use crate::tui::render::plain_text;
use crate::tui::terminal::catch_quietly;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::{
    Axis, Bar, BarChart, BarGroup, Block, Chart, Dataset, GraphType, LegendPosition, Sparkline,
    Widget,
};
use serde::Deserialize;

const MAX_SPEC_BYTES: usize = 64 * 1024;
const MAX_POINTS: usize = 2000;
const MAX_SERIES: usize = 8;
const MAX_BARS: usize = 64;
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 6;
const MAX_HEIGHT: u16 = 30;
pub(crate) const DEFAULT_HEIGHT: u16 = 12;
const BAR_GAP: u16 = 1;

/// The markers series take in turn under `NO_COLOR`, where they cannot
/// differ by colour. There are fewer of them than `MAX_SERIES`, so a
/// fifth series takes the first one's marker again.
const PLAIN_MARKERS: [Marker; 4] = [Marker::Braille, Marker::Dot, Marker::Block, Marker::Bar];

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum ChartSpec {
    Line(XyChart),
    /// The fields of a line chart, its points drawn unjoined.
    Scatter(XyChart),
    Bar(BarSpec),
    Sparkline(SparklineSpec),
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct XyChart {
    title: Option<String>,
    height: Option<u16>,
    x_label: Option<String>,
    y_label: Option<String>,
    x_bounds: Option<[f64; 2]>,
    y_bounds: Option<[f64; 2]>,
    marker: Option<MarkerSpec>,
    series: Vec<Series>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MarkerSpec {
    Braille,
    Dot,
    Block,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Series {
    name: Option<String>,
    data: SeriesData,
}

/// A series as `[x, y]` pairs, or as bare values whose index is the x.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
enum SeriesData {
    Points(Vec<[f64; 2]>),
    Values(Vec<f64>),
}

impl SeriesData {
    fn points(&self) -> Vec<(f64, f64)> {
        match self {
            SeriesData::Points(points) => points.iter().map(|[x, y]| (*x, *y)).collect(),
            SeriesData::Values(values) => (0_u32..)
                .zip(values)
                .map(|(index, y)| (f64::from(index), *y))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BarSpec {
    title: Option<String>,
    height: Option<u16>,
    bars: Vec<BarEntry>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct BarEntry {
    label: String,
    value: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SparklineSpec {
    title: Option<String>,
    height: Option<u16>,
    data: Vec<u64>,
}

/// Why a chart is shown as source instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChartError {
    TooLarge,
    Invalid(String),
    TooNarrow { needed: u16, available: u16 },
    Panicked(String),
}

impl ChartError {
    /// The one-line note shown under the source.
    pub(crate) fn note(&self) -> String {
        let why = match self {
            ChartError::TooLarge => format!("spec larger than {} KiB", MAX_SPEC_BYTES / 1024),
            ChartError::Invalid(message) => format!("invalid spec: {message}"),
            ChartError::TooNarrow { needed, available } => {
                format!("too narrow: needs {needed} columns, {available} available")
            }
            ChartError::Panicked(message) => format!("renderer failed: {message}"),
        };
        format!("chart: {why} (showing source)")
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// The spec in `body`, read as YAML (so JSON too) and checked: every
/// number finite, no series or bar list empty or over its cap, and any
/// bounds given low end first. Its text is scrubbed of escapes and format
/// characters, so a title cannot reorder or hide what is drawn.
pub(crate) fn parse(body: &str) -> Result<ChartSpec, ChartError> {
    if body.len() > MAX_SPEC_BYTES {
        return Err(ChartError::TooLarge);
    }
    if has_alias(body) {
        return Err(ChartError::Invalid(
            "YAML aliases (*name) are not supported".to_string(),
        ));
    }
    let mut spec: ChartSpec = serde_yaml::from_str(body)
        .map_err(|err| ChartError::Invalid(first_line(&err.to_string())))?;
    spec.check().map_err(ChartError::Invalid)?;
    spec.scrub();
    Ok(spec)
}

/// Whether `body` may hold a YAML alias: a `*` where a token can start,
/// followed by a name. serde_yaml replays an alias's whole anchored node
/// at each use and caps only how many uses there are, so a 10 KiB spec of
/// nested aliases takes seconds and gigabytes to read. The scan does not
/// follow quoting or comments, so a quoted ` *word` is refused as well;
/// it never misses an alias.
fn has_alias(body: &str) -> bool {
    let chars: Vec<char> = body.chars().collect();
    (0..chars.len()).any(|i| {
        let starts_token = i == 0 || chars[i - 1].is_whitespace() || "[{,:".contains(chars[i - 1]);
        let named = chars
            .get(i + 1)
            .is_some_and(|next| !next.is_whitespace() && !",[]{}".contains(*next));
        chars[i] == '*' && starts_token && named
    })
}

impl ChartSpec {
    fn scrub(&mut self) {
        let flat = |text: &str| plain_text(text).replace(['\n', '\t'], " ");
        let scrub = |text: &mut Option<String>| {
            if let Some(text) = text {
                *text = flat(text);
            }
        };
        match self {
            ChartSpec::Line(chart) | ChartSpec::Scatter(chart) => {
                for text in [&mut chart.title, &mut chart.x_label, &mut chart.y_label] {
                    scrub(text);
                }
                for series in &mut chart.series {
                    scrub(&mut series.name);
                }
            }
            ChartSpec::Bar(spec) => {
                scrub(&mut spec.title);
                for bar in &mut spec.bars {
                    bar.label = flat(&bar.label);
                }
            }
            ChartSpec::Sparkline(spec) => scrub(&mut spec.title),
        }
    }

    fn check(&self) -> Result<(), String> {
        match self {
            ChartSpec::Line(chart) | ChartSpec::Scatter(chart) => chart.check(),
            ChartSpec::Bar(spec) => match spec.bars.len() {
                0 => Err("no bars".to_string()),
                count if count > MAX_BARS => Err(format!("more than {MAX_BARS} bars")),
                _ => Ok(()),
            },
            ChartSpec::Sparkline(spec) => match spec.data.len() {
                0 => Err("no data".to_string()),
                count if count > MAX_POINTS => Err(format!("more than {MAX_POINTS} values")),
                _ => Ok(()),
            },
        }
    }

    fn height(&self) -> Option<u16> {
        match self {
            ChartSpec::Line(chart) | ChartSpec::Scatter(chart) => chart.height,
            ChartSpec::Bar(spec) => spec.height,
            ChartSpec::Sparkline(spec) => spec.height,
        }
    }

    /// The fewest columns the chart can be drawn in: a bar chart needs a
    /// column for each bar and a gap between each two, and a sparkline a
    /// column for each value, as ratatui draws only the values that fit
    /// yet scales them against all of them.
    fn min_width(&self) -> u16 {
        match self {
            ChartSpec::Bar(spec) => {
                let bars = u16::try_from(spec.bars.len()).unwrap_or(u16::MAX);
                MIN_WIDTH.max(bars.saturating_mul(1 + BAR_GAP).saturating_sub(BAR_GAP))
            }
            ChartSpec::Sparkline(spec) => {
                MIN_WIDTH.max(u16::try_from(spec.data.len()).unwrap_or(u16::MAX))
            }
            ChartSpec::Line(_) | ChartSpec::Scatter(_) => MIN_WIDTH,
        }
    }

    fn draw(&self, area: Rect, buffer: &mut Buffer, style: &RichStyle) {
        match self {
            ChartSpec::Line(chart) => chart.draw(GraphType::Line, area, buffer, style),
            ChartSpec::Scatter(chart) => chart.draw(GraphType::Scatter, area, buffer, style),
            ChartSpec::Bar(spec) => spec.draw(area, buffer, style),
            ChartSpec::Sparkline(spec) => spec.draw(area, buffer, style),
        }
    }
}

impl XyChart {
    fn check(&self) -> Result<(), String> {
        if self.series.is_empty() {
            return Err("no series".to_string());
        }
        if self.series.len() > MAX_SERIES {
            return Err(format!("more than {MAX_SERIES} series"));
        }
        for (label, bounds) in [("x_bounds", self.x_bounds), ("y_bounds", self.y_bounds)] {
            if let Some([low, high]) = bounds
                && !(low.is_finite() && high.is_finite() && low <= high)
            {
                return Err(format!("{label} must be two finite numbers, low first"));
            }
        }
        for (index, series) in self.series.iter().enumerate() {
            let name = series_name(series, index);
            let points = series.data.points();
            if points.is_empty() {
                return Err(format!("{name} has no data"));
            }
            if points.len() > MAX_POINTS {
                return Err(format!("{name} has more than {MAX_POINTS} points"));
            }
            if points
                .iter()
                .any(|(x, y)| !(x.is_finite() && y.is_finite()))
            {
                return Err(format!("{name} has a value that is not a finite number"));
            }
        }
        Ok(())
    }

    fn draw(&self, graph: GraphType, area: Rect, buffer: &mut Buffer, style: &RichStyle) {
        let points: Vec<Vec<(f64, f64)>> = self.series.iter().map(|s| s.data.points()).collect();
        let x_bounds = bounds(self.x_bounds, points.iter().flatten().map(|(x, _)| *x));
        let y_bounds = bounds(self.y_bounds, points.iter().flatten().map(|(_, y)| *y));
        let chosen = match self.marker {
            None | Some(MarkerSpec::Braille) => Marker::Braille,
            Some(MarkerSpec::Dot) => Marker::Dot,
            Some(MarkerSpec::Block) => Marker::Block,
        };
        let markers = markers(chosen, self.series.len(), style.no_color);
        let datasets = self
            .series
            .iter()
            .zip(&points)
            .zip(markers)
            .enumerate()
            .map(|(index, ((series, points), marker))| {
                Dataset::default()
                    .name(series_name(series, index))
                    .data(points)
                    .marker(marker)
                    .graph_type(graph)
                    .style(style.series(index))
            })
            .collect();
        let axis = |title: &Option<String>, [low, high]: [f64; 2]| {
            Axis::default()
                .title(title.clone().unwrap_or_default())
                .bounds([low, high])
                .labels([
                    axis_label(low),
                    axis_label((low + high) / 2.0),
                    axis_label(high),
                ])
                .style(style.axis)
        };
        let legend = (self.series.len() > 1 || style.no_color).then_some(LegendPosition::TopRight);
        // ratatui hides a legend wider or taller than a quarter of the
        // chart by default; here it shows whenever it fits.
        Chart::new(datasets)
            .block(titled(&self.title, style))
            .x_axis(axis(&self.x_label, x_bounds))
            .y_axis(axis(&self.y_label, y_bounds))
            .legend_position(legend)
            .hidden_legend_constraints((Constraint::Percentage(100), Constraint::Percentage(100)))
            .render(area, buffer);
    }
}

impl BarSpec {
    fn draw(&self, area: Rect, buffer: &mut Buffer, style: &RichStyle) {
        let block = titled(&self.title, style);
        let inner = block.inner(area).width;
        let count = u16::try_from(self.bars.len()).unwrap_or(u16::MAX);
        let bar_width = ((inner + BAR_GAP) / count).saturating_sub(BAR_GAP).max(1);
        let bars: Vec<Bar> = self
            .bars
            .iter()
            .map(|bar| Bar::default().label(bar.label.clone()).value(bar.value))
            .collect();
        BarChart::default()
            .block(block)
            .data(BarGroup::default().bars(&bars))
            .bar_width(bar_width)
            .bar_gap(BAR_GAP)
            .bar_style(style.series(0))
            .label_style(style.axis)
            .render(area, buffer);
    }
}

impl SparklineSpec {
    fn draw(&self, area: Rect, buffer: &mut Buffer, style: &RichStyle) {
        Sparkline::default()
            .block(titled(&self.title, style))
            .data(&self.data)
            .style(style.series(0))
            .render(area, buffer);
    }
}

fn series_name(series: &Series, index: usize) -> String {
    series
        .name
        .clone()
        .unwrap_or_else(|| format!("series {}", index + 1))
}

/// A borderless block holding the chart, with its title on the top row.
fn titled<'a>(title: &Option<String>, style: &RichStyle) -> Block<'a> {
    match title {
        Some(title) => Block::new().title(Line::styled(title.clone(), style.title)),
        None => Block::new(),
    }
}

/// One marker per series: the one chosen for all of them, or under
/// `no_color` the plain markers in turn, starting with the one chosen and
/// starting over after the fourth series.
fn markers(chosen: Marker, count: usize, no_color: bool) -> Vec<Marker> {
    if !no_color {
        return vec![chosen; count];
    }
    let others = PLAIN_MARKERS.into_iter().filter(|marker| *marker != chosen);
    std::iter::once(chosen)
        .chain(others)
        .cycle()
        .take(count)
        .collect()
}

/// The bounds given, or the span of `values`; a span of one value is
/// widened by one each way so the axis has length.
fn bounds(given: Option<[f64; 2]>, values: impl Iterator<Item = f64>) -> [f64; 2] {
    let [low, high] = given.unwrap_or_else(|| {
        values.fold([f64::INFINITY, f64::NEG_INFINITY], |[low, high], value| {
            [low.min(value), high.max(value)]
        })
    });
    if low == high {
        [low - 1.0, high + 1.0]
    } else {
        [low, high]
    }
}

/// `value` rounded to two places, without trailing zeros.
fn axis_label(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    format!("{}", rounded + 0.0)
}

/// The chart in `body` drawn `width` columns wide and `height` rows high,
/// or as high as its own `height`; either is held to 6 to 30 rows.
pub(crate) fn render_chart(
    body: &str,
    width: u16,
    height: u16,
    style: &RichStyle,
) -> Result<Vec<Line<'static>>, ChartError> {
    let spec = guarded(|| parse(body))?;
    let needed = spec.min_width();
    if width < needed {
        return Err(ChartError::TooNarrow {
            needed,
            available: width,
        });
    }
    let height = spec
        .height()
        .unwrap_or(height)
        .clamp(MIN_HEIGHT, MAX_HEIGHT);
    let area = Rect::new(0, 0, width, height);
    let buffer = guarded(|| {
        let mut buffer = Buffer::empty(area);
        spec.draw(area, &mut buffer, style);
        Ok(buffer)
    })?;
    Ok(buffer_lines(&buffer, style.no_color))
}

/// `step` run so that a panic in it is an error: serde_yaml panics on an
/// alias it cannot resolve, and a widget may panic on a size it did not
/// expect.
fn guarded<T>(step: impl FnOnce() -> Result<T, ChartError>) -> Result<T, ChartError> {
    catch_quietly(step).map_err(|message| ChartError::Panicked(first_line(&message)))?
}

/// Every chart `type` and every field name a spec can hold. A unit test
/// holds it to what the parser reads, and the wiki's chart section is
/// checked against it.
#[cfg(test)]
const CHART_SPEC_NAMES: &[&str] = &[
    "line",
    "scatter",
    "bar",
    "sparkline",
    "type",
    "title",
    "height",
    "x_label",
    "y_label",
    "x_bounds",
    "y_bounds",
    "marker",
    "series",
    "name",
    "data",
    "bars",
    "label",
    "value",
];

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use std::collections::BTreeSet;

    fn style(no_color: bool) -> RichStyle {
        RichStyle {
            palette: vec![Color::Cyan, Color::Yellow],
            title: Style::new(),
            axis: Style::new(),
            no_color,
        }
    }

    fn invalid(body: &str) -> String {
        match parse(body) {
            Err(ChartError::Invalid(message)) => message,
            other => panic!("{body:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn a_line_spec_takes_pairs_or_bare_values() {
        let yaml = "type: line\ntitle: Load\nx_label: t\ny_label: req/s\nx_bounds: [0, 4]\n\
                    marker: dot\nheight: 8\nseries:\n  - name: a\n    data: [[0, 1], [2, 3.5]]\n  \
                    - data: [5, 6, 7]\n";
        let ChartSpec::Line(chart) = parse(yaml).unwrap() else {
            panic!("a line chart");
        };
        assert_eq!(chart.title.as_deref(), Some("Load"));
        assert_eq!(chart.x_bounds, Some([0.0, 4.0]));
        assert_eq!(chart.marker, Some(MarkerSpec::Dot));
        assert_eq!(chart.height, Some(8));
        assert_eq!(chart.series[0].data.points(), [(0.0, 1.0), (2.0, 3.5)]);
        assert_eq!(
            chart.series[1].data.points(),
            [(0.0, 5.0), (1.0, 6.0), (2.0, 7.0)]
        );
        assert_eq!(series_name(&chart.series[1], 1), "series 2");
    }

    #[test]
    fn json_is_read_as_well_as_yaml() {
        let json = r#"{"type": "scatter", "series": [{"name": "s", "data": [[1, 2]]}]}"#;
        assert!(matches!(parse(json), Ok(ChartSpec::Scatter(_))));
    }

    /// The names the parser lists when it meets one it does not know are
    /// the documented ones.
    #[test]
    fn the_documented_names_are_the_ones_the_parser_reads() {
        let mut read = BTreeSet::from(["type".to_string()]);
        for body in [
            "type: unknown\n",
            "type: line\nunknown: 1\n",
            "type: bar\nunknown: 1\n",
            "type: sparkline\nunknown: 1\n",
            "type: line\nseries:\n  - unknown: 1\n",
            "type: bar\nbars:\n  - unknown: 1\n",
        ] {
            let message = invalid(body);
            let expected = message
                .split_once(", expected ")
                .unwrap_or_else(|| panic!("{body:?}: {message}"))
                .1;
            read.extend(
                expected
                    .split('`')
                    .skip(1)
                    .step_by(2)
                    .map(ToString::to_string),
            );
        }
        let documented: BTreeSet<String> =
            CHART_SPEC_NAMES.iter().map(ToString::to_string).collect();
        assert_eq!(read, documented);
    }

    #[test]
    fn bar_and_sparkline_specs_parse() {
        let bar = parse("type: bar\ntitle: Langs\nbars:\n  - {label: rust, value: 7}\n").unwrap();
        let ChartSpec::Bar(bar) = bar else {
            panic!("a bar chart");
        };
        assert_eq!(bar.bars[0].label, "rust");
        assert_eq!(bar.bars[0].value, 7);
        let spark = parse(r#"{"type": "sparkline", "data": [1, 5, 2]}"#).unwrap();
        let ChartSpec::Sparkline(spark) = spark else {
            panic!("a sparkline");
        };
        assert_eq!(spark.data, [1, 5, 2]);
    }

    #[test]
    fn a_spec_serde_rejects_names_its_problem() {
        for (body, needle) in [
            ("type: pie\ndata: [1]\n", "unknown variant `pie`"),
            ("data: [1]\n", "missing field `type`"),
            (
                "type: sparkline\ndata: [1]\ncolour: red\n",
                "unknown field `colour`",
            ),
            ("type: line\ntitle: x\n", "missing field `series`"),
            (
                "type: bar\nbars:\n  - {label: a, value: -1}\n",
                "invalid value",
            ),
            ("type: line\nseries: [\n", "at line 3 column 1"),
        ] {
            let message = invalid(body);
            assert!(message.contains(needle), "{body:?}: {message}");
        }
    }

    #[test]
    fn a_spec_the_checks_reject_names_its_problem() {
        let too_many = format!("type: sparkline\ndata: [{}]\n", vec!["1"; 2001].join(", "));
        let too_many_bars = format!(
            "type: bar\nbars: [{}]\n",
            vec!["{label: a, value: 1}"; 65].join(", ")
        );
        for (body, message) in [
            ("type: line\nseries: []\n", "no series"),
            (
                "type: line\nseries:\n  - {name: a, data: []}\n",
                "a has no data",
            ),
            (
                "type: line\nseries:\n  - data: [1, .nan]\n",
                "series 1 has a value that is not a finite number",
            ),
            (
                "type: line\nseries:\n  - data: [[0, .inf]]\n",
                "series 1 has a value that is not a finite number",
            ),
            (
                "type: line\ny_bounds: [3, 1]\nseries:\n  - data: [1]\n",
                "y_bounds must be two finite numbers, low first",
            ),
            ("type: bar\nbars: []\n", "no bars"),
            (too_many_bars.as_str(), "more than 64 bars"),
            ("type: sparkline\ndata: []\n", "no data"),
            (too_many.as_str(), "more than 2000 values"),
        ] {
            assert_eq!(invalid(body), message, "{body:?}");
        }
    }

    #[test]
    fn a_spec_over_the_size_cap_is_not_read() {
        let body = format!(
            "type: sparkline\ndata: [1]\n#{}\n",
            "x".repeat(MAX_SPEC_BYTES)
        );
        assert_eq!(parse(&body), Err(ChartError::TooLarge));
    }

    /// Nested aliases over a large anchored list: serde_yaml would replay
    /// the list at every use (each level here takes about nine times the
    /// last, seconds by the third), so any alias is refused before it is
    /// read.
    #[test]
    fn an_alias_bomb_is_refused_at_once() {
        let mut body = String::from("type: line\nseries:\n  - data: [&a0 [");
        body.push_str(&vec!["1"; 5000].join(","));
        body.push(']');
        for level in 1..=9 {
            let uses = vec![format!("*a{}", level - 1); 9].join(",");
            body.push_str(&format!(", &a{level} [{uses}]"));
        }
        body.push_str("]\n");
        assert!(body.len() < MAX_SPEC_BYTES);
        let start = std::time::Instant::now();
        let refused = parse(&body);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(
            refused,
            Err(ChartError::Invalid(
                "YAML aliases (*name) are not supported".to_string()
            ))
        );
    }

    /// Every place an alias can start is caught; a `*` inside a word or
    /// before a space, and an anchor with no alias, are not aliases.
    #[test]
    fn only_an_alias_is_refused() {
        for alias in [
            "*a",
            "title: *a",
            "data: [*a]",
            "data: [1,*a]",
            "{\"title\":*a}",
            "series:\n\t- *a",
        ] {
            assert!(has_alias(alias), "{alias:?}");
        }
        for plain in ["title: a * b", "title: 2*3", "title: x*", "title: *"] {
            assert!(!has_alias(plain), "{plain:?}");
        }
        assert!(parse("type: sparkline\ntitle: &t Load\ndata: [1]\n").is_ok());
    }

    #[test]
    fn a_panic_while_reading_is_an_error() {
        let read = guarded(|| -> Result<ChartSpec, ChartError> { panic!("unresolved alias: 3") });
        assert_eq!(
            read,
            Err(ChartError::Panicked("unresolved alias: 3".to_string()))
        );
    }

    /// A title, a bar label and a series name lose the format characters
    /// that could reorder or hide what the chart shows.
    #[test]
    fn the_specs_text_is_scrubbed() {
        let ChartSpec::Line(chart) = parse(
            "type: line\ntitle: \"rm \\u202Etxt\"\nx_label: \"t\\u200B\"\n\
             series:\n  - {name: \"up\\u2066x\", data: [1]}\n",
        )
        .unwrap() else {
            panic!("a line chart");
        };
        assert_eq!(chart.title.as_deref(), Some("rm txt"));
        assert_eq!(chart.x_label.as_deref(), Some("t"));
        assert_eq!(chart.series[0].name.as_deref(), Some("upx"));
        let ChartSpec::Bar(bar) =
            parse("type: bar\ntitle: \"one\\ntwo\\tthree\"\nbars:\n  - {label: \"a\\u202Eb\", value: 1}\n")
                .unwrap()
        else {
            panic!("a bar chart");
        };
        assert_eq!(bar.bars[0].label, "ab");
        assert_eq!(bar.title.as_deref(), Some("one two three"));
    }

    #[test]
    fn a_chart_too_narrow_for_the_room_is_not_drawn() {
        let body = "type: sparkline\ndata: [1, 2]\n";
        assert_eq!(
            render_chart(body, 19, 12, &style(false)),
            Err(ChartError::TooNarrow {
                needed: 20,
                available: 19,
            })
        );
        let bars = format!(
            "type: bar\nbars: [{}]\n",
            vec!["{label: a, value: 1}"; 30].join(", ")
        );
        assert_eq!(
            render_chart(&bars, 40, 12, &style(false)),
            Err(ChartError::TooNarrow {
                needed: 59,
                available: 40,
            })
        );
        let values = format!("type: sparkline\ndata: [{}]\n", vec!["1"; 50].join(", "));
        assert_eq!(
            render_chart(&values, 40, 12, &style(false)),
            Err(ChartError::TooNarrow {
                needed: 50,
                available: 40,
            })
        );
        assert!(render_chart(&values, 50, 12, &style(false)).is_ok());
    }

    #[test]
    fn a_span_of_one_value_is_widened_both_ways() {
        assert_eq!(bounds(None, [3.0, 3.0].into_iter()), [2.0, 4.0]);
        assert_eq!(bounds(Some([5.0, 5.0]), std::iter::empty()), [4.0, 6.0]);
        assert_eq!(bounds(None, [4.0, -1.0, 2.0].into_iter()), [-1.0, 4.0]);
        assert_eq!(bounds(Some([0.0, 10.0]), [50.0].into_iter()), [0.0, 10.0]);
    }

    #[test]
    fn the_height_is_the_specs_or_the_default_held_to_its_range() {
        let rows =
            |body: &str, height: u16| render_chart(body, 40, height, &style(false)).unwrap().len();
        let spark = "type: sparkline\ndata: [1, 2]\n";
        assert_eq!(rows(spark, DEFAULT_HEIGHT), 12);
        assert_eq!(rows(spark, 2), usize::from(MIN_HEIGHT));
        assert_eq!(rows("type: sparkline\nheight: 99\ndata: [1]\n", 12), 30);
        assert_eq!(rows("type: sparkline\nheight: 7\ndata: [1]\n", 12), 7);
    }

    /// Under `NO_COLOR` the series take the plain markers in turn, starting
    /// with the one asked for and starting over at the fifth; with colour
    /// all share it.
    #[test]
    fn under_no_color_each_series_has_its_own_marker() {
        assert_eq!(
            markers(Marker::Dot, 3, true),
            [Marker::Dot, Marker::Braille, Marker::Block]
        );
        assert_eq!(
            markers(Marker::Dot, MAX_SERIES, true),
            [
                Marker::Dot,
                Marker::Braille,
                Marker::Block,
                Marker::Bar,
                Marker::Dot,
                Marker::Braille,
                Marker::Block,
                Marker::Bar,
            ]
        );
        assert_eq!(markers(Marker::Dot, 2, false), [Marker::Dot, Marker::Dot]);
    }

    #[test]
    fn under_no_color_a_drawn_chart_has_no_colour() {
        let body = "type: line\nseries:\n  - {name: up, data: [1, 2, 3]}\n";
        let lines = render_chart(body, 40, 12, &style(true)).unwrap();
        let spans = lines.iter().flat_map(|line| &line.spans);
        assert!(
            spans
                .clone()
                .all(|span| span.style.fg.is_none() && span.style.bg.is_none()),
            "{lines:?}"
        );
        let text: String = lines.iter().map(ToString::to_string).collect();
        assert!(text.contains("up"), "the legend is drawn:\n{text}");
    }

    #[test]
    fn axis_labels_are_short() {
        assert_eq!(axis_label(2.0), "2");
        assert_eq!(axis_label(0.30000000000000004), "0.3");
        assert_eq!(axis_label(-0.001), "0");
    }

    #[test]
    fn every_note_is_one_line_naming_the_reason() {
        for (error, why) in [
            (ChartError::TooLarge, "spec larger than 64 KiB"),
            (
                ChartError::Invalid("no bars".to_string()),
                "invalid spec: no bars",
            ),
            (
                ChartError::TooNarrow {
                    needed: 20,
                    available: 12,
                },
                "too narrow: needs 20 columns, 12 available",
            ),
            (
                ChartError::Panicked("boom".to_string()),
                "renderer failed: boom",
            ),
        ] {
            assert_eq!(error.note(), format!("chart: {why} (showing source)"));
        }
    }
}
