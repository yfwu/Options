#!/usr/bin/env bash
# Reproduce every number / chart in docs/validation/REPORT.md.
# Requires: cargo, python3 with numpy pandas scipy matplotlib tabulate, fonts-noto-cjk.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p out/curves out/crash_curves ../docs/validation
(cd harness && cargo build --release)   # stand-alone crate; Cargo.lock copied from the workspace (libc 0.2.189)
H=harness/target/release/optlab-validation-harness
$H pricing out/rust_pricing.csv
$H synth out/rust_synth.csv
$H inputs out
python3 py/make_stress.py
$H jobs out/jobs_stress.json out/stress.jsonl
python3 py/analyze_stress.py > /dev/null
python3 py/make_jobs.py
for j in main attr full_grid band cc proxy; do $H jobs out/jobs_$j.json out/$j.jsonl; done
for s in qqq spy; do for t in uncond stress; do $H boot out/boot_${s}_${t}.json out/boot_${s}_${t}.jsonl; done; done
(cd py && python3 check_pricing.py && python3 chain_check.py && python3 term_structure.py && python3 replicate.py && python3 robustness_py.py)
python3 py/attribution.py
python3 py/analyze_boot.py > /dev/null
python3 py/charts.py
python3 py/build_report.py
