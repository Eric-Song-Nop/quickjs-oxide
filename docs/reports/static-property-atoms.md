# Static property Atom reuse — 2026-09-09

静态名称现在在字节码发布时链接成 Runtime 内的 Atom，执行时按常量索引直接复用。缓存的是名称身份，不是属性值或对象中的位置，因此 getter、Proxy 和原型变化仍走正常查询路径。本次基于已完成的数组提前判断优化，关联 [issue #7](https://github.com/pocket-stack/quickjs-oxide/issues/7)。

Static names are now linked to runtime-local Atoms at bytecode publication and reused by constant index during execution. This caches name identity, not property values or object offsets; getters, Proxies and prototype changes retain normal lookup behavior. This change builds on the array early-check optimization and addresses [issue #7](https://github.com/pocket-stack/quickjs-oxide/issues/7).

## Execution results / 执行结果

Five independent processes per version/workload, alternating order, diagnostics disabled. / 每版本每工作负载五次独立进程，交替顺序，关闭诊断。

| Workload / 工作量 | Before median ms / 修改前 | After median ms / 修改后 | Time reduction / 耗时下降 |
| --- | ---: | ---: | ---: |
| Empty loop ×10 million / 空循环 1,000 万次 | 1945.535 | 1890.326 | 2.84% |
| Property reads ×20 million / 属性读取 2,000 万次 | 10813.374 | 7240.065 | **33.05%** |
| Fixed Richards ×10 / 固定 Richards 十次 | 2117.465 | 1477.229 | **30.24%** |
| Original Richards harness / 原始 Richards harness | 8460.681 | 6163.680 | **27.15%** |

原始 Richards 中位分数为 **16.7 → 24.1（+44.31%，越高越好）**。分数与完整进程耗时是不同指标，不通过其中一个反推另一个。以上均包含启动、编译、执行和收尾；原始 harness 还包含其 warmup 和计时逻辑。

Original Richards median score: **16.7 → 24.1 (+44.31%, higher is better)**. Harness scores and whole-process wall times are distinct metrics; neither is derived from the other. Process times include startup, compilation, execution and teardown, plus warmup/timing logic in the original harness.

40 次计时运行全部成功且 stderr 为空；固定工作量输出严格校验，保留 Richards 内部断言；原始 harness 的 Richards 与总分两行必须匹配。属性读取五次范围为修改前 **10699.269–11261.891 ms**、修改后 **7198.157–7362.440 ms**。空循环前后范围重叠（1854.449–1989.332 / 1864.893–1934.948 ms），其小幅变化不能当作数值循环优化的证据。

All 40 timed runs succeeded with empty stderr, exact fixed-workload output checks and retained Richards assertions. Original harness Richards/aggregate score lines had to match. Property-read ranges were **10699.269–11261.891 ms before**, **7198.157–7362.440 ms after**. Empty-loop ranges overlap (1854.449–1989.332 / 1864.893–1934.948 ms); the small change does not establish a numeric-loop improvement.

## Compilation and storage / 编译与存储

另用相同源码的外部 Rust 探针，分别链接前后引擎库。每个工作负载/版本运行三个进程，每进程创建 20 个独立 Runtime/Context；前两次标为 warmup，剩余共 54 个样本计算中位数。`Instant` 包围 `Context::compile`，包含解析、编译和发布，**不是单独的发布时间**。内存快照在保留编译结果、尚未执行脚本时采集，位于计时范围之外。

A separate identical Rust probe links each engine library. Three processes per workload/version create 20 fresh Runtime/Context instances each; the first two iterations are warmups, leaving 54 samples for medians. `Instant` surrounds `Context::compile`, including parsing, compilation and publication, **not publication alone**. Memory snapshots occur outside the timer, with compiled bytecode retained and the script unexecuted.

| Workload / 工作量 | Compile + publish before ms / 修改前 | After ms / 修改后 | Linked-name entries / 链接名称条目 | Map bytes / 映射字节 |
| --- | ---: | ---: | ---: | ---: |
| Empty loop | 0.055291 | 0.056944 | 1 | 32 |
| Property reads | 0.094090 | 0.101119 | 10 | 176 |
| Fixed Richards ×10 | 12.114134 | 12.079653 | 359 | 6736 |
| Original Richards | 12.696623 | 12.585997 | 364 | 6912 |

映射以常量索引寻址，只延伸到最大静态名称索引；字节数包含中间空槽。这里 Rust 的安全 Atom 句柄带有表域和代数，共 16 字节，并非 QuickJS 的裸 32 位编号。每个链接条目还在既有 `auxiliary_atoms` 中增加一个 16 字节引用记录：原始 Richards 额外为 **5824 字节**，映射加这部分数组载荷共 **12736 字节**。不含 Rc/allocator 头、Atom 表和字符串存储、VM 帧变化，不能当作总内存增量。

The constant-indexed map ends at the highest static-name index; bytes include unused intermediate slots. Rust's checked Atom includes table-domain and generation fields and occupies 16 bytes here, unlike QuickJS's raw 32-bit identifier. Each linked entry also adds a 16-byte reference record to existing `auxiliary_atoms`: **5824 extra bytes** for original Richards, totaling **12736 bytes** of map/reference-array payload. This excludes Rc/allocator headers, Atom-table/string storage and VM-frame changes; it is not total memory overhead.

原始 Richards 的编译快照中，活跃表内 Atom 数为 **471 → 518**，字节码节点仍为 64，arena 已用内联字节仍为 308264。属性读取为 **384 → 389** 个表内 Atom。所有探针迭代释放编译结果后，都验证字节码节点为零；新版本还验证映射条目及字节为零。该检查不声称 allocator 或 Atom 表容量归还系统。

Original Richards compilation snapshots contain **471 → 518** live table-backed Atoms, with 64 bytecode nodes and 308264 used arena inline bytes in both versions. Property reads contain **384 → 389** table-backed Atoms. Every probe iteration verifies zero bytecode nodes after dropping the compiled result; the new version also verifies zero map entries/bytes. This does not claim allocator or Atom-table capacity is returned to the OS.

编译时间的小幅起伏不能解释为编译器优化；属性读取的编译加发布中位数增加约 7 µs。全部 480 个探针迭代均保留（432 个非 warmup），包括 Context 时间和每次快照数值。

Small compilation-time fluctuations are not evidence of a compiler optimization; the property-read compile/publication median increases by about 7 µs. All 480 probe iterations are retained (432 non-warmup), including Context times and every snapshot value.

## Implementation and validation / 实现与验证

- `Instruction::constant_property_key_index` identifies static names, including eval/dynamic-binding/reference operations and unreachable instructions. Publication interns each referenced constant index once. / 统一识别静态名称，包括 eval、动态绑定、引用操作及不可达指令；发布时每个被引用索引只 intern 一次。
- `FunctionBytecodeData` owns an optional shared map through its metadata; Atom references use the existing `auxiliary_atoms` transaction and GC cleanup. Functions without static names allocate no map. / 字节码保存可选共享映射，Atom 引用接入现有事务、回滚和 GC 清理，无静态名称的函数不分配映射。
- Normal calls and resumed activations carry the same map. Production `constant_property_key` performs a lookup and existing `PropertyKey` reference promotion, with no string-intern fallback. Only synthetic, unpublished unit-test hosts retain a `cfg(test)` fallback. / 普通调用和恢复帧复用映射；生产路径直接查表并提升引用，无字符串 intern 回退；只有未发布的合成单元测试 host 保留测试专用回退。
- Original String constants and serialized representation remain unchanged; binary reading reaches the same runtime publication boundary. The heap checks mapping coverage, string kinds, bounds and owned-reference multiplicity. / 保留字符串常量和序列化形式，二进制读取经过同一发布边界；堆检查覆盖、字符串类型、边界与引用所有权数量。
- **2998 Rust tests passed, 0 failed, one existing ignored**, across 13 binaries with Rust 1.88 and profiling enabled. Tests cover release/rollback, runtime isolation, numeric spellings/lone surrogates, getters/Proxies/prototype changes, resumed generators and malformed mappings. / 13 个测试二进制共 2998 项通过，0 失败，1 项原有忽略，覆盖释放、回滚、Runtime 隔离、特殊字符串、动态查询行为和损坏映射。
- Formatting, source layout, Rust-only checks and strict production Clippy passed. Clippy covered profiling, default workspace and `test262-host` library/binary configurations. / 格式、源码布局、Rust-only 和三种配置的生产目标 strict Clippy 通过。
- Binary-object production boundary passed; **all 690 isolation canaries were rejected** on the remote host. Its final log hash is recorded in the JSON and retained separately from the earlier measurement archive. / 远端字节码生产边界检查通过，690 个隔离 canary 全部被拒绝；最终日志哈希见 JSON，日志单独保留于测量归档之外。
- Full Test262 ran separately on both revisions: **all 102037 outcome rows identical, zero changed rows/new failures**. Both retained 79982 passes, 7 parse failures, 43 runtime failures, 3530 unsupported and 18475 skipped. The frozen full-file receipt gate still exits 1 because the source fingerprint changes the header; no baseline was updated. / 前后分别完整运行 Test262，102037 行结果完全一致，零变化和新增失败；既有分类保持不变。冻结完整文件校验仍因源码指纹改变报告头而失败，没有更新基线。

## Reproduction and evidence / 复现与证据

Before / 修改前: `4e2e69bca89a2f287ea5dc99c9557f993eeb1176` (`perf/array-own-key-early-check`). After / 修改后: `a9ddf42ad32cb515335b5db9628897b17bbab0c8`. Later report commits do not change measured runtime code. / 后续报告提交不改变所测运行时代码。

Heavy builds/tests/measurements ran serially through Herdr on `eric-83am` / PocketLab: AMD Ryzen 7 7840HS, Linux 6.18.44-1-lts. Both release binaries used Rust 1.94.1 and the same command below; performance diagnostics were disabled. Workloads were reused unchanged from `ahaoboy/js-engine-benchmark@2034d98fc8c5f8044e186267593f5d5ea5232caf`. / 构建、测试和测量均在远端执行；两版本采用同样的构建参数与未改动工作负载。

```sh
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
CARGO_TARGET_DIR=target/cpu-profile \
QUICKJS_OXIDE_BUILD_COMMIT=$(git rev-parse HEAD) \
cargo build --locked --release -p quickjs-oxide-cli \
  --no-default-features --features profiling
```

Whole-process timing uses Python `time.perf_counter_ns()` with a 90-second timeout. No builds run concurrently with measurements. CPU frequency/system isolation are not controlled; these results do not establish gains for every workload, and CPU stacks were not re-sampled in this round. / 进程计时采用单调时钟、90 秒超时，期间不并行构建；未锁定频率或完全隔离系统，未在本轮重采 CPU 栈，不将结果推广到所有工作负载。

[Machine-readable evidence / 机器可读证据](static-property-atoms.json) includes every timing/probe sample, outputs, exact commands, workload/binary hashes and Test262 row comparison. Outcome-row SHA-256: `fa99d3349bb4b61f30ba57d7c7f275df64f19691edefc50b96b53887339fa8c3`.

Raw archive / 原始包: `quickjs-oxide-static-atoms-pocketlab-2026-09-09.tar.gz`, **42235911 bytes**, SHA-256 **`e3261ee85fab36999888fbf5c3cb3207713896e42f20d1a2aa3f334dd1aace5c`**. All 164 manifest members were verified remotely; the downloaded archive hash also matches. It contains matching before/after executables, probe/runner source, build/test logs, complete Test262 vectors and raw measurements. It is retained in local and remote workspace `target/`, not attached to GitHub; external benchmark source is not included. / 164 个清单成员逐一验证，本地下载哈希一致；原始包保留在本地与远端，不作为 GitHub 附件，不包含外部 benchmark 源码。
