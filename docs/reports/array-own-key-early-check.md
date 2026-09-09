# Array key early check — 2026-09-09

`array_own_key` 现在先判断对象 payload，非数组直接返回 `Other`，避免无用的 `"length"` intern。只读 state 借用在 intern 前结束，避免 RefCell 冲突；真正数组仍按原规则返回 `Length`、`Index` 或 `Other`。这是对 [CPU 热点报告](cpu-hotspots.md) 中一个具体候选的独立修复，关联 [issue #6](https://github.com/pocket-stack/quickjs-oxide/issues/6)。

`array_own_key` now checks the object payload first and returns `Other` for non-arrays before interning `"length"`. The read borrow ends before interning to avoid a RefCell conflict; genuine arrays retain the existing `Length`/`Index`/`Other` classification. This isolates one candidate from the [CPU investigation](cpu-hotspots.md), tracked in [issue #6](https://github.com/pocket-stack/quickjs-oxide/issues/6).

## Results / 结果

| Measurement / 指标 | Before / 修改前 | After / 修改后 | Change / 变化 |
| --- | ---: | ---: | ---: |
| Fixed Richards ×10, median process ms / 固定工作量进程中位耗时 | 2411.234 | 2167.421 | −10.11% time / 耗时 |
| Original Richards harness, median process ms / 原 harness 进程中位耗时 | 9627.840 | 8726.416 | −9.36% time / 耗时 |
| Original Richards median score / 原 harness 中位分数 | 14.7 | 16.2 | +10.20% score / 分数 |

固定工作量的五次范围：修改前 2391.683–2444.869 ms，修改后 2145.846–2174.457 ms。20 次运行全部成功，stderr 均为空；固定脚本输出严格校验为 `richards-fixed:10`，保留 Richards 内部正确性断言；原 harness 必须输出相同数值的 `Richards` 和总分两行。

Fixed-workload ranges across five runs: before 2391.683–2444.869 ms, after 2145.846–2174.457 ms. All 20 runs succeeded with empty stderr. Fixed-script output was checked exactly against `richards-fixed:10`, with Richards' internal correctness checks retained; the original harness had to emit matching Richards and aggregate scores.

数字为同一台机器上的观测，不代表所有工作负载的收益。完整进程时间包含初始化、编译、执行和收尾；原 harness 包含其计时与 warmup，不能当作固定工作量或单次操作延迟。没有固定 CPU 频率或完全隔离系统，没有用原先 13.10% 的 inclusive CPU 占比代替实测收益。本次未重新采集 CPU 栈，也未测数组密集型性能，无法据此保证所有数组工作负载无回退。

These are observations on one machine, not a general speedup guarantee. Process time includes initialization, compilation, execution and teardown; the original harness includes its timing/warmup and is not a fixed-workload or per-operation latency measurement. CPU frequency/system isolation were not controlled. The earlier 13.10% inclusive CPU share was not substituted for measured gains. CPU stacks and array-heavy performance were not remeasured; these results cannot guarantee no regression across all array workloads.

## Reproduction / 复现

- Before: `16540677694d6358695160fd4351782388559f7f` (`perf/profilor`). After: `553c73e60681bea89dbb2d109e8d24c80ae95a75`. Later report commits do not change runtime code. / 后续报告提交不修改运行时代码。
- Herdr `eric-83am` / PocketLab: AMD Ryzen 7 7840HS, Linux 6.18.44-1-lts; Rust 1.94.1 release builds. Heavy work ran remotely. / 构建、测试和计时均远端执行。
- Both builds use the command below; binaries are copied before switching revisions. No diagnostic source probes; profiling is compiled but disabled at runtime. / 两版本采用相同构建参数，切换版本前保留二进制；无源码诊断探针，profiling 编译但运行时关闭。

```sh
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
CARGO_TARGET_DIR=target/cpu-profile \
QUICKJS_OXIDE_BUILD_COMMIT=$(git rev-parse HEAD) \
cargo build --locked --release -p quickjs-oxide-cli \
  --no-default-features --features profiling
```

复用前次调查的两个外部脚本，源码来自 `ahaoboy/js-engine-benchmark@2034d98fc8c5f8044e186267593f5d5ea5232caf`，内容未改。固定脚本连续调用十次 `runRichards()`；另一个保留原 harness。每个二进制分别执行 `qjs <script>` 五次，以 Python `time.perf_counter_ns()` 包围完整 subprocess，90 秒超时；偶数轮 before→after，奇数轮 after→before，全部串行，计时期间没有并行构建。

The two unchanged external scripts are reused from the prior investigation, sourced from `ahaoboy/js-engine-benchmark@2034d98fc8c5f8044e186267593f5d5ea5232caf`. One calls `runRichards()` ten times; the other retains the original harness. Execute `qjs <script>` five times per binary/workload, timing the full subprocess with Python `time.perf_counter_ns()` and a 90-second timeout. Even repetitions run before→after, odd repetitions after→before, all serially with no concurrent builds.

[JSON evidence](array-own-key-early-check.json) contains every sample, stdout/stderr, workload/binary SHA-256, commands and summaries. Logs are retained in local and remote `target/array-own-key-validation/`; matching before/after binaries remain remotely. External benchmark source is not vendored. / JSON 包含全部样本、输出、哈希、命令和汇总；日志保存在本地与远端上述目录，匹配二进制保存在远端，不引入外部 benchmark 源码。

## Validation / 验证

- `cargo fmt --all -- --check`: passed / 通过。
- `cargo +1.88.0 test --locked -p quickjs-oxide --lib --features profiling`: **1885 passed, 0 failed**. Existing coverage includes array index/length growth, read-only length, shrink rollback, deletion, conversion errors and GC. / 已有测试覆盖数组索引和长度增长、只读长度、缩短回滚、删除、转换异常及 GC。
- `cargo +1.88.0 clippy --locked -p quickjs-oxide -p quickjs-oxide-cli --lib --bins --features profiling -- -D warnings`: passed / 通过。
- An initial broader `--all-targets` strict Clippy run failed on three unused imports in unchanged context/string-test source. The production-target run above follows the repository's CI lint scope; the failed log is retained. / 初次扩大到所有测试目标的 strict Clippy 因未修改代码中的三处未使用导入失败；上述生产目标检查按 CI 范围通过，失败日志保留。
- Full Test262 was not rerun for this change. / 本次没有重跑完整 Test262，不将前次结果冒充本次验证。
