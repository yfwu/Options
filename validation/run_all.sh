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
# --- 修正後 (engine >= 6a0d2a3); "before" numbers are frozen in out/before/
$H jobs out/jobs_legacy.json out/legacy.jsonl          # legacy-mode equivalence check
$H inputs out
python3 - <<'PY'
import json
jobs=[]
for data in ['qqq','spy','qqq_preset:dgs3mo','spy:dgs3mo']:
    tag=data.replace(':','_')
    for nm,c in [('d80_1.0',dict(target_exposure=1.0)),('d80_2.0',dict(target_exposure=2.0)),('d60_1.5',dict(strike={'Delta':0.6},target_exposure=1.5)),('d80_1.0_lag0',dict(target_exposure=1.0,exec_lag_days=0))]:
        jobs.append(dict(name=f'{tag}_{nm}',data=data,cfg=c,records=f'out/curves/new_{tag}_{nm}.csv'))
json.dump(jobs,open('out/jobs_newrepl.json','w'))
PY
$H jobs out/jobs_newrepl.json out/newrepl.jsonl
(cd py && python3 replicate_new.py)
python3 py/make_after.py
$H jobs out/jobs_after_attr.json out/after_attr.jsonl
$H jobs out/jobs_stress_after.json out/stress_after.jsonl
$H jobs out/jobs_proxy.json out/proxy_after.jsonl
for s in qqq spy; do for t in uncond stress; do $H boot out/boot_after_${s}_${t}.json out/boot_after_${s}_${t}.jsonl; done; done
STRESS_IN=out/stress_after.jsonl STRESS_SUF=_after python3 py/analyze_stress.py > /dev/null
BOOT_PRE=boot_after python3 py/analyze_boot.py > /dev/null
python3 py/charts.py && python3 py/build_report.py
