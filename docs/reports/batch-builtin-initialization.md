# 批量内建方法初始化 / Batch lazy builtin initialization

Context creation falls **23.16%**, from **2.286008 ms to 1.756603 ms**, in seven interleaved process triplets. This is an initialization improvement, with a measured fixed property-read regression: **2.37% slower** in a separate six-pair, CPU-2-pinned follow-up. Its cause is unresolved; this report does not claim a general interpreter speedup.

七轮交错测量中，Context 创建耗时降低 **23.16%**。固定属性读取负载在额外固定核心复测中仍有 **2.37%** 的耗时回退，原因尚未定位。初始化收益与长脚本吞吐分别报告，不以单个指标代表整体性能。

## Change and publication boundaries / 实现与发布边界

`NativeBuiltinProperty` keeps a named method's native target, name, length, minimum readable argument count and flags together. The internal batch entry consumes descriptors and interns owning keys before borrowing Runtime state, validates the receiver/live realm/entire table, copies existing entries and slots once, appends in order and calls the existing `replace_layout` once. It preserves lazy `AutoInitProperty` slots and the existing shape, Heap and Atom ownership mechanisms. / 描述表集中保存方法元数据；键的根引用保持至安装结束，一次构造最终布局并复用现有发布机制。

| Batch / 批次 | Methods / 方法数 | Layout calls removed / 减少布局调用 |
| --- | --- | --- |
| Date.prototype, after GMT alias | 42 | 41 |
| Array.prototype methods | 38 | 37 |
| Array constructor methods | 3 | 2 |
| TypedArray.prototype at/with | 2 | 1 |
| TypedArray.prototype set through includes | 28 | 27 |
| TypedArray base constructor from/of | 2 | 1 |
| Uint8Array.prototype codecs | 4 | 3 |
| Uint8Array constructor codecs | 2 | 1 |

**121 methods in 8 batches replace 121 single installations, removing 113 intermediate layout replacements per Context.** Getter, Symbol and alias boundaries remain explicit. Date's `toUTCString` is still materialized once and shared by `toGMTString`; Array/TypedArray iterator aliases and their shared `toString` retain identity. / 共 121 项方法改为 8 次批量安装，减少 113 次中间布局替换；getter、Symbol 和别名仍按原有顺序处理。

Empty batches return immediately. Nonempty batches reject foreign-runtime receivers, nonextensible receivers or unsupported payload kinds, stale realms, existing/duplicate keys, index keys and accessor flags before layout publication. All production tables use extensible ordinary, Array-prototype or native-function receivers and named data methods. Preparation/retain failures are checked without partial publication. Errors after publication retain the existing `replace_layout` cleanup contract; this change does not promise rollback for every possible invariant error. / 准备失败不发布部分属性；发布后的清理错误沿用原有契约，不新增 Heap 事务框架或少量引用保留快路径。

## Lifecycle and short processes / 生命周期与短进程

All values below are **median [minimum–maximum] in ms**. Lifecycle entries summarize seven **process means**, each over 1000 fresh Runtime/Context pairs; they are not cold-start percentiles or repeated Contexts on one shared Runtime. The CLI's initial Context and diagnostic output are outside those phase intervals. Context-handle drop is distinct from later Runtime teardown. / 生命周期以每进程 1000 次均值为样本，再汇总七轮；Context 句柄释放不等于整个 realm 已回收。

| Phase | Before | Date pilot | Expanded | Time reduction / 耗时减少 |
| --- | --- | --- | --- | --- |
| runtime_create | 0.005719 [0.005628–0.005844] | 0.005738 [0.005584–0.005883] | 0.005694 [0.005555–0.005798] | +0.44% |
| context_create | 2.286 [2.277–2.316] | 2.042 [2.03–2.061] | 1.757 [1.74–1.781] | +23.16% |
| context_drop | 4.345e-05 [3.908e-05–5.663e-05] | 4.616e-05 [3.701e-05–5.678e-05] | 4.024e-05 [3.886e-05–6.219e-05] | +7.39% |
| runtime_drop | 0.1886 [0.1875–0.1921] | 0.1875 [0.184–0.1907] | 0.19 [0.1871–0.1929] | -0.74% |

| Case | Before | Date pilot | Expanded | Time reduction |
| --- | --- | --- | --- | --- |
| CLI -q, 20 processes/version | 4.833 [4.266–6.005] | 4.562 [4.097–5.21] | 4.071 [3.68–4.938] | +15.77% |

Single-process ranges overlap; the startup median shift is workload/host-specific. An earlier Date-only decision run also passed: 7 before/pilot lifecycle pairs reduced Context creation from 2.278237 to 2.031024 ms, and independent counts confirmed 41 fewer layout/shape calls before expansion proceeded. / 单次启动范围有重叠，不作跨平台保证；Date 试点先独立验证收益与计数，再扩展。

## Work removed and phase attribution / 减少的工作与阶段归因

Count-only diagnostics are exact across three repetitions. Divide the process counts by **1001 Contexts**, including the CLI's initial Context. They are separate from ordinary timing binaries. / 三次计数完全一致；包含 CLI 初始 Context，不混入无探针计时。

| Per Context / 每个 Context | Before | Date pilot | Expanded |
| --- | --- | --- | --- |
| replace_layout_calls | 778 | 737 | 665 |
| replace_layout_entry_visits | 6419 | 5353 | 4048 |
| shape_lookup_calls | 943 | 902 | 830 |
| shape_fingerprint_entries | 6421 | 5355 | 4050 |
| shape_cache_hits | 158 | 158 | 158 |
| retain_edges_calls | 1899 | 1817 | 1673 |
| retain_edges_total | 7656 | 6508 | 5108 |

`replace_layout_entry_visits` and slot visits both fall from 6419 to 4048; fingerprint entries fall from 6421 to 4050. Shape cache hits remain 158. The change removes repeated work instead of introducing a new shape cache or reference-count fast path. / 属性槽和 shape fingerprint 的重复遍历减少，缓存命中次数不变。

Coarse inclusive initialization timers, ms per Context / 粗粒度初始化计时：

| Stage | Before | Date pilot | Expanded |
| --- | --- | --- | --- |
| date_intrinsic | 0.2853 | 0.07381 | 0.07933 |
| array_intrinsics | 0.2113 | 0.2098 | 0.06293 |
| typed_array_intrinsics | 0.3378 | 0.3387 | 0.2238 |

## Complete existing benchmark matrix / 现有完整 benchmark 矩阵

The V8 matrix covers all eight isolated suites from the external `run.js`. Each version is screened once with a 90-second timeout; every case with valid scores on **all three** versions receives two more runs each. Failed/timeout/incomplete cases remain visible and get no numeric ratio. This policy was recorded before screening. All five default microbench prefixes receive three runs each. / 覆盖全部八项 V8 独立套件和现有五项默认 microbench；先筛查，再重复有效用例，超时不折算成绩。这里不宣称覆盖 QuickJS microbench 文件中的所有函数，也不把可完成用例的分数当作完整套件总分。

V8 scores are higher-is-better; cells show median [range] and successful/attempted runs. / V8 分数越高越好：

| V8 case | Before | Date pilot | Expanded | Score change / 分数变化 |
| --- | --- | --- | --- | --- |
| richards | 31.2 [30.7–31.2] (3/3) | 30.4 [30.4–30.6] (3/3) | 30.8 [30.5–31.1] (3/3) | -1.28% |
| deltablue | 38.3 [38.2–38.3] (3/3) | 37.2 [37.1–37.5] (3/3) | 38.6 [38.5–38.6] (3/3) | +0.78% |
| crypto | timeout (0/1) | timeout (0/1) | timeout (0/1) | not comparable / 不比较 |
| raytrace | 46.4 [46.4–46.5] (3/3) | 46.5 [46.3–46.6] (3/3) | 46.8 [46.8–47.1] (3/3) | +0.86% |
| earley-boyer | timeout (0/1) | timeout (0/1) | timeout (0/1) | not comparable / 不比较 |
| regexp | timeout (0/1) | timeout (0/1) | timeout (0/1) | not comparable / 不比较 |
| splay | 195 [189–195] (3/3) | 195 [194–195] (3/3) | 196 [196–198] (3/3) | +0.51% |
| navier-stokes | timeout (0/1) | timeout (0/1) | timeout (0/1) | not comparable / 不比较 |

Microbench ns/op is lower-is-better. Every run verifies the identical Date.now clock marker. These are the upstream harness's minimum-of-many scores and a millisecond clock, not latency distributions of individual operations. / microbench 使用原始最小值采样与毫秒时钟，不代表逐操作延迟分布：

| Microbench | Before | Date pilot | Expanded | Time reduction / 耗时减少 |
| --- | --- | --- | --- | --- |
| empty_loop | 200 [200–200] (3/3) | 200 [200–200] (3/3) | 200 [200–200] (3/3) | +0.00% |
| prop_read | 500 [500–500] (3/3) | 500 [500–500] (3/3) | 500 [500–500] (3/3) | +0.00% |
| array_read | 400 [400–400] (3/3) | 400 [400–400] (3/3) | 400 [400–400] (3/3) | +0.00% |
| func_call | 500 [500–500] (3/3) | 500 [500–500] (3/3) | 500 [500–500] (3/3) | +0.00% |
| int_arith | 400 [400–400] (3/3) | 400 [400–400] (3/3) | 400 [400–400] (3/3) | +0.00% |

The property-read row reports N=1000, and all five microbench rows have identical values across versions. This coarse result does not rule out the regression observed by the longer fixed property workload. / 属性读取行报告 N=1000，五项在三版本间的数值均相同；这种粗粒度结果不能排除较长固定属性负载检出的回退。

## Fixed workloads and regression check / 固定工作量与回退核查

Six process runs per version use all six version-order permutations. `short` evaluates `42`; `richards_fixed` runs ten direct `runRichards()` calls with original assertions; `prop_fixed` uses the pinned `prop_read(5000000)` body, four reads per iteration, and verifies outputs 20000000 and 50000000. These are fixed workloads, not V8 adaptive scores. / 六种版本顺序均运行一次，保留所有样本。

| Fixed case, ms | Before | Date pilot | Expanded | Time reduction / 耗时减少 |
| --- | --- | --- | --- | --- |
| short | 5.271 [4.676–5.981] | 5.245 [4.637–7.469] | 4.264 [4.172–4.778] | +19.11% |
| richards_fixed | 1154 [1146–1172] | 1180 [1175–1209] | 1157 [1153–1175] | -0.31% |
| prop_fixed | 5673 [5592–5762] | 5953 [5824–5992] | 5909 [5846–6055] | -4.17% |

The ordinary property case is 4.17% slower, with disjoint ranges. A follow-up pins the **same ordinary binaries** to CPU 2 and collects hardware counters in six alternating pairs; every pair is slower after the change. / 普通属性固定负载的范围不重叠，固定核心追加六对测量仍均显示回退。

| Pinned follow-up metric | Before | Expanded |
| --- | --- | --- |
| wall ms | 5812 [5630–5933] | 5950 [5772–6036] |
| instructions | 6.791e+10 | 6.79e+10 |
| cycles | 2.623e+10 | 2.686e+10 |
| IPC | 2.589 | 2.528 |
| effective GHz | 4.533 | 4.527 |
| branch misses | 5.094e+06 | 5.14e+06 |
| cache misses | 8.564e+04 | 8.483e+04 |

The retired instruction count is nearly unchanged, while cycles increase and IPC falls. CPU migrations are zero. This does not establish the cause of the regression; neither compiler/code placement nor data-cache effects have been proven. Do not interpret the initialization gain as a property-read throughput gain. One aborted collector attempt caused by a `task-clock:u` label suffix is retained separately and excluded from the complete six-pair series. / 指令数基本不变，周期增加、IPC 降低；尚未证明具体原因。首个采集器解析中止记录单独保留，六对有效复测完整纳入分析。

## Short, medium and long phase timing / 短、中、长负载阶段计时

Independent coarse-timer binaries, median of three runs, ms. These binaries have different instrumentation/code layout and are for phase attribution, not substitutes for ordinary-binary throughput results. / 三次独立粗粒度计时仅用于归因，不替代普通可执行文件的吞吐测量。

| Case | Version | Context | Compile | Execute | Whole process |
| --- | --- | --- | --- | --- | --- |
| short | before | 2.625 | 0.08782 | 0.02908 | 5.281 |
| short | pilot | 2.723 | 0.107 | 0.03546 | 5.444 |
| short | after | 2.162 | 0.07454 | 0.02109 | 4.655 |
| richards_fixed | before | 2.618 | 13.19 | 1159 | 1178 |
| richards_fixed | pilot | 2.433 | 12.97 | 1118 | 1138 |
| richards_fixed | after | 2.133 | 13.15 | 1173 | 1191 |
| prop_fixed | before | 3.392 | 0.2184 | 5549 | 5556 |
| prop_fixed | pilot | 2.408 | 0.244 | 5607 | 5613 |
| prop_fixed | after | 2.618 | 0.2189 | 5826 | 5832 |

## CPU sampling and probe effects / CPU 采样与探针影响

All samples use `cycles:u`, 499 Hz, frame-pointer builds, three repetitions per case/version. The matching executable, raw perf data, processed self/inclusive reports, stacks, periods and libc symbols are archived. Percentages are period-weighted; unknown samples remain in denominators. / 使用匹配的可执行文件和 libc 符号，保留原始采样与解析结果。

| Case | Version | Samples | Lost | Unknown leaf % | qjs::main in stack % | Max depth | Depth ≥127 % |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lifecycle | before | 3787 | 0 | 0.0662 | 99.94 | 25 | 0 |
| lifecycle | pilot | 3391 | 0 | 0.0352 | 99.96 | 26 | 0 |
| lifecycle | after | 3505 | 0 | 0.001856 | 100 | 27 | 0 |
| richards_fixed | before | 1796 | 0 | 0.00338 | 99.99 | 79 | 0 |
| richards_fixed | pilot | 1782 | 0 | 0.00941 | 99.99 | 125 | 0 |
| richards_fixed | after | 1779 | 0 | 0.1192 | 99.88 | 76 | 0 |
| prop_fixed | before | 8761 | 0 | 0.01254 | 100 | 31 | 0 |
| prop_fixed | pilot | 8621 | 0 | 0.01498 | 99.99 | 31 | 0 |
| prop_fixed | after | 8793 | 0 | 0.02325 | 100 | 31 | 0 |

Selected inclusive frame shares **within Context creation** / Context 内选定调用帧占比：

| Frame category, % | Before | Date pilot | Expanded |
| --- | --- | --- | --- |
| replace_layout | 65.31 | 62.11 | 63.51 |
| shape_lookup | 25.03 | 24.43 | 24.74 |
| retain_edges | 10.31 | 10.72 | 11.83 |
| batch_install | 0 | 2 | 1.894 |

Inlining and reduced total work change symbol attribution and denominators. The unchanged high layout share is not evidence that no work was removed: exact call/entry counts and ordinary lifecycle timing establish that reduction. Stage initializer symbols may be inlined away. / 符号占比不能直接当作绝对耗时；工作减少由独立计数和无探针计时验证。

Measured perturbation, not a universal profiler overhead / 本次探针扰动：

| Case | Version | FP plain vs ordinary | perf vs FP plain |
| --- | --- | --- | --- |
| lifecycle | before | +1.66% | +3.23% |
| lifecycle | pilot | +1.14% | +2.77% |
| lifecycle | after | +17.56% | +3.18% |
| richards_fixed | before | +0.85% | +6.13% |
| richards_fixed | pilot | -0.88% | +4.58% |
| richards_fixed | after | -0.32% | +7.41% |
| prop_fixed | before | +0.41% | +3.42% |
| prop_fixed | pilot | -1.58% | -0.63% |
| prop_fixed | after | -0.96% | +1.26% |

Diagnostic-mode whole-process overhead vs the same diagnostic binary with mode 0 / 相对同一诊断程序关闭探针：

| Version | Mode 1 counts | Mode 2 detailed | Mode 3 coarse |
| --- | --- | --- | --- |
| before | -0.13% | +6.51% | +0.21% |
| pilot | +0.93% | +6.07% | +0.92% |
| after | +0.07% | +5.11% | +0.58% |

## Correctness and gates / 正确性与检查

- Rust 1.88 workspace/all-targets/profiling: **3015 tests passed for the pilot; 3018 for the expanded version**, zero failures, one existing ignored test, 13 test binaries. Eight added tests cover descriptor order/flags/name/length/minimum arguments, laziness and identity, duplicate/index/accessor/nonextensible/exotic/domain rejection, expired realms, forced retain overflow without leaked atoms/shapes/realm edges, aliases and separate Context functions sharing a shape. / 八项新增测试覆盖元数据、惰性身份、失败回滚与 realm 独立性。
- Format, source layout (**423 Rust files / 65 READMEs**), Rust-only and strict production Clippy pass for profiling/default/test262-host. **All 690 binary-object isolation canaries were rejected.** / 基础检查与全部边界隔离测试通过。
- Fresh full Test262 runs have **102037 identical result rows**, zero changed results/new failures. Both retain 79982 passes, 7 parse failures, 43 runtime failures, 3530 unsupported and 18475 skipped. TSV and JSONL each differ only on line 1, the source fingerprint. **Both frozen full-file checksum gates exit 1**; no frozen baseline was changed. / 完整结果逐条一致；冻结完整文件校验仍因源码指纹头变化失败，未更新基线。
- Full Date/Array/TypedArray/Uint8Array constructor and prototype property snapshots are byte-identical across all three ordinary executables, including order, flags, callable metadata and aliases. / 三版本属性快照逐字节一致。

## Versions, environment and archive / 版本、环境与原始包

Before `8eb2a1ecb95428c02e108bc558852b17518713b1`; Date pilot `a4ae546b1459715174599a176a3a6aaa6c425abe`; expanded `0274697330e4cabfd71561afcd096571f928947b`. Later report commits do not change runtime logic. / 后续文档提交不改变运行逻辑。

All heavy work ran serially via Herdr on PocketLab / eric-83am, AMD Ryzen 7 7840HS. Release compiler Rust 1.94.1, debug info 1, strip none, profiling compiled but inactive for ordinary timings; only CPU-sampling binaries force frame pointers. The baseline is the exact preserved PR #4 executable, with hash/build provenance, run interleaved with freshly built pilot/expanded binaries. CPU frequency and whole-system isolation were not controlled; only the hardware-counter follow-up pins CPU affinity. / 普通计时不启用探针；基线使用保留的精确可执行文件并核验构建来源，未锁频或完全隔离系统。

```sh
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
QUICKJS_OXIDE_BUILD_COMMIT=$(git rev-parse HEAD) \
cargo build --locked --release -p quickjs-oxide-cli \
  --no-default-features --features profiling
```

External V8 source is pinned by commit and file hashes in the matrix metadata; pinned QuickJS microbench source and shared clock-prefix hashes are recorded separately. Fixed-workload preparation scripts preserve original bodies and assertions. Workload sources are not vendored into this repository or raw archive. / 外部来源、生成方式和哈希均记录，未纳入外部源码。

[Machine-readable evidence / 机器可读证据](batch-builtin-initialization.json) records environment, all matrix samples, fixed-workload distributions, phase/counter summaries, CPU quality, hardware counters and validation receipts. Complete 1000-element lifecycle vectors and unabridged stacks are in the archive. / 完整生命周期向量与调用栈保留在原始包。

Raw archive / 原始包: `quickjs-oxide-batch-builtins-pocketlab-2026-09-10.tar.gz`, **105056302 bytes**, SHA-256 **`276ff233370e2984347dc69ea75af648e2015f36e96cf33c60ad9fcc6f96586d`**. All **1149** manifest members verified remotely; downloaded archive hash matches. Includes nine exact ordinary/FP/diagnostic executables, diagnostic patches, perf data, raw outputs, full Test262 vectors, logs and reproduction scripts. Stored in local/remote workspace `target/`, not attached to GitHub. / 原始包保留于本地及远端工作区，逐项清单及下载哈希已核验。
