//! Checks bviz's query language against Bazel's own query engine, run as
//! `query_oracle`, on expressions drawn at random: each must parse to the same
//! tree, which both print the same way, or fail with the same message; and over
//! graphs drawn at random, each must name the same targets or fail alike.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use bviz_graph_layout::query_lang::{ArgType, Function, parse};
use fastrand::Rng;

mod evaluation;

/// The seeds expressions are drawn from, unless `QUERY_ORACLE_SEEDS` lists
/// others, comma-separated.
const SEEDS: &[u64] = &[1, 2, 3, 5, 8, 13, 21, 34];

/// The seeds listed in the environment variable `name`, comma-separated, or
/// `default` where it lists none.
fn seeds(name: &str, default: &[u64]) -> Vec<u64> {
    match std::env::var(name) {
        Ok(list) => list
            .split(',')
            .map(|seed| seed.trim().parse().expect("a seed is an integer"))
            .collect(),
        Err(_) => default.to_vec(),
    }
}
const EXPRESSIONS_PER_SEED: usize = 1500;
/// How deeply drawn expressions nest.
const MAX_DEPTH: u32 = 3;
/// How many disagreements a failure lists.
const REPORTED_MISMATCHES: usize = 20;

/// Words an expression is built from: patterns, keywords, variable
/// references, integers, and text only quoting keeps one word.
const WORDS: &[&str] = &[
    "a",
    "b",
    "//a",
    "//a/b:c",
    "//a/...",
    "//...:all",
    "a/...:all-targets",
    "@r//x:y",
    "@@ext+//p:t",
    "@@rules_jvm_external++maven+maven//:bar",
    "@a+@b",
    ":t",
    "foo*",
    "*",
    "-x",
    "$x",
    "$_",
    "x",
    "in",
    "let",
    "set",
    "union",
    "except",
    "intersect",
    "0",
    "3",
    "-1",
    "+2",
    "007",
    "2147483648",
    "",
    "a b",
    "a+b",
    "a=b",
    "x[1]",
    "a'b",
    "a\"b",
    "a,b",
    "a(b)",
    "#c",
    "!x",
    "ü//:äöü",
    "//:🌱",
    ".*_test",
    "jar$",
];

/// Names a call is drawn with, beyond Bazel's own functions.
const UNKNOWN_FUNCTIONS: &[&str] = &["nope", "Deps", "deps_"];

/// One in this many expressions is drawn long.
const LONG_EXPRESSION_ODDS: usize = 40;
/// The length past which Bazel truncates an expression it quotes.
const LONG_EXPRESSION_BYTES: usize = 1000;
const TRUNCATION_MARK: &str = "[truncated]";

/// Ends each expression the oracle reads and each answer it gives, as no
/// expression holds one.
const RECORD_END: char = '\0';

const OPERATORS: &[&str] = &["+", "-", "^", "union", "except", "intersect"];

/// Characters spliced into an expression to break it.
const NOISE: &[&str] = &["(", ")", ",", "=", "'", "\"", " ", "+", "let", "in", "set"];

#[test]
fn parses_as_bazel_does() {
    let seeds = seeds("QUERY_ORACLE_SEEDS", SEEDS);
    let cases: Vec<(u64, String)> = seeds
        .iter()
        .flat_map(|&seed| {
            let mut rng = Rng::with_seed(seed);
            (0..EXPRESSIONS_PER_SEED).map(move |_| (seed, expression(&mut rng)))
        })
        .collect();

    let answers = ask_bazel(None, cases.iter().map(|(_, expr)| expr.as_str()));
    assert_eq!(
        answers.len(),
        cases.len(),
        "the oracle answers every expression"
    );

    let mut parsed = 0;
    let mut mismatches = Vec::new();
    for ((seed, input), answer) in cases.iter().zip(&answers) {
        let ours = match parse(input) {
            Ok(expr) => {
                parsed += 1;
                format!("parse\t{expr}")
            }
            Err(error) => format!("error\t{}", error.message),
        };
        if &ours != answer {
            mismatches.push(format!(
                "seed {seed}: {input:?}\n  bazel: {answer:?}\n  bviz:  {ours:?}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} expressions read differently:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches[..mismatches.len().min(REPORTED_MISMATCHES)].join("\n")
    );
    // Both outcomes are exercised, so neither half of the comparison is idle.
    assert!(
        parsed > 0 && parsed < cases.len(),
        "{parsed} of {}",
        cases.len()
    );
}

/// Bazel's answer to each expression, one record apiece: how it parses, or
/// with the streamed_proto dump at `graph`, what it names there.
fn ask_bazel<'a>(graph: Option<&Path>, expressions: impl Iterator<Item = &'a str>) -> Vec<String> {
    let runfiles = runfiles::Runfiles::create().unwrap();
    let oracle = runfiles::rlocation!(runfiles, std::env::var("QUERY_ORACLE").unwrap()).unwrap();
    let mut child = Command::new(oracle)
        .args(graph)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = String::new();
    for expression in expressions {
        input.push_str(expression);
        input.push(RECORD_END);
    }
    let mut stdin = child.stdin.take().unwrap();
    // Written from its own thread so neither pipe fills while the other waits.
    let writer = bviz_parallel::spawn(move || stdin.write_all(input.as_bytes()));
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    writer.join().unwrap().unwrap();
    assert!(child.wait().unwrap().success());
    let end = RECORD_END as u8;
    let records = output.strip_suffix(&[end]).unwrap_or(&output);
    records
        .split(|&byte| byte == end)
        .map(answer_text)
        .collect()
}

/// An answer as text. Bazel cuts a long expression it quotes at a byte count,
/// which can split a character; the bytes of the split character are dropped,
/// as bviz's parser drops them.
fn answer_text(record: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(record) {
        return text.to_owned();
    }
    let cut = record
        .windows(TRUNCATION_MARK.len())
        .position(|window| window == TRUNCATION_MARK.as_bytes())
        .expect("only a truncation splits a character");
    let quoted = record[..cut].utf8_chunks().map(|chunk| chunk.valid());
    let rest = std::str::from_utf8(&record[cut..]).unwrap();
    quoted.chain([rest]).collect()
}

/// An expression, now and then broken by a stray token or a cut.
fn expression(rng: &mut Rng) -> String {
    let mut text = match rng.usize(..LONG_EXPRESSION_ODDS) {
        0 => long_expression(rng),
        _ => expr(rng, MAX_DEPTH),
    };
    match rng.u8(..10) {
        0 => {
            let at = char_boundary(rng, &text);
            text.insert_str(at, NOISE[rng.usize(..NOISE.len())]);
        }
        1 => text.truncate(char_boundary(rng, &text)),
        _ => {}
    }
    text
}

/// A run of operands long enough that Bazel truncates it where an error
/// quotes it, as one does when it ends in a stray parenthesis.
fn long_expression(rng: &mut Rng) -> String {
    let mut text = literal(rng);
    while text.len() <= LONG_EXPRESSION_BYTES {
        let op = OPERATORS[rng.usize(..OPERATORS.len())];
        text = format!("{text} {op} {}", literal(rng));
    }
    if rng.bool() {
        text.push_str(" )");
    }
    text
}

fn char_boundary(rng: &mut Rng, text: &str) -> usize {
    let boundaries: Vec<usize> = (0..=text.len())
        .filter(|&at| text.is_char_boundary(at))
        .collect();
    boundaries[rng.usize(..boundaries.len())]
}

fn expr(rng: &mut Rng, depth: u32) -> String {
    let choice = if depth == 0 { 0 } else { rng.u8(..6) };
    match choice {
        0 => word(rng),
        1 => {
            let mut text = expr(rng, depth - 1);
            for _ in 0..rng.usize(1..4) {
                let op = OPERATORS[rng.usize(..OPERATORS.len())];
                text = format!(
                    "{text}{}{op}{}{}",
                    space(rng),
                    space(rng),
                    expr(rng, depth - 1)
                );
            }
            text
        }
        2 => format!("({}{})", expr(rng, depth - 1), space(rng)),
        3 => format!(
            "let {} = {} in {}",
            word(rng),
            expr(rng, depth - 1),
            expr(rng, depth - 1)
        ),
        4 => {
            let words: Vec<String> = (0..rng.usize(..4)).map(|_| word(rng)).collect();
            format!("set({})", words.join(" "))
        }
        _ => call(rng, depth),
    }
}

/// A call, usually with the arguments its function takes, sometimes with one
/// too few, one too many, or one of the wrong type.
fn call(rng: &mut Rng, depth: u32) -> String {
    let function = Function::BAZEL[rng.usize(..Function::BAZEL.len())];
    let name = match rng.u8(..20) {
        0 => UNKNOWN_FUNCTIONS[rng.usize(..UNKNOWN_FUNCTIONS.len())],
        _ => function.name(),
    };
    let name = match rng.u8(..10) {
        0 => format!("'{name}'"),
        _ => name.to_owned(),
    };
    let signature = function.signature();
    let declared = (0..)
        .map_while(|index| signature.param(index).filter(|_| index < 4))
        .count();
    let count = match rng.u8(..8) {
        0 => signature.mandatory.saturating_sub(1),
        1 => declared + 1,
        _ => rng.usize(signature.mandatory..=declared),
    };
    let args: Vec<String> = (0..count)
        .map(|index| {
            let ty = match rng.u8(..12) {
                0 => ArgType::Expr,
                1 => ArgType::Int,
                _ => signature.param(index).unwrap_or(ArgType::Expr),
            };
            match ty {
                ArgType::Expr => expr(rng, depth - 1),
                ArgType::Word => word(rng),
                ArgType::Int => rng.i32(-2..12).to_string(),
            }
        })
        .collect();
    let separator = format!(",{}", space(rng));
    format!("{name}({})", args.join(&separator))
}

/// A word quoted so it parses as a target literal.
fn literal(rng: &mut Rng) -> String {
    loop {
        let word = WORDS[rng.usize(..WORDS.len())];
        if !word.starts_with(['-', '*']) {
            let quote = if word.contains('"') { '\'' } else { '"' };
            return format!("{quote}{word}{quote}");
        }
    }
}

/// A word, written bare, single-quoted or double-quoted.
fn word(rng: &mut Rng) -> String {
    let word = WORDS[rng.usize(..WORDS.len())];
    match rng.u8(..4) {
        0 => format!("'{word}'"),
        1 => format!("\"{word}\""),
        _ => word.to_owned(),
    }
}

fn space(rng: &mut Rng) -> &'static str {
    const SPACES: &[&str] = &["", " ", "  ", "\t", "\n", "\r", "\r\n"];
    SPACES[rng.usize(..SPACES.len())]
}
