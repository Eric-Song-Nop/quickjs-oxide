# 改善实施设计与切片

> 这是待实施设计，不是已完成的引擎改造。
> 事实与状态以 [证据账本](evidence.md) 为准；所有切片服从 [统一门禁](measurement.md)。
> 两条实施线：A 收尾紧凑错误载体；B 删除数组／数值数据流中的实际工作。调用与检查重排仅列为条件项。

<a id="a-error"></a>
## A. 紧凑错误载体收尾

### A1. 恢复并审查已测候选

首先取得 #41 报告的 `0cd4acee` 补丁、完整报告和构建 receipts。当前主仓 R0 不包含它，本次读取也没有解析到该 ref；不能以同名的新实现冒充原实验。原候选不可取得时，允许独立重建最小候选，但必须赋予新身份并从零 A/B。

审查范围限定为 `src/engine/api/error.rs` 的冷 payload 及直接受影响的测试。保留 `Debug`、错误 kind/message/span、CLI 输出和退出码；明确记录公共 `Error` 的表示变化、`const fn` API 和分配行为。不同时改 opcode、stack facade、内联注解或 release 协议。

### A2. 接纳前补测

按统一测量脚本在无人并发使用的主机上重测 #41 的 13 个固定负载，重点复核 `prop_write` 的 cycles。补错误密集场景的分配次数／字节、峰值 RSS、构造和传播成本；分配观测使用独立诊断构建或外部工具，不用插桩产物发正式 Score。

对公共 API 使用者测试 `kind`、`span`、`with_span`、Debug 格式、带 native message 与嵌套错误。对异常场景测试 TDZ、语法错误、getter throw、类型转换 throw、普通对象 throw、栈溢出／容量不足；对成功场景确认错误对象没有被预先构造。

反汇编复核实际保留的边界：`copy_value/copy_reference`、`push_current`、`local_current`、`parameter_current`、`replace_local_current`、`pop_current`、`array_immediate_read_current`、`property_ic_write_scalar_current`，以及 `run`、`numeric_local_add`、`numeric_local_field_add` 和比较／更新 handler。记录内联消失、返回寄存器／返回槽、调用者栈帧和 spill，不从 `size_of` 单独推导速度。

### A3. 决策与后续边界

固定矩阵、真实 V8、错误分配／RSS与一致性通过后，单独接纳候选并生成新的工作基线 R1；它是一个已测小改动，不是批准全仓库错误通道重构。若 cycles 或错误路径成本不通过，回退候选，R1=R0，B 线仍可继续。

候选接纳后重新归因。只有新产物仍保留、且在真实负载上足够热的调用边界，才允许追加窄成功路径。不能复用旧的 80B 模型推测剩余成本，也不要求给已经内联的 helper 添加无意义 facade。

<a id="b-arrays"></a>
## B. 数值／数组执行块

### B0. 先认证目标形态，不从 benchmark 名称识别程序

目标是通用的 JS 数据流，不是 `array_read.js`、Crypto 或某段源码的特殊识别。先在固定上游版本上 dump Crypto `am3` 与 NavierStokes `project/lin_solve/advect` 的真实 canonical 指令，统计候选序列、动态执行次数、binding 类别和失败原因。报告中必须有至少一个真实内核对应的 opcode 形态，不能只展示人工循环。

首批候选限定为一个无内部控制流入口、无回调、无分配的短直线跨度。沿用 `FusionPlan::build` 的入口检查、同一 canonical PC 和短期 `FrameTransaction`；不创建第二个全函数 QuickOp 数组，不给每条普通指令增加新的通用查询层，不实现 TOS install/spill facade。元数据无候选不分配；全函数扫描成本和内存必须测量。

拟议 API 名称仅是设计草图，不能在报告中写成当前已经存在的能力：

```rust
// 意向接口：Number 是立即数；不返回拥有式 JsValue 或堆引用。
fn peek_dense_number(/* runtime、当前 receiver、index */) -> Option<Number>;
// 完整预检后只有一次不可失败提交；None 前不更改任何状态。
fn try_numeric_span(/* 已认证窗口、不可变指令 */) -> Option<NextPc>;
```

不得通过在这些接口内部调用完整 `array_immediate_read_current` 来“复用实现”：它仍处理拥有式栈输入及 release，恰好是本路线要删除的工作。应复用下层数值语义、数组有效性规则和存储判定，而不是复用整条事务。

### B1. 第一片：非拥有数组读取

从真实字节码选择 `producer(base); producer(index); GetArrayEl` 形态；producer 首批为经过发布验证的直接 local／argument 或允许的整数常量。若真正热点先产生位运算索引，则将该形态列为下一片，不无条件囊括所有表达式。

执行顺序及证明：

| 阶段 | 必须证明 | 成功路径做什么 | 失败时 |
| --- | --- | --- | --- |
| 发布 | 形态、索引、内部入口、栈效应、终点 PC | 生成已有载体可表示的候选事实 | 不生成候选 |
| 窗口 | 当前 frame authority、输出容量、直接 binding | 借用 frame 持有的 base，不 retain 临时 owner | 原地 canonical |
| 动态准入 | 当前 base 是普通 dense Array；index 是允许的非负整数；元素存在且为 Number | 短期共享 heap 借用，读取立即数 | 无副作用回落 |
| 提交 | 结果槽可写，原输入未被消费，无待释放 owner | 一次安装结果与 depth/PC 更新 | 提交后不允许再返回 miss |

当前 `array_immediate_read_current` 在 [stack.rs](../../src/engine/vm/stack.rs) 中取出拥有式 base/key，替换栈槽并分别 release。新跨度的预期删除清单是：base 的临时 retain/release、数字 index 的 push/pop、通用操作数槽检查及对应 helper 链。最终输出若仍被 canonical 消费，可以保留一次结果压栈；这一片不是假装已经消除了全部中间值。

safe Rust 实现使用普通索引／切片和作用域借用。frame 的 owner 必须在整个读取期间真实存在，heap 借用不得跨过回调、释放或可能使地址失效的操作。保留必要 bounds／generation／live-state 检查；LLVM 能否消除重复检查，以机器码确认。不能为了让 Rust 接受借用，把 frame、对象或数组数据 clone 出一份。

### B2. 第二片：连接索引、读取和数值写回

在 B1 的真实覆盖与净收益通过后，扩展到类似下列数据流：

```text
读标量 index → 与常量做 BitAnd → dense 数值读 → 与 acc 做 Add → 写回 acc
```

只把其作为一个通用形态例子，不承诺当前编译器一定生成该序列。发布器按实际 opcode 精确匹配；栈保留语义必须区分 `PutLocal` 与 `SetLocal; Drop`，不带 Drop 的 Set 不得误折叠。

所有动态检查完成之前，不移动 owner、不改 local/depth/PC、不预热 IC、不构造 Error。成功域只接受可用同一套 Number 语义执行的值；String、BigInt、对象转换及 TDZ 回到原 PC。算术结果可以在 Rust 局部变量中传递，但不得通过替换 JS 运算顺序、FMA、重关联或饱和 cast 改变结果。

在一个块内复用只要未被本块操作改变的事实。索引变了就重新计算索引，接收者变了就重新验证接收者；“无 JS 回调”不等于“本块所有数据都恒定”。若 acc 与 index 是同一槽，先保留 canonical 要求的旧值，再进行唯一提交；必要时首批拒绝复杂 alias。用 `split_at_mut` 或独立的标量读取处理合法不相交访问，不引入 unsafe，不为绕过借用检查分配临时 Vec。

首批每次跨度只允许一个最终可观察写入。后续多表达式／多 store 块必须定义精确提交点与恢复状态，不能把“成功后若再 guard 失败就从首 PC 重跑”当作实现。

### B3. 第三片：只写已有 dense 数值槽

只在 B1/B2 证明真实效果后实现。首批只替换数组内已存在、允许写入的数值槽：不扩容、不改 length、不新增属性、不经过 setter，不处理引用值的最后 owner 释放。数组只读／冻结状态、特殊元素或转慢存储一律回落。

复用当前数组写路径的有效性约束，但把多次拥有式栈搬运合并为一次预检、一次 Number 写入。写入与 guard 之间不能回调或释放 owner。若一块涉及多个数组，先检测 alias；禁止因假定 `x != x0` 而重排 NavierStokes 中可能别名的读写。保留每一次 JS 浮点操作的顺序。

### B4. 覆盖升级：参数、this、捕获绑定

各类 producer 分开统计：direct local、direct argument、this、captured initialized、TDZ／其他；不把 captured miss 混成一个不明的“generic”。当前 `immediate_local` 等拒绝 Captured 是基线事实，而不是以后永远不支持的限制。

仅在真实画像显示覆盖被捕获读取显著限制时，另开只读捕获候选：从实际 closure slot 验证初始化和数值域，只在短无回调窗口内使用读出的值。不得长期缓存 cell 的值或假定两个共享 bytecode 的闭包共享环境；捕获写入、mapped arguments、eval 可观察绑定必须单独证明或回落。

若高覆盖只能靠跨回调／跨循环保存 heap 借用实现，则这一形态不属于本轮执行块，停止扩展而非放松安全边界。

### B5. 事实缓存与失败成本

静态形态事实可发布时保存；动态事实每次按真实失效条件验证。区分“同一 receiver”与“同布局的不同 receiver”：命中后者必须从当前 receiver 读取，不能复用上一个对象的值／closure／身份。shape 与 revision 也不能被不明含义的裸索引取代。

本轮不默认引入适配计数器或按失败三次永久禁用。如果未来 per-site 状态有依据，应懒分配、无 owning edge、有内存上限；先比较无状态跨度，再用命中／失败成本加权证明新状态值得。`Megamorphic(1024)` burst 的处理另做实验，不与数组读取片混入。

失败类别至少包括 binding、数字类型、base kind、index、空洞／越界、慢数组、IC 状态、shape／revision、输出容量。profiling 计数与普通 release 分开；必须同时报告 attempts、成功覆盖的逻辑操作、每类 miss 成本和非候选路径成本。不得只发命中率。

### B6. 正确性证明与测试矩阵

| 风险 | 必测输入与断言 |
| --- | --- |
| 数字语义 | Int 边界、表示晋升、NaN、正负零、Infinity、移位超过 31、无符号移位、浮点顺序；与 canonical／pinned oracle 对照 |
| 数组形态 | dense 数字／显式 undefined／空洞、负数／浮点／字符串 index、边界两侧、delete、length 缩短、Array.prototype 数字 getter、冻结／只读、Proxy、转慢存储 |
| 绑定 | direct/captured、多个共享代码但不同环境的闭包、TDZ、const／checked store、参数缺省、mapped arguments、别名 local |
| owner 与 identity | receiver 是唯一活 owner、对象／槽复用、generation 不匹配、临时 owner 清理、结果精确一次提交；引用输入不进入无释放证明的成功域 |
| 控制与异常 | 内部跳转入口不生成 span、try/catch/finally、getter／valueOf 计数顺序、原 PC 回落、错误消息与 fault PC、资源限额及容量不足 |
| 挂起与观察 | generator／async 恢复使用 canonical PC；遇可观察边界先退出块，禁止持借用恢复或重放已提交副作用 |
| 插桩与资源 | `record_span` 逻辑指令权重、深度变化、owner 事件、零候选函数不分配；编译成本和首次执行不被隐藏 |

关键提交不变量：**miss 时状态逐字节／逐 owner 等价于进入跨度之前；hit 时等价于完整 canonical 跨度结束之后。** 提交后不存在可失败动作。后续确需多提交块时，应把它拆成多个有独立 PC 的片段，而不是一开始引入复杂 deopt 日志。

测试同时包括有意永不命中的完整语义输入和真实命中的输入；不能只在程序结果相同时就忽略错误顺序、引用存活或 frame 状态。

### B7. 文件面、性能判据与回退

| 文件／模块 | 预计变更 | 不应夹带 |
| --- | --- | --- |
| `src/engine/code/fusion.rs` | 精确形态、内部入口／栈效应认证 | 通用统一查询重构、全函数第二执行数组 |
| `src/engine/vm/run/fusion.rs` | 短 Number／数组 handler | 将整套 generic helper 原封不动包一层 |
| `src/engine/vm/stack/window.rs` | 非拥有 producer 与单次提交能力 | 放宽 frame authority、长寿命借用 |
| `src/engine/vm/run.rs` | 最小入口接线 | 重排无关 opcode 臂、全量 inline(always) |
| 数组／heap 模块 | 短期只读 Number 投影及已有数值槽替换 | 新 GC 模型、去 generation、取消特殊语义 |

每片分别提交 shape 认证／测试与引擎接线；发布的实现 PR 必须同时包含证明与性能结果，不能先默认开启再把回退留给“后续裁决”。入口成本对照和强制 miss 诊断的编译器消除风险见测量文档。

P2 目标固定探针至少 −30% 指令、对应真实子项至少 +3% Score，且所有不回退门禁通过，才批准 P3 扩大。若只在人工形态获益、真实执行几乎不触发，收缩为窄优化或撤销；不建设一个更复杂的通用 IR 来掩盖覆盖不足。

<a id="c-deferred"></a>
## C. 条件项与明确不做

**调用路径：** #43 前置画像已完成，不再安排一次同样的广泛调研或通用 native 借用参数改造。仅在 A 后的统一八项画像仍支持 DeltaBlue 窄热点时，比较 frame 安装／清理的最小候选。真正的 callsite cache 须先测 per-PC callee 分布；必须守卫实际函数、realm、this、默认参数、arguments 与栈限制，不缓存跨闭包环境、不凭属性名识别 builtin、不持永久强 callee owner。native argv 已池化，不再设“去掉每次分配”目标。

**S3 检查顺序：** #44 的候选与复读变体均不接纳。重新实施必须用新基线的五类成本、真实分布和完整 helper 反汇编给出负的加权成本；4.8% 只是特定两类模型的阈值。没有新证据不重复布局彩票。

**B2.2、IC 冷却与 codegen：** 可在数组主线后作为局部实验，但旧 752→632 上界不构成倍数总分依据。优化 `run` 不变也不够，必须检查 outlined handler 的内联与 spill。源码位置变化不能直接诊断 I-cache／分支预测。

**不重新立项：** B1 全量 QuickOp 投影、单槽 TOS facade、无依据的统一 span 查询、NaN-box、已完成的 D 布局工程、通过延迟 RC 或修改 benchmark 来提高分数。目标是删除工作，同时守住真实程序与 safe Rust 的边界。
