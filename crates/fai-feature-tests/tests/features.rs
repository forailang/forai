//! One libtest-mimic trial per fixture.

use fai_feature_tests::{discover_fixtures, run_fixture};
use libtest_mimic::{Arguments, Failed, Trial};

fn main() {
    let args = Arguments::from_args();
    let fixtures = discover_fixtures();
    if fixtures.is_empty() {
        eprintln!(
            "no fixtures discovered in {} — add a .fai file to exercise the harness",
            fai_feature_tests::fixtures_root().display(),
        );
    }

    let trials: Vec<Trial> = fixtures
        .into_iter()
        .map(|fx| {
            let name = fx.display_name.clone();
            let skip_reason = fx.skip.clone();
            let trial = Trial::test(name, move || -> Result<(), Failed> {
                run_fixture(&fx).map_err(|e| Failed::from(e.to_string()))
            });
            if let Some(reason) = skip_reason {
                trial.with_ignored_flag(true).with_kind(reason)
            } else {
                trial
            }
        })
        .collect();

    libtest_mimic::run(&args, trials).exit();
}
