# NonoClaw — 需求说明书

## Why（为什么做）
把 NonoClaw 做成世界上最好用的项目开发工具：用户只处理逻辑，流程由 agent 机制化引导。

## Who（服务谁）
单机开发者 / 重 CLI 的项目开发者。

## What（做什么）— 功能点 checklist
- [ ] F1. Agent 运行时（loop、compaction、session 持久化）
- [ ] F2. serve + Web 前端（WS + REST API）
- [ ] F3. 技能/插件/图谱扩展系统
- [ ] F4. PR 合并后自动同步需求说明书（requirements-sync 脚本）
- [x] F5. 沙箱化执行环境（命令与文件操作可路由进容器，不触碰宿主机）
- [x] F6. Run 成本预算控制（按模型定价估算累计花费，超预算自动终止）
- [x] F7. SWE-bench 评测（单任务 runner 与批量跑批）
- [x] F8. Prompt cache 命中率基准测试
