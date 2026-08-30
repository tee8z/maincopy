# Code quality measurement

Maincopy uses Change Risk Anti-Patterns (CRAP) scores to identify risky
functions. CRAP combines cyclomatic complexity with executable-line coverage.
It does not measure design quality or correctness by itself.

The report uses the original formula from
[Alberto Savoia](https://testing.googleblog.com/2011/02/this-code-is-crap.html):

```text
CRAP = C^2 * (1 - p)^3 + C
```

`C` is Knots McCabe complexity. `p` is the covered fraction of executable
lines in the named function. The project requires every measured score to be
below 20. This limit applies to production, build, and test code.

The original review threshold was 30. Maincopy uses 20 as a stricter budget.
The [NDepend CRAP overview](https://blog.ndepend.com/crap-metric-thing-tells-risk-code/)
recommends reducing such limits gradually.

## Run the report

Use Bash, POSIX `awk`, and common Unix file utilities. Install these exact
measurement tools:

- `cargo-llvm-cov 0.8.5`
- `knots 1.16.0`

Run the pipeline from the workspace root:

```bash
KNOTS_BIN=/path/to/knots scripts/crap-report.sh
```

The script runs offline and does not install tools. It writes two raw inputs
and two reports below `target/crap/`:

- `coverage.lcov` and `complexity.json` reproduce the measurement.
- `crap.json` is the canonical machine-readable report.
- `crap.md` is the review summary.

The script runs these pinned measurement commands:

```bash
cargo llvm-cov --all-targets --all-features --include-build-script \
  --locked --offline --lcov --output-path target/crap/coverage.lcov
knots --recursive --language rust --count-anonymous-closures --format json \
  crates >target/crap/complexity.json
```

The parser resolves source paths before it joins the inputs. It merges duplicate
compilation contexts by physical path. A line is covered when any context
covers it.

The report separates production, build, and test functions. It recognizes test
paths, `test_support.rs`, test attributes, and inline test modules. Keep inline
test modules after the production items in a file.

Knots 1.16.0 folds Rust closure complexity into the enclosing named function.
It does not emit separate Rust closure records. A function with no instrumented
lines remains unscored and requires review.

The command exits with status 1 when any measured score is 20 or higher. It
writes the reports before it exits.

Rebuild a report without rerunning coverage or Knots:

```bash
scripts/crap-report.sh --input-dir target/crap \
  --output-dir target/crap-replay
```

This replay does not use Cargo or Knots. In the same source tree, identical
inputs produce byte-identical JSON and Markdown reports.

## Use the result

Use the score as a risk signal. Add meaningful branch tests before changing a
complex function. Then simplify the function without weakening its invariants.

Do not split one operation into arbitrary fragments. Do not add tests that only
execute lines. Security invariants and stable public contracts take priority.

The 2026-08-30 baseline measured 1,461 of 1,624 named functions. Twelve
production functions and three build functions had scores of at least 20. The
highest production, build, and test scores were 90.00, 26.18, and 6.00.

After the simplification and workspace split, the same-day final report measured
1,334 of 1,498 named functions. Canonical line coverage was 15,786 of 17,307
lines (91.21%). No measured function had a score of 20 or higher. The highest
production, build, and test scores were 15.02, 16.47, and 6.00.
