# Primitive operand preparation — 2026-09-09

VM 数值准备现在通过一个共享 `ToPrimitive` 入口：原始值保持表示直接返回，对象及原 hint 交给已有 host。基于静态 Atom 优化 [PR #2](https://github.com/Eric-Song-Nop/quickjs-oxide/pull/2)，关联 [issue #8](https://github.com/pocket-stack/quickjs-oxide/issues/8) 与 stacked [PR #3](https://github.com/Eric-Song-Nop/quickjs-oxide/pull/3)。

VM numeric preparation now uses one shared `ToPrimitive` entry point: primitives retain their representation and return directly, while objects and the original hint use the existing host. This stacks on static Atom reuse; arithmetic implementations remain shared.

## Initial measurements / 首轮测量

同机同 release 参数，每版本/负载五个独立进程，前后交替顺序。正数表示耗时下降，负数表示变慢。所有 40 次运行输出检查通过。

Five independent processes per version/workload, alternating order, identical release settings. Positive reductions mean less time; negative reductions mean slower. All 40 outputs passed validation.

| Workload / 负载 | Before median ms / 修改前 | After median ms / 修改后 | Time reduction / 耗时下降 |
| --- | ---: | ---: | ---: |
| Integer loop ×10 million / 整数空循环一千万次 | 1967.352 | 1547.222 | +21.36% |
| Property reads ×20 million / 属性读取负载两千万次 | 7041.108 | 6522.871 | +7.36% |
| Fixed Richards ×10 / 固定 Richards 十次 | 1496.602 | 1506.159 | -0.64% |
| Original Richards harness / 原始 Richards harness | 6243.921 | 6295.843 | -0.83% |

Sample ranges / 样本范围：

- Integer loop ×10 million / 整数空循环一千万次: 1923.923–1985.769 ms → 1500.397–1723.616 ms.
- Property reads ×20 million / 属性读取负载两千万次: 6926.040–7081.772 ms → 6446.743–6726.171 ms.
- Fixed Richards ×10 / 固定 Richards 十次: 1494.998–1517.357 ms → 1492.278–1511.041 ms.
- Original Richards harness / 原始 Richards harness: 6174.643–6307.535 ms → 6233.639–6304.144 ms.

整数循环与属性读取负载的前后范围不重叠。属性读取负载也包含循环及累加运算，不能把其收益全部归因于属性查找。首轮 Richards 中位耗时略升且范围重叠，因此另外安排十轮定向复测；没有删除或替换首轮样本。

Integer-loop and property-workload ranges do not overlap. The property workload includes loop and accumulation work, so its gain is not an isolated property-lookup improvement. Initial Richards medians were slightly slower with overlapping ranges, motivating a separate ten-pair confirmation; the original samples were not removed or replaced.

## Richards confirmation / Richards 定向复测

所有验证完成后，使用哈希相同的两份二进制，不重新构建。每版本/负载十次独立进程，交替顺序且首轮先运行修改后版本。另 40 次输出检查全部通过。

After validation, reuse the exact same binaries without rebuilding. Ten processes per version/workload, alternating order starting with the after version. All 40 additional outputs passed.

| Workload / 负载 | Before median ms / 修改前 | After median ms / 修改后 | Time reduction / 耗时下降 |
| --- | ---: | ---: | ---: |
| Fixed Richards ×10 / 固定 Richards 十次 | 1498.446 | 1494.523 | +0.26% |
| Original Richards harness / 原始 Richards harness | 6224.281 | 6230.859 | -0.11% |

- Fixed Richards ×10 / 固定 Richards 十次: 1486.070–1518.298 ms → 1489.392–1515.162 ms.
- Original Richards harness / 原始 Richards harness: 6175.498–6431.029 ms → 6174.804–6299.447 ms.

Original harness median scores / 原始 harness 中位分数：Initial / 首轮: **23.80 → 23.60**；Confirmation / 复测: **23.90 → 23.85**. Scores are separate from whole-process times. / 分数与完整进程耗时是不同指标。

本轮没有建立稳定的 Richards 加速结论；报告保留小幅回退的可能性，不将整数循环收益推广到整个引擎。

This round does not establish a stable Richards speedup; a small regression remains possible. Integer-loop gains are not a whole-engine speedup claim.

## Implementation / 实现

- `src/engine/vm/numeric.rs`: shared inline `to_primitive` returns all primitive kinds unchanged and delegates only objects; existing `to_numeric` uses it. / 共享入口按原始值与对象的语义边界分流，已有数值准备复用它。
- `src/engine/vm/numeric_execution.rs`: replace eight direct host calls across numeric preparation and unary/add/comparison paths in total (one in `numeric.rs`, seven here). Arithmetic branches, hints, conversion order and exception handling remain intact. / 共替换八处调用（此文件七处），不复制计算逻辑。
- Preserve `Int/Float` tags for unary/update operations rather than routing all operations through `NumericValue::Number(f64)`. Runtime retains its general ToPrimitive implementation for other callers. / 不把所有操作强制统一成 f64，避免改变现有标签行为；Runtime 转换仍服务其他调用者。
- Detached-host call tracking is test-only. Production adds no counter, cache or persistent allocation. / host 调用记录只用于测试，生产代码不增加计数器、缓存或持久分配。

## Validation / 验证

- **3002 Rust tests passed**, 0 failed, one existing ignored, across 13 binaries (Rust 1.88, workspace/all-targets/profiling). / 3002 项通过，零失败，一项原有忽略。
- New regressions cover all primitive kinds, integral Float tags, negative zero and NaN bits, host bypass, object hint/order/abrupt completion, ordinary conversion fallback, numeric boundaries, strings and BigInt. Existing numeric tag/boundary tests also pass. / 新增测试覆盖原始值表示、host 绕过、对象可观察行为及混合类型语义。
- Formatting, source layout (419 Rust files / 65 READMEs), Rust-only and strict production Clippy pass for profiling/default/test262-host. / 格式、布局、Rust-only 与三种配置的生产目标 Clippy 通过。
- Binary-object production boundary passed; **all 690 isolation canaries rejected**. / 字节码生产边界检查通过，全部隔离 canary 按预期拒绝。
- Full Test262 ran separately on both revisions: **102037 outcome rows identical**, zero changed outcomes/new failures. Both retain 79982 passes, 7 parse failures, 43 runtime failures, 3530 unsupported and 18475 skipped. Only the first fingerprint header line differs. Both frozen full-file gates exit 1; no baseline was updated. / 前后完整结果逐行一致；冻结文件校验仍因源码指纹头变化失败，未更新基线。

## Reproduction and evidence / 复现与证据

Before / 修改前: `eb48e7163423b69379a2e98a8d6c7aff812b1fbf`. After / 修改后: `6e59179e042b7ad93659136b9ba1af99f8aa9cdb`. Later documentation commits do not change measured runtime code. / 后续文档提交不改变所测运行时代码。

Heavy work ran serially through Herdr on eric-83am / PocketLab (AMD Ryzen 7 7840HS, Linux 6.18.44-1-lts). Release Rust 1.94.1, profiling compiled but inactive, no forced frame pointers or diagnostic patch. / 重型工作均在远端串行执行，诊断关闭。

```sh
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
CARGO_TARGET_DIR=target/cpu-profile \
QUICKJS_OXIDE_BUILD_COMMIT=$(git rev-parse HEAD) \
cargo build --locked --release -p quickjs-oxide-cli \
  --no-default-features --features profiling
```

Workloads reused unchanged from `ahaoboy/js-engine-benchmark@2034d98fc8c5f8044e186267593f5d5ea5232caf`. Python `time.perf_counter_ns()` measures complete subprocesses with a 90-second timeout. CPU frequency and system isolation are not controlled. No new CPU sampling, phase timing or memory probe was performed. / 使用未改动的固定工作负载，完整进程计时；未锁定频率或完全隔离系统，本轮不重采 CPU 栈或阶段/内存探针。

[Machine-readable evidence / 机器可读证据](primitive-coercion-fast-path.json) contains all 80 samples, output validation data, commands, hashes, test receipts and environment metadata. Outcome-row SHA-256: `fa99d3349bb4b61f30ba57d7c7f275df64f19691edefc50b96b53887339fa8c3`.

Raw archive / 原始包: `quickjs-oxide-primitive-coercion-pocketlab-2026-09-09.tar.gz`, **22471956 bytes**, SHA-256 **`d7c8d752afe2b97c78726985cfae61a9f2f9fda7cf0d26940436d56f96ae5b2e`**. All 199 manifest members verified remotely; downloaded archive hash matches. Includes both exact executables, all measurements, validation logs, complete Test262 vectors, scripts and source diff. Retained in local/remote workspace `target/`, not attached to GitHub; external benchmark source is excluded. / 所有清单成员逐一验证，本地下载哈希一致；原始包保留于工作区，不作为 GitHub 附件，不包含外部 benchmark 源码。
