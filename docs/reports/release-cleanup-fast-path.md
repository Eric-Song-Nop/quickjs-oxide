# Release and cleanup fast paths — 2026-09-09

Runtime 释放现在先检查是否有延迟工作；Heap 仍有活引用且整个零引用队列为空时，不再构造/应用空清理结果。基于 [PR #3](https://github.com/Eric-Song-Nop/quickjs-oxide/pull/3)，关联 [issue #9](https://github.com/pocket-stack/quickjs-oxide/issues/9) 和 stacked [PR #4](https://github.com/Eric-Song-Nop/quickjs-oxide/pull/4)。

Runtime releases now check for pending deferred work before draining. Heap releases omit cleanup when the reference stays live and the entire zero-reference queue is empty. Existing safe points and destruction timing remain intact.

## Three-version measurement / 三版本测量

同机、同 release 参数，每负载/版本六个独立进程，按三版本全部六种排列交错运行（每个位置各两次），共 90 次输出检查全部通过。中间版本只含 Runtime 优化，最终版本再加 Heap 优化；阶段百分比各自以前一阶段为分母，不能直接相加。正数表示耗时下降。

Six independent processes per workload/version, using all six permutations of three versions. All 90 outputs passed. Runtime is the first implementation stage; Final adds optional heap cleanup. Stage reductions have different denominators and are not additive. Positive values mean lower elapsed time.

| Workload / 负载 | Before ms | Runtime ms | Final ms | Runtime gain | Heap gain | Total time reduction |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Integer loop ×10m / 整数空循环 | 1538.679 | 1496.283 | 1545.590 | +2.76% | -3.30% | -0.45% |
| Property workload ×20m / 属性负载 | 6232.549 | 5797.043 | 5620.119 | +6.99% | +3.05% | +9.83% |
| Fixed Richards ×10 / 固定 Richards | 1476.607 | 1316.252 | 1147.185 | +10.86% | +12.84% | +22.31% |
| Original Richards harness / 原始 Richards | 6170.316 | 5215.159 | 5100.399 | +15.48% | +2.20% | +17.34% |
| 100 chains ×2000 nodes / 链式释放 | 362.614 | 348.052 | 326.934 | +4.02% | +6.07% | +9.84% |

Sample ranges, Before → Runtime → Final / 样本范围：

- Integer loop ×10m / 整数空循环: 1501.542–1547.824 → 1459.481–1561.700 → 1532.235–1583.824 ms.
- Property workload ×20m / 属性负载: 6149.758–6341.840 → 5757.578–6067.637 → 5386.392–5706.979 ms.
- Fixed Richards ×10 / 固定 Richards: 1466.917–1499.312 → 1306.216–1350.086 → 1135.472–1162.984 ms.
- Original Richards harness / 原始 Richards: 6134.521–6208.518 → 5184.345–5317.490 → 5050.296–5122.734 ms.
- 100 chains ×2000 nodes / 链式释放: 356.924–371.500 → 345.158–349.714 → 321.065–339.748 ms.

属性、两种 Richards 和链式负载的三个版本范围均不重叠。空循环最终中位耗时略升 0.45%，范围重叠，不作改善结论，也不排除小幅回退。属性负载包含循环和累加，不是单独的属性查找性能。

All three ranges are disjoint for property, both Richards workloads and chain destruction. Final empty-loop median is 0.45% slower with overlapping ranges: no gain is established and a small regression remains possible. Property results include loop and accumulation work, not isolated lookup cost.

Original Richards median harness score / 原始 harness 中位分数：24.10 → 27.20 → 31.25. Harness scores and whole-process elapsed time are separate metrics. / 分数与完整进程耗时是不同指标。

## Implementation and constraints / 实现与约束

- `DeferredOperations` owns the queue plus `pending` and `draining` flags. Queue mutation maintains the pending bit. An RAII guard resets the drainer after errors/unwind; acquire Runtime state before dequeue to leave blocked work untouched. / 队列统一维护状态，借用失败不出队，guard 在错误/展开后复位。
- Inline idle checks skip queue borrowing. Blocked releases enqueue and return without immediately retrying the known blocked borrow. A shared `apply_deferred_operation` handles release/restoration in immediate, deferred and teardown paths. / 空路径不借用队列，失败后不立即重试，三种执行路径共享操作逻辑。
- `Heap::release_reference` reuses existing validation, decrement and iterative zero-queue draining, returning `Option<HeapCleanup>`. Existing typed cleanup-returning entry points remain wrappers; Runtime applies only present cleanup. Two now-unused production wrappers carry scoped lint annotations. / 可选清理入口复用原有逻辑；保留 typed 兼容入口，仅对无生产调用的两个 wrapper 局部标注。
- Keep all operation entry/exit safe points, front-priority frame/backtrace restoration, cycle GC, weak references and finalization behavior. No unsafe, production counters, script-end batching or alternative reference-count implementation. / 保留检查点、恢复优先级、循环 GC 及弱引用/finalization；不推迟到脚本结束。
- Cost: two `Cell<bool>` flags per Runtime and one guarded slow path, no separate allocation. No runtime memory-size or phase-timing probe was run. / 每 Runtime 增加两个布尔状态，无额外独立分配；本轮未测内存尺寸/阶段时间。

## Diagnostic evidence / 诊断证据

Independent counter builds instrument fresh copies of the three measured revisions; 4 fixed workloads ×3 repetitions ×3 versions = 36 runs. Counts are identical across repeats. Object retains/releases, deferred operations actually applied, blocked releases, nonempty zero-queue entries, processed nodes and nonempty Runtime cleanup work are unchanged across all three versions for every workload. / 独立计数构建 36 次运行，同负载重复计数一致；真实引用操作和非空清理工作量不变。

| Fixed Richards counter / 计数 | Before | Runtime | Final |
| --- | ---: | ---: | ---: |
| object_handle_retains | 13,074,740 | 13,074,740 | 13,074,740 |
| object_handle_releases | 13,075,283 | 13,075,283 | 13,075,283 |
| deferred_check_requests | 35,077,874 | 35,077,850 | 35,077,850 |
| deferred_slow_entries | 35,077,874 | 1 | 1 |
| deferred_operations_applied | 24 | 24 | 24 |
| release_state_borrow_blocked | 24 | 24 | 24 |
| zero_queue_drain_entries | 15,359,417 | 15,359,417 | 517,456 |
| zero_queue_nonempty_entries | 1,947 | 1,947 | 1,947 |
| zero_queue_nodes_processed | 1,948 | 1,948 | 1,948 |
| cleanup_apply_calls | 15,359,417 | 15,359,417 | 520,093 |
| cleanup_apply_nonempty_runtime_work | 1,944 | 1,944 | 1,944 |
| deferred_empty_at_check | 35,077,849 | 35,077,849 | 35,077,849 |
| deferred_state_borrow_blocked | 24 | 0 | 0 |

For fixed Richards, zero-queue drain entries fall from 15,359,417 to 517,456 and cleanup application calls from 15,359,417 to 520,093; nonempty drains remain 1,947 and processed nodes remain 1,948. Other cleanup callers still contribute empty work; the patch does not eliminate every empty cleanup in the engine. / 固定 Richards 的零队列入口降至 517,456 次，应用清理降至 520,093 次，非空入口与销毁节点不变；其他调用路径仍有空清理，并非全引擎清零。

Counters are process-global cumulative snapshots at `RuntimeInner::drop` entry. The last snapshot excludes that final Runtime teardown; earlier snapshots are not added together. Nonempty Runtime cleanup means atom or finalized-shape payloads, not every finalized node. Production timings never use these instrumented builds. / 计数在 Runtime 析构入口记录，最后快照不含最后 Runtime 的析构本身；非空 Runtime payload 与实际销毁节点是不同口径，不用探针构建作性能结论。

Uninstrumented CPU sampling: `perf record -e cycles:u -F 499`, no call graph, 3 repetitions ×2 workloads ×3 versions = 18 runs; all report zero lost samples. The table aggregates sample periods; zero means no samples attributed to that named symbol, not zero execution. / CPU 采样使用原始生产二进制；表格是 period 占比，不是逻辑调用次数，内联可改变符号归属。

| Workload / version | Samples | deferred drain % | zero queue drain % | release_object_handle % | shared apply operation % |
| --- | ---: | ---: | ---: | ---: | ---: |
| prop_fixed / before | 9326 | 2.01 | 2.00 | 7.65 | 0.00 |
| prop_fixed / runtime | 8826 | 0.00 | 1.73 | 0.00 | 6.94 |
| prop_fixed / after | 8541 | 0.00 | 0.00 | 0.00 | 1.61 |
| richards_fixed / before | 2194 | 6.10 | 4.88 | 10.93 | 0.00 |
| richards_fixed / runtime | 1994 | 0.00 | 5.91 | 0.00 | 15.89 |
| richards_fixed / after | 1704 | 0.00 | 0.18 | 0.00 | 2.20 |

The counter evidence is the direct evidence for removed empty work. Symbol percentages are supporting attribution only: inlining moves cost into callers and smaller total work changes denominators. Raw symbols, periods, sample counts and stderr are preserved. / 空工作减少由独立计数直接验证；CPU 符号占比仅作辅助，不能把符号消失当作工作消失。

## Validation / 验证

- Runtime stage: **3008 Rust tests passed**. Final logic: **3010 passed**, zero failed, one existing ignored, 13 test binaries, Rust 1.88 workspace/all-targets/profiling. / 两阶段测试分别通过 3008/3010 项。
- Eight new tests cover queue priority, work arriving during draining, guard reset after unwind/error, idle and blocked borrows, immediate 10000-node cascading destruction, Runtime teardown, optional live-reference cleanup and draining previously queued nodes on a nonzero release. Existing frame/backtrace, weak-reference and cycle tests also pass. / 八项新测试覆盖队列、借用、立即销毁、析构及整个零队列判定。
- Format, source layout (421 Rust files /65 READMEs), Rust-only and strict production Clippy pass for profiling/default/test262-host. Binary-object boundary passed; **all 690 isolation canaries rejected**. / 基础检查与全部边界 canary 通过。
- Fresh full Test262 before/after: **102037 outcome rows identical**, zero changed outcomes/new failures. Counts: 79982 pass, 7 parse failure, 43 runtime failure, 3530 unsupported, 18475 skipped. Only the first source-fingerprint header differs. Both frozen full-file gates exit 1; no baseline was updated. / 完整 Test262 结果逐行一致；冻结校验因源码指纹头变化失败，未更新基线。

## Reproduction and archive / 复现与原始包

Measured commits / 测量版本：Before `06c92290ee6b5cb48ddd11c283c585242d18b56e`; Runtime `12d3b7a33f31895680d8e5012915d209f4daaca6`; Final `cb5ab8246bb1daebf41366275a1df27ef678fe97`. Validation revision adds only comments/lint attributes: `730993b44003645cc09f2f5bf56427323adbcb77`. Later documentation does not change runtime logic. / 最后验证版本只增加注释与 lint 标注，后续文档不改变逻辑。

Heavy work ran serially through Herdr, eric-83am / PocketLab (AMD Ryzen 7 7840HS, Linux 6.18.44-1-lts). Release Rust 1.94.1; profiling compiled but inactive; no forced frame pointers. / 重型工作均在远端 Herdr 串行执行。

```sh
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
CARGO_TARGET_DIR=target/cpu-profile \
QUICKJS_OXIDE_BUILD_COMMIT=$(git rev-parse HEAD) \
cargo build --locked --release -p quickjs-oxide-cli \
  --no-default-features --features profiling
```

External workloads are unchanged from the earlier investigation pinned to `ahaoboy/js-engine-benchmark@2034d98fc8c5f8044e186267593f5d5ea5232caf`; the authored chain case builds/releases 100 chains of 2000 nodes. All source/binary hashes and the authored source are recorded. Whole-process timing uses `time.perf_counter_ns()` and a 180-second timeout. CPU frequency and complete system isolation are not controlled. / 复用固定外部负载，额外自写链式案例；完整进程计时，未锁频或完全隔离系统。

[Machine-readable evidence / 机器可读证据](release-cleanup-fast-path.json) includes all 90 timing runs, 18 CPU runs, 36 counter runs, commands, hashes, test receipts and environment. Outcome-row SHA-256: `fa99d3349bb4b61f30ba57d7c7f275df64f19691edefc50b96b53887339fa8c3`.

Raw archive / 原始包: `quickjs-oxide-release-cleanup-pocketlab-2026-09-09.tar.gz`, **63169351 bytes**, SHA-256 **`7656864382e9dac5116f6f6c616a0b355fdfca79b15f473c3c2457281195d4ea`**. All 437 manifest members verified remotely; downloaded archive hash matches. Includes three exact production executables, three counter executables and patches, perf data, raw measurements, full Test262 vectors, logs and reproduction scripts. Stored in local/remote workspace `target/`, not attached to GitHub; external benchmark sources excluded. / 清单逐项验证，本地下载哈希一致；原始包保留于工作区，包含可执行文件与原始证据，不包含外部 benchmark 源码。
