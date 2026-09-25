# 性能历史报告入口

当前唯一性能计划位于 [Safe Rust 性能改善路线](../performance/README.md)。本目录不再安排 S0–S3 的后续实施；保留历史证据入口，而不是把过期计划复制进一个新的 archive 后继续当路线使用。

## 当前证据与历史原文

[issues #41–#44 的新测量账本](../performance/evidence.md) 是本轮裁决依据。以下两份纯测量报告保留关键结果和完整原文的固定版本入口：

- [B1/C 负结果](s3-c-negative-result.md)：为什么投影、二次分类和单槽 TOS 没有净收益。
- [旧全量重测](s3-full-rerun-results.md)：当时各构建协议、V8 分项及测量限制。

二者本次整理为历史摘要；完整原文没有被改写成新的实验结果，可通过其固定 commit 链接读取。Git 历史保留所有被删除文件。

## 被取代的规划／混合记录清单

下表各文件从当前树删除。旧文档内的测量仍可在固定快照查看，不再与新的计划争夺优先级或声称“待裁决”。

| 原文件 | 处理理由与替代入口 |
| --- | --- |
| `performance-plan.md` | S0–S2 与 S3 选型混放；现用新路线与证据账本 |
| `performance-architecture.md` | 多轮状态与过期 8B／B/C/D 路线混放；现用新实施设计 |
| `s3-a-plan.md` | 已实施阶段与遗留回退混放；旧债务在新测量协议单列 |
| `s3-a-closure-plan.md` | T1/T2 收尾路线已过期；不作为新增收益 |
| `s3-a4-d-b-decision.md` | 旧选型裁决及测量；历史事实保留固定版本，不重复立项 |
| `s3-b-initial-plan.md` | 已撤回的 B1 投影方案 |
| `s3-b-plan.md` | B2.1 记录与 B2.2/B2.3 待实施计划混放；由 #41–#44 重新裁决 |
| `s3-bc-implementation.md` | 已回退的混合接线／实施状态；负结果单独保留 |
| `s3-c-b-opening.md` | 已回退路线的开头接线计划／记录 |
| `s3-c-b-next.md` | 已回退路线的后续计划／记录 |
| `s3-c-plan.md` | 已关闭的 TOS／StoreDrop 路线 |
| `s3-c2-boundary-audit.md` | 已关闭 C2 的待接线清单；不再作当前实现指令 |
| `s3-d-plan.md` | D1–D4 已成为基线，D5 条件未成立；不再重复计算收益 |
| `s3-full-rerun-plan.md` | 当时重测任务已结束；现用统一测量协议 |

上述 14 份文件的完整原文统一位于
[清理前的 reports 快照](https://github.com/pocket-nexus/quickjs-oxide/tree/f531f6052cb497ce4707f01c276e8642e5e26788/docs/reports)。
例如恢复单份文本：

```sh
git show f531f6052cb497ce4707f01c276e8642e5e26788:docs/reports/s3-b-plan.md
```

清理不涉及引擎源码、benchmark 实现／外部语料、Test262 receipt、parity 契约、编译器／解析器独立文档。新计划的 3–4 倍目标没有因删旧文件而成为已取得成绩。
