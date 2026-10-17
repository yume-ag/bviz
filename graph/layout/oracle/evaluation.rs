//! Evaluates expressions drawn at random over query graphs drawn at random,
//! with bviz's engine and with Bazel's, and checks they name the same targets.
//!
//! A graph is written as Bazel writes `query --output=streamed_proto`, read by
//! bviz's query loader, and handed to Bazel's engine as targets whose
//! dependencies are those `LabelVisitationUtils.visitTarget` visits. bviz
//! draws a source file's `visibility_label` and a package group's includes as
//! no edges, so the graphs drawn state neither, and the two engines walk the
//! same edges.
//!
//! `attr` is drawn for the attributes bviz loads, which the graphs drawn state
//! as `--proto:output_rule_attrs` writes them: every rule its tags, some rules
//! `testonly`.
//!
//! `somepath` and `some` may each answer any of several sets. Drawn only at
//! the top of an expression, where nothing reads their answer further, each is
//! checked for being one Bazel's engine could give: a path of fewest edges
//! from the first argument down to the second, and as many of the first
//! argument as Bazel takes.
//!
//! An error's message is compared word for word, which checks Bazel's own
//! wording wherever Bazel's code raises it: a target pattern it cannot parse,
//! a variable `let` leaves unbound or cannot bind, an empty `some`. A pattern
//! naming a target, package or repository the dump lacks fails in the
//! oracle's resolver with bviz's wording, the dump holding no BUILD file for
//! Bazel's, so the comparison checks only that both fail there. Patterns
//! resolve as a query resolves them, keeping rules tagged `manual`; a build
//! command line's leaving them out has no counterpart in a query engine and
//! is checked against Bazel's loading-phase tests in `target_pattern`.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use bazel_query_proto::blaze_query::{
    Attribute, GeneratedFile, PackageGroup, Rule, SourceFile, Target,
    attribute::Discriminator as AttributeType, target::Discriminator,
};
use bviz_graph_layout::build_joint;
use bviz_graph_layout::query_lang::{Universe, parse};
use bviz_graph_layout::streamed_proto::read_query_targets;
use fastrand::Rng;
use prost::Message;

use super::{REPORTED_MISMATCHES, ask_bazel, seeds};

/// The seeds graphs are drawn from, unless `QUERY_ORACLE_GRAPH_SEEDS` lists
/// others, comma-separated.
const GRAPH_SEEDS: &[u64] = &[1, 2, 3, 5, 8, 13, 21, 34];
const EXPRESSIONS_PER_GRAPH: usize = 400;
/// The fewest of every this many expressions each outcome — naming nothing,
/// naming something, failing — must be drawn for.
const MIN_OUTCOME_SHARE: usize = 10;
/// How deeply drawn expressions nest.
const MAX_DEPTH: u32 = 3;

/// The packages a graph may hold, by the repository part their labels carry.
const PACKAGES: &[(&str, &str)] = &[
    ("", ""),
    ("", "a"),
    ("", "a/b"),
    ("", "a/b/c"),
    ("", "ab"),
    ("", "d"),
    ("@@ext+", ""),
    ("@@ext+", "p"),
    ("@@ext+", "p/q"),
];

const RULE_NAMES: &[&str] = &["lib", "app", "test", "tool", "all", "a", "b"];
const SOURCE_NAMES: &[&str] = &["s.txt", "x/y.txt", "data.json"];
const RULE_CLASSES: &[&str] = &["cc_library", "genrule", "java_binary", "sh_test"];

/// Patterns naming what no graph holds, or that no graph can resolve.
const MISSING: &[&str] = &[
    "//zz:x",
    "//a:nope",
    "//zz/...",
    "@@nope//x:y",
    "@nope//...",
    "@@ext//p:all",
    "//a/...:lib",
    "nowhere/at/all.txt",
];

/// Patterns over every graph.
const WIDE: &[&str] = &[
    "//...",
    "//...:*",
    "//...:all-targets",
    "...",
    "@ext//...",
    "@@ext+//...:*",
    "a/...",
    "a/b",
    "a:all",
];

#[test]
fn evaluates_as_bazel_does() {
    let mut cases = 0;
    // Expressions naming nothing, naming something, and failing.
    let mut outcomes = [0; 3];
    let mut mismatches: Vec<String> = Vec::new();
    for seed in seeds("QUERY_ORACLE_GRAPH_SEEDS", GRAPH_SEEDS) {
        let mut rng = Rng::with_seed(seed);
        let graph = Graph::draw(&mut rng);
        let path = graph.write(seed);
        let targets = read_query_targets(std::fs::File::open(&path).unwrap(), None).unwrap();
        let joint = build_joint(None, None, Some(&targets), false);
        let universe = Universe::new(
            &joint.graph,
            &joint.names,
            &Arc::new(joint.facets),
            &joint.capabilities,
        );
        let ours = Ours {
            universe: &universe,
            graph: &graph,
        };

        let expressions: Vec<Drawn> = (0..EXPRESSIONS_PER_GRAPH)
            .map(|_| Drawn::draw(&mut rng, &graph))
            .collect();
        let asked: Vec<&str> = expressions.iter().flat_map(Drawn::asked).collect();
        let answers = ask_bazel(Some(&path), asked.iter().copied());
        assert_eq!(
            answers.len(),
            asked.len(),
            "the oracle answers every expression"
        );
        let mut answers = answers.iter().map(String::as_str);
        for drawn in &expressions {
            cases += 1;
            let bazel: Vec<&str> = drawn.asked().map(|_| answers.next().unwrap()).collect();
            match named(bazel[0]) {
                Some(labels) if labels.is_empty() => outcomes[0] += 1,
                Some(_) => outcomes[1] += 1,
                None => outcomes[2] += 1,
            }
            if let Err(why) = ours.check(drawn, &bazel) {
                mismatches.push(format!("graph {seed}: {}\n  {why}", drawn.text));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {cases} expressions evaluated differently:\n{}",
        mismatches.len(),
        mismatches[..mismatches.len().min(REPORTED_MISMATCHES)].join("\n")
    );
    // Each outcome is exercised, so no part of the comparison is idle.
    assert!(
        outcomes
            .iter()
            .all(|&count| count * MIN_OUTCOME_SHARE >= cases),
        "{outcomes:?} of {cases}"
    );
}

/// A graph's targets, and the dependencies each states.
struct Graph {
    targets: Vec<Target>,
    /// Every label, in the order drawn.
    labels: Vec<String>,
    /// The packages holding a target, as `{repo}//{path}`.
    packages: Vec<(String, String)>,
    /// The labels each target depends on that the graph holds.
    deps: HashMap<String, Vec<String>>,
}

impl Graph {
    fn draw(rng: &mut Rng) -> Self {
        let mut packages: Vec<(String, String)> = PACKAGES
            .iter()
            .filter(|_| rng.u8(..4) != 0)
            .map(|&(repo, path)| (repo.to_string(), path.to_string()))
            .collect();
        if packages.is_empty() {
            packages.push((String::new(), "a".to_string()));
        }
        let mut rules: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        let mut files: Vec<Target> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        for (repo, path) in &packages {
            let label = |name: &str| format!("{repo}//{path}:{name}");
            let mut names: Vec<&str> = RULE_NAMES.to_vec();
            rng.shuffle(&mut names);
            let held = &names[..rng.usize(1..4)];
            for name in held {
                let tags = match rng.u8(..4) {
                    0 => vec!["manual".to_string()],
                    1 => vec!["local".to_string(), "manual".to_string()],
                    _ => Vec::new(),
                };
                rules.push((label(name), Vec::new(), tags));
                labels.push(label(name));
            }
            for name in SOURCE_NAMES.iter().filter(|_| rng.bool()) {
                files.push(source(&label(name)));
                labels.push(label(name));
            }
            for index in 0..rng.usize(..3) {
                let owner = rules.len() - 1 - rng.usize(..held.len());
                let output = label(&format!("out{index}.o"));
                rules[owner].1.push(output.clone());
                files.push(generated(&output, &rules[owner].0));
                labels.push(output);
            }
            if rng.u8(..6) == 0 {
                files.push(package_group(&label("group")));
                labels.push(label("group"));
            }
        }
        let mut targets: Vec<Target> = Vec::with_capacity(rules.len() + files.len());
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        for (name, outputs, tags) in &rules {
            let mut inputs: Vec<String> = (0..rng.usize(..4))
                .map(|_| labels[rng.usize(..labels.len())].clone())
                .filter(|input| input != name)
                .collect();
            if rng.u8(..8) == 0 {
                inputs.push("//zz:absent".to_string());
            }
            inputs.sort();
            inputs.dedup();
            let class = RULE_CLASSES[rng.usize(..RULE_CLASSES.len())];
            let mut attributes = vec![string_list("tags", tags)];
            if rng.u8(..3) == 0 {
                attributes.push(boolean("testonly", rng.bool()));
            }
            targets.push(Target {
                r#type: Discriminator::Rule as i32,
                rule: Some(Rule {
                    name: name.clone(),
                    rule_class: class.to_string(),
                    rule_input: inputs.clone(),
                    rule_output: outputs.clone(),
                    attribute: attributes,
                    ..Default::default()
                }),
                ..Default::default()
            });
            let held = inputs.into_iter().filter(|input| labels.contains(input));
            deps.insert(name.clone(), held.collect());
        }
        for file in &files {
            if let Some(generated) = &file.generated_file {
                deps.insert(
                    generated.name.clone(),
                    vec![generated.generating_rule.clone()],
                );
            }
        }
        targets.extend(files);
        rng.shuffle(&mut targets);
        Graph {
            targets,
            labels,
            packages,
            deps,
        }
    }

    /// Writes the graph as a streamed_proto dump, returning its path.
    fn write(&self, seed: u64) -> PathBuf {
        let directory = std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = directory.join(format!("graph{seed}.binpb"));
        let mut bytes: Vec<u8> = Vec::new();
        for target in &self.targets {
            target.encode_length_delimited(&mut bytes).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// A target pattern, mostly one naming what the graph holds.
    fn pattern(&self, rng: &mut Rng) -> String {
        let (repo, path) = &self.packages[rng.usize(..self.packages.len())];
        let slash = |path: &str| match path {
            "" => String::new(),
            path => format!("/{path}"),
        };
        match rng.u8(..32) {
            0..=11 => self.labels[rng.usize(..self.labels.len())].clone(),
            12 | 13 => format!("{repo}//{path}:all"),
            14 | 15 => format!("{repo}//{path}:*"),
            16 => format!("{repo}//{path}:all-targets"),
            17 | 18 => format!("{repo}/{}/...", slash(path)),
            19 => format!("{repo}/{}/...:*", slash(path)),
            20 => match path.as_str() {
                "" => format!("{repo}//:all"),
                path => format!("{repo}//{path}"),
            },
            21 | 22 if repo.is_empty() && !path.is_empty() => {
                let file = SOURCE_NAMES[rng.usize(..SOURCE_NAMES.len())];
                [path.clone(), format!("{path}/{file}")][rng.usize(..2)].clone()
            }
            23 => MISSING[rng.usize(..MISSING.len())].to_string(),
            24..=27 => WIDE[rng.usize(..WIDE.len())].to_string(),
            _ => self.labels[rng.usize(..self.labels.len())].clone(),
        }
    }
}

fn source(name: &str) -> Target {
    Target {
        r#type: Discriminator::SourceFile as i32,
        source_file: Some(SourceFile {
            name: name.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn generated(name: &str, rule: &str) -> Target {
    Target {
        r#type: Discriminator::GeneratedFile as i32,
        generated_file: Some(GeneratedFile {
            name: name.to_string(),
            generating_rule: rule.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn package_group(name: &str) -> Target {
    Target {
        r#type: Discriminator::PackageGroup as i32,
        package_group: Some(PackageGroup {
            name: name.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn string_list(name: &str, values: &[String]) -> Attribute {
    Attribute {
        name: name.to_string(),
        r#type: AttributeType::StringList as i32,
        string_list_value: values.to_vec(),
        ..Default::default()
    }
}

fn boolean(name: &str, value: bool) -> Attribute {
    Attribute {
        name: name.to_string(),
        r#type: AttributeType::Boolean as i32,
        boolean_value: Some(value),
        ..Default::default()
    }
}

/// An expression drawn at random, and how its answer is checked.
struct Drawn {
    text: String,
    check: Check,
}

enum Check {
    /// The two engines name the same targets.
    Same,
    /// `somepath(from, to)`, whose arguments are asked of Bazel as well.
    SomePath { from: String, to: String },
    /// `some(of, count)`, whose argument is asked of Bazel as well.
    Some { of: String, count: Option<i32> },
}

impl Drawn {
    fn draw(rng: &mut Rng, graph: &Graph) -> Self {
        let mut draw = Drawer { rng, graph };
        match draw.rng.u8(..10) {
            0 | 1 => {
                let (from, to) = (draw.expr(MAX_DEPTH - 1), draw.expr(MAX_DEPTH - 1));
                Drawn {
                    text: format!("somepath({from}, {to})"),
                    check: Check::SomePath { from, to },
                }
            }
            2 => {
                let of = draw.expr(MAX_DEPTH - 1);
                let count = draw.rng.bool().then(|| draw.rng.i32(-1..4));
                let text = match count {
                    Some(count) => format!("some({of}, {})", int(count)),
                    None => format!("some({of})"),
                };
                Drawn {
                    text,
                    check: Check::Some { of, count },
                }
            }
            _ => Drawn {
                text: draw.expr(MAX_DEPTH),
                check: Check::Same,
            },
        }
    }

    /// The expressions asked of Bazel for this one, itself first.
    fn asked(&self) -> impl Iterator<Item = &str> {
        let arguments: Vec<&str> = match &self.check {
            Check::Same => Vec::new(),
            Check::SomePath { from, to } => vec![from, to],
            Check::Some { of, .. } => vec![of],
        };
        std::iter::once(self.text.as_str()).chain(arguments)
    }
}

/// An integer argument as a query writes it, a negative one quoted, since the
/// lexer reads a bare `-` as the operator.
fn int(value: i32) -> String {
    match value < 0 {
        true => format!("'{value}'"),
        false => value.to_string(),
    }
}

struct Drawer<'a> {
    rng: &'a mut Rng,
    graph: &'a Graph,
}

/// The patterns `kind`, `filter` and `attr` are drawn with: constructs Java's
/// and the `regex` crate's engines read alike over ASCII text, which leaves out
/// look-around, backreferences, possessive quantifiers, atomic groups and
/// `\Q…\E` quoting.
const KIND_PATTERNS: &[&str] = &[
    "rule",
    "file",
    "source",
    "gen.*",
    "^genrule rule$",
    "_test",
    "package",
    "java|sh",
    "e r",
    "d f",
    "^s",
    "",
];
const LABEL_PATTERNS: &[&str] = &[
    "lib",
    "\\.txt$",
    "^@@ext",
    "//a/b",
    ":a",
    "out\\d",
    "a.*b",
    "^//a:",
    "x/y",
    "[.]json",
    "(?i)APP",
    "\\w+\\.o$",
];
const ATTRIBUTE_PATTERNS: [(&str, &[&str]); 3] = [
    (
        "tags",
        &["manual", "^\\[\\]$", "local", "\\[manual\\]", "", ", "],
    ),
    ("testonly", &["1", "0", "", "^1$"]),
    ("name", &["^a", "lib|app", "t$", "", "^[a-c]$"]),
];

/// The variables a drawn `let` binds; a reference to one may fall outside it.
const VARIABLES: &[&str] = &["v", "w"];

impl Drawer<'_> {
    fn expr(&mut self, depth: u32) -> String {
        let choice = if depth == 0 { 0 } else { self.rng.u8(..12) };
        match choice {
            0 | 1 => self.literal(),
            2 | 3 => {
                let mut text = self.expr(depth - 1);
                for _ in 0..self.rng.usize(1..3) {
                    let ops = ["+", "-", "^", "union", "except", "intersect"];
                    let op = ops[self.rng.usize(..ops.len())];
                    text = format!("{text} {op} {}", self.expr(depth - 1));
                }
                text
            }
            4 => format!("({})", self.expr(depth - 1)),
            5 => {
                let variable = VARIABLES[self.rng.usize(..VARIABLES.len())];
                let value = self.expr(depth - 1);
                let body = match self.rng.bool() {
                    true => format!("${variable} + {}", self.expr(depth - 1)),
                    false => self.expr(depth - 1),
                };
                format!("let {variable} = {value} in {body}")
            }
            6 => {
                let words: Vec<String> = (0..self.rng.usize(..3))
                    .map(|_| self.graph.pattern(self.rng))
                    .collect();
                format!("set({})", words.join(" "))
            }
            _ => self.call(depth),
        }
    }

    fn literal(&mut self) -> String {
        match self.rng.u8(..24) {
            0 => format!("${}", VARIABLES[self.rng.usize(..VARIABLES.len())]),
            _ => self.graph.pattern(self.rng),
        }
    }

    fn depth(&mut self) -> String {
        match self.rng.u8(..8) {
            0 => int(-1),
            _ => int(self.rng.i32(0..4)),
        }
    }

    /// One of `patterns`, quoted.
    fn pick(&mut self, patterns: &[&str]) -> String {
        format!("'{}'", patterns[self.rng.usize(..patterns.len())])
    }

    fn call(&mut self, depth: u32) -> String {
        let mut arg = || self.expr(depth - 1);
        let (one, two) = (arg(), arg());
        let bounded = self.rng.bool();
        let depth = self.depth();
        match self.rng.u8(..9) {
            0 if bounded => format!("deps({one}, {depth})"),
            0 => format!("deps({one})"),
            1 if bounded => format!("allrdeps({one}, {depth})"),
            1 => format!("allrdeps({one})"),
            2 if bounded => format!("rdeps({one}, {two}, {depth})"),
            2 => format!("rdeps({one}, {two})"),
            3 => format!("allpaths({one}, {two})"),
            4 => format!("siblings({one})"),
            5 => format!("same_pkg_direct_rdeps({one})"),
            6 => format!("kind({}, {one})", self.pick(KIND_PATTERNS)),
            7 => format!("filter({}, {one})", self.pick(LABEL_PATTERNS)),
            _ => {
                let (attribute, patterns) = ATTRIBUTE_PATTERNS[self.rng.usize(..3)];
                format!("attr({attribute}, {}, {one})", self.pick(patterns))
            }
        }
    }
}

/// bviz's engine over one graph.
struct Ours<'a> {
    universe: &'a Universe,
    graph: &'a Graph,
}

impl Ours<'_> {
    /// bviz's answer to `expr`, written as Bazel's oracle writes its own.
    fn answer(&self, expr: &str) -> String {
        let parsed = match parse(expr) {
            Ok(parsed) => parsed,
            Err(error) => return format!("error\t{}", error.message),
        };
        match self.universe.evaluate(&parsed) {
            Ok(value) => {
                let named = value.ones().map(|id| {
                    let id = bviz_graph_layout::Id(id as u32);
                    self.universe
                        .label(id)
                        .expect("a query graph labels every node")
                });
                let labels: BTreeSet<&str> = named.collect();
                let labels: Vec<&str> = labels.into_iter().collect();
                format!("ok\t{}", labels.join(" "))
            }
            Err(error) => format!("error\t{}", error.message),
        }
    }

    /// Whether bviz answers `drawn` as Bazel could, Bazel having answered
    /// `bazel` to [`Drawn::asked`].
    fn check(&self, drawn: &Drawn, bazel: &[&str]) -> Result<(), String> {
        let asked: Vec<&str> = drawn.asked().collect();
        let ours: Vec<String> = asked.iter().map(|expr| self.answer(expr)).collect();
        for (at, (ours, bazel)) in ours.iter().zip(bazel).enumerate().skip(1) {
            if ours != bazel {
                return Err(format!(
                    "{}\n  bazel: {bazel:?}\n  bviz:  {ours:?}",
                    asked[at]
                ));
            }
        }
        let differ = || Err(format!("bazel: {:?}\n  bviz:  {:?}", bazel[0], ours[0]));
        let (Some(ours_named), Some(bazel_named)) = (named(&ours[0]), named(bazel[0])) else {
            return match ours[0] == bazel[0] {
                true => Ok(()),
                false => differ(),
            };
        };
        match &drawn.check {
            Check::Same if ours_named == bazel_named => Ok(()),
            Check::Same => differ(),
            Check::SomePath { .. } => {
                let from = named(bazel[1]).unwrap_or_default();
                let to = named(bazel[2]).unwrap_or_default();
                let fewest = self.fewest_edges(&from, &to);
                let path_of_fewest = |named: &BTreeSet<&str>| match fewest {
                    None => named.is_empty(),
                    Some(edges) => named.len() == edges + 1 && self.joins(named, &from, &to),
                };
                match path_of_fewest(&ours_named) && bazel_named.is_empty() != fewest.is_some() {
                    true => Ok(()),
                    false => differ(),
                }
            }
            Check::Some { count, .. } => {
                let of = named(bazel[1]).unwrap_or_default();
                let taken = usize::try_from(count.unwrap_or(1)).unwrap_or(0);
                let fits = ours_named.is_subset(&of)
                    && ours_named.len() == taken.min(of.len())
                    && ours_named.len() == bazel_named.len();
                match fits {
                    true => Ok(()),
                    false => differ(),
                }
            }
        }
    }

    /// The fewest edges a path from one of `from` down to one of `to` takes.
    fn fewest_edges(&self, from: &BTreeSet<&str>, to: &BTreeSet<&str>) -> Option<usize> {
        let mut fewest: HashMap<&str, usize> = from.iter().map(|&label| (label, 0)).collect();
        let mut open: VecDeque<&str> = from.iter().copied().collect();
        while let Some(at) = open.pop_front() {
            if to.contains(at) {
                return Some(fewest[at]);
            }
            for dep in self.graph.deps.get(at).into_iter().flatten() {
                if !fewest.contains_key(dep.as_str()) {
                    fewest.insert(dep, fewest[at] + 1);
                    open.push_back(dep);
                }
            }
        }
        None
    }

    /// Whether `path` holds a path from one of `from` down to one of `to`
    /// running through `path` alone.
    fn joins(&self, path: &BTreeSet<&str>, from: &BTreeSet<&str>, to: &BTreeSet<&str>) -> bool {
        let mut seen: HashSet<&str> = path.intersection(from).copied().collect();
        let mut open: Vec<&str> = seen.iter().copied().collect();
        while let Some(at) = open.pop() {
            if to.contains(at) {
                return true;
            }
            for dep in self.graph.deps.get(at).into_iter().flatten() {
                if path.contains(dep.as_str()) && seen.insert(dep) {
                    open.push(dep);
                }
            }
        }
        false
    }
}

/// The labels an answer names, `None` for an error.
fn named(answer: &str) -> Option<BTreeSet<&str>> {
    let labels = answer.strip_prefix("ok\t")?;
    Some(
        labels
            .split(' ')
            .filter(|label| !label.is_empty())
            .collect(),
    )
}
