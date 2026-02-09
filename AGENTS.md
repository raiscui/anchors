# Repository Guidelines

## 项目结构与模块组织
> src/lib.rs:3-5
> pub mod collections;
> pub mod expert;
> pub mod singlethread;
仓库采用 `src` 目录作为核心实现.
`src/singlethread` 提供默认 `Engine` 与 `Var` 能力.
`collections` 覆盖响应式容器, `expert` 暴露底层控制接口.
`tests/` 记录 slotmap 样本.
`examples/` 演示静态与动态组合.
`benches/` 集中 Criterion 脚本.
`.gitignore` 忽略 `target/` 与 profiling 数据.
撰写文档时引用精确路径, 方便交叉验证.

## 编译、测试与开发命令
> readme.md:71-74
> You can check out the `bench` folder for some microbenchmarks.
常规编译执行 `cargo build --all-features`.
预检使用 `cargo check` 观察告警.
全量测试运行 `cargo test --workspace`.
定位特定测试可执行:
`cargo test singlethread::test::test_readme_example -- --exact`.
性能回归执行 `cargo bench --bench stabilize_linear_nodes_simple`.
如需 Criterion 网页报告, 附加 `--features html_reports`.
SlotMap 流程建议跑 `cargo test --test slotmap_pending_requests --features anchors_pending_queue`（pending queue 默认关闭）。

## 编码风格与命名约定
`Cargo.toml` 配置 `edition = "2024"`, 需在 nightly 下编译.
变量保持驼峰, 模块与类型遵循 Rust 惯例.
`im_rc::OrdMap` 等不可变结构非常常见.
需要注释解释设计, 但避免冗长.
`singlethread.rs` 已有中文注释, 新增文本需匹配语气.

## 测试指引
`tests/` 目录涵盖 pending 队列与 slotmap 回收边界.
`src/singlethread/test.rs` 展示 `Engine::new` 到 `mark_observed` 的链路.
新增测试沿用 `test_*` 命名, 并保持 Arrange-Act-Assert 节奏.
Criterion 基准结束后记录 `target/criterion` 摘要.
不要提交原始数据.
若依赖外部资源, 连接失败时必须 panic 提醒启动服务.

## 提交与 PR
> git log -5 --oneline
> 70bc2a5 mem
> 8c1223f PendingDefer
提交说明保持一行一事, 推荐格式 `组件: 简述`.
允许中英文混排, 但需清晰表达影响.
PR 描述应包含动机、影响面与运行命令.
若涉及 UI 或性能变化, 提供截图或指标.
关联 Issue 并列出期待的评审人.

## 四文件上下文（anchors 子目录）

- 对于多步骤任务（>=3 步）:
  - 使用 `anchors/task_plan.md` 跟踪阶段与决策.
  - 使用 `anchors/notes.md` 记录调研与结论.
  - 使用 `anchors/WORKLOG.md` 记录最终执行事实.
  - 使用 `anchors/ERRORFIX.md` 记录问题/根因/修复/验证.
- 上述文件采用追加写入策略,避免中间插入造成追溯困难.
- 若出现历史版本（如 `task_plan_*.md`）,连续学习结束后移动到 `anchors/archive/`.

## 安全与配置提示
`lock_strict` 默认启用, `ANCHORS_LOCK_STRICT` 非 0 时阻断重入缺陷.
`Engine::export_dot_from_tokens` 仅在可信环境调用, 避免泄露图谱.
排查死循环或残留节点时可开启 `ANCHORS_DEBUG_SPIN`.
需要长程监控时再启用 `ANCHORS_ACTIVE_NODES_LOG`.
