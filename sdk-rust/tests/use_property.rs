//! A property test of `use_meter` over the real `License`: seeded random
//! sequences of spends, check-ins and releases, from two processes of the app
//! over one state file, against a server whose answers are lost, late, refused or
//! lost after it has recorded the spend, with a disk that refuses its writes and a
//! device clock that jumps forward and back. The properties (`property/world.rs`):
//!
//! - `use_meter` and `status` never panic;
//! - a spend the offline allowance covers is refused only for the journal's size,
//!   and asks the server unless a wait runs;
//! - a spend it does not cover is never refused without asking, unless a void under
//!   a minute old stands (by the device clock too), and never asks while one does;
//! - the stored journal holds no more than 64 entries (but for the spends the
//!   customer was told were made while only memory could hold them) and 16 voids,
//!   and a process holds no more than 16 voids in memory, none of them marked told;
//! - offline, the unsynced spends of a meter never pass its allowance, those same
//!   told spends apart;
//! - the server never holds more than the customer was told and the refusals it
//!   recorded, at any step; once everything is reported it holds every spend the
//!   customer was told was made, once (but for what the contract lets a release or
//!   the end of a process drop unreported), a spend they were refused after it
//!   recorded it is there only when nothing could have lost its void, and every
//!   status shows what the server has left;
//! - what `use_meter` answers as `left` is the figure less the unsynced less the
//!   spend when made offline, and never more than the server has when made online.
//!
//! The oracle takes its constants, figures and waits from the contract, not from
//! the code it judges. A case replayed runs the same way every time (a test below
//! holds it to that), which is what lets a failing one be shrunk and kept.
//! `PROPERTY_RUNS` and `PROPERTY_SEED` choose how many sequences run and from which
//! seed: the default is 1500, a hunt is `PROPERTY_RUNS=20000`. A failing case is
//! shrunk and printed as data, with what happened: add it to the kept cases
//! (`property/cases.rs`).

mod common;
mod property;

use keygrant::SpendKind;
use property::case::{generate, Case, Rng};
use property::cases::{FOUND, GOLDEN, SHAPES};
use property::net::PropertyFailure;
use property::run_case;

/// A failing case with steps taken out while it fails the same way: the shortest sequence that shows the bug.
fn shrink(case: &Case, failure: &PropertyFailure) -> Case {
    let mut best = case.clone();
    let mut size = best.steps.len().div_ceil(2);
    while size >= 1 {
        let mut at = 0;
        while at < best.steps.len() {
            let mut steps = best.steps.clone();
            steps.drain(at..(at + size).min(steps.len()));
            let trial = best.with_steps(steps);
            match run_case(&trial, false).0 {
                Some(again) if again.invariant == failure.invariant => best = trial,
                _ => at += size,
            }
        }
        if size == 1 {
            break;
        }
        size /= 2;
    }
    let lean = Case {
        preload: Vec::new(),
        ..best.clone()
    };
    match run_case(&lean, false).0 {
        Some(bare) if bare.invariant == failure.invariant => lean,
        _ => best,
    }
}

fn setting(name: &str, fallback: i64) -> i64 {
    match std::env::var(name) {
        Err(_) => fallback,
        Ok(text) => text
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a whole number: {text}")),
    }
}

#[test]
fn keeps_every_property_of_the_contract_over_seeded_sequences() {
    let runs = setting("PROPERTY_RUNS", 1500);
    let first = setting("PROPERTY_SEED", 1);
    // A seed or a count that would run nothing, and pass.
    assert!(runs >= 1, "PROPERTY_RUNS must be at least 1");
    for seed in first..first + runs {
        let case = generate(seed);
        let Some(failure) = run_case(&case, false).0 else {
            continue;
        };
        let small = shrink(&case, &failure);
        let (again, trace) = run_case(&small, true);
        panic!(
            "seed {seed}: {}\nshrunk to {} steps ({}):\n{}\n{}",
            failure.message,
            small.steps.len(),
            again.map_or("no longer fails".to_owned(), |a| a.message),
            small.to_json(),
            trace.join("\n"),
        );
    }
}

/// The first line two traces differ at, with the lines before it.
fn difference(a: &str, b: &str) -> String {
    let (x, y): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let at = (0..x.len().min(y.len()))
        .find(|i| x[*i] != y[*i])
        .unwrap_or(x.len().min(y.len()));
    let before = x[at.saturating_sub(6)..at].join("\n");
    format!(
        "{before}\n--- first run, line {at}:\n{}\n--- other run:\n{}",
        x.get(at).unwrap_or(&"(ends)"),
        y.get(at).unwrap_or(&"(ends)")
    )
}

#[test]
fn a_case_replayed_runs_the_same_way_every_time_the_same_requests_answers_and_journals() {
    for seed in 1..=60 {
        let case = generate(seed);
        let first = run_case(&case, true);
        let trace = |run: &(Option<PropertyFailure>, Vec<String>)| {
            let ended = run
                .0
                .as_ref()
                .map_or(String::new(), |f| format!("\nfailed: {}", f.message));
            format!("{}{ended}", run.1.join("\n"))
        };
        let first = trace(&first);
        for again in 0..2 {
            let next = trace(&run_case(&case, true));
            assert!(
                first == next,
                "seed {seed} ran differently the {}nd time:\n{}",
                again + 2,
                difference(&first, &next)
            );
        }
    }
}

#[test]
fn generates_the_first_draws_the_reference_generates() {
    // The reference's generator, line for line: the first draws of mulberry32(1).
    let mut rng = Rng::new(1);
    let draws = [rng.next(), rng.next(), rng.next()];
    assert_eq!(
        draws,
        [0.6270739405881613, 0.002735721180215478, 0.5274470399599522]
    );
}

#[test]
fn generates_the_case_the_reference_generates_from_a_seed_so_a_seed_found_there_means_the_same_here(
) {
    for (seed, reference) in GOLDEN {
        let made = generate(*seed);
        // Both through the same serializer, so a number the reference wrote is compared as the number it is.
        assert_eq!(
            Case::from_json(reference).to_json(),
            made.to_json(),
            "seed {seed}"
        );
        // And it is data: the case survives its own JSON.
        assert_eq!(
            made.to_json(),
            Case::from_json(&made.to_json().to_string()).to_json(),
            "seed {seed}"
        );
    }
}

#[test]
fn generates_the_long_cases_the_reference_generates_by_what_they_hold() {
    // A storm of voids (6) and a journal past its cap (115): their size, their steps in order and their first draw.
    for (seed, preload, voids, steps, first_draw) in SHAPES {
        let made = generate(*seed);
        assert_eq!(made.preload.len(), *preload, "seed {seed}");
        assert_eq!(
            made.preload
                .iter()
                .filter(|p| p.kind == SpendKind::Void)
                .count(),
            *voids,
            "seed {seed}"
        );
        let kinds: Vec<&str> = made.steps.iter().map(|s| s.kind()).collect();
        assert_eq!(kinds.join(" "), *steps, "seed {seed}");
        let draw = made.steps.iter().find_map(|s| s.draws().first().copied());
        assert_eq!(draw, Some(*first_draw), "seed {seed}");
    }
}

/// A kept case holds: each is a bug the property test found, shrunk to the steps that show it.
fn holds_the_found_case(index: usize) {
    let (name, json) = FOUND[index];
    let (failure, trace) = run_case(&Case::from_json(json), true);
    assert!(
        failure.is_none(),
        "{name}: {}\n{}",
        failure.map_or(String::new(), |f| f.message),
        trace.join("\n")
    );
}

macro_rules! found_cases {
    ($($name:ident: $index:expr),* $(,)?) => {
        $( #[test] fn $name() { holds_the_found_case($index) } )*
    };
}

found_cases! {
    holds_a_spend_held_in_memory_is_not_reported_under_its_own_id: 0,
    holds_a_spend_held_in_memory_is_not_reported_in_the_entry_it_joins_in_its_own_view: 1,
    holds_a_spend_held_in_memory_is_not_reported_while_another_process_lands_entries_beside_it: 2,
    holds_spends_held_in_memory_are_not_reported_while_voids_come_and_go_beside_them: 3,
    holds_a_landing_that_opens_several_voids_in_a_journal_of_fifteen_replaces_the_oldest: 4,
    holds_a_landing_that_opens_a_void_and_confirms_it_in_the_same_save: 5,
    holds_a_second_spend_that_meets_the_journal_the_first_filled: 6,
    holds_a_void_held_in_memory_that_lands_with_the_confirmation_that_takes_the_oldest_off: 7,
    holds_two_calls_of_one_process_on_one_meter_the_first_made_offline_and_the_second_refused: 8,
}

#[test]
fn every_kept_case_has_a_test() {
    assert_eq!(FOUND.len(), 9);
}
