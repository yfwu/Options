# validation/ — OptLab 引擎獨立驗證

報告：[`docs/validation/REPORT.md`](../docs/validation/REPORT.md)（繁體中文）。

- `harness/`：獨立 Rust crate（不在 workspace members 內），只使用 `optlab-core` 的公開 API。`Cargo.lock` 複製自 workspace，libc 保持 0.2.189。
  - `cargo run --release -- jobs <jobs.json> <out.jsonl>`：批次回測，JSON 中的 `cfg` 會合併到 `RollConfig::qqq()/spy()` 上。
  - `cargo run --release -- boot <spec.json> <out.jsonl>`：區塊拔靴。
  - `cargo run --release --bin repro`：報告 §4 的最小重現。
- `py/optlab_ref.py`：獨立 Python 參考實作（BS、合成曲面、成本、滾動回測）。
- `run_all.sh`：重現報告中所有數字與圖表。大型輸出已 gitignore。
