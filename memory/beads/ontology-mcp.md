---
id: ontology-mcp
title: 本体论 skill → ontology-mcp server 落地
status: done
priority: normal
updated: 2026-08-17
---

# 已完成
- `~/.nonoclaw/skills/ontology/SKILL.md`：全局 skill（OBR 六要素+九元模型+推理分级+YAML 模板）
- `~/.nonoclaw/ontology-mcp/server.py`：FastMCP stdio server，四工具
- `~/.nonoclaw/ontology-mcp/domains/order-fulfillment.yaml`：测试域 v1.3
- 测试：test_e2e.py 14/14、test_hardening.py 13/13、MCP 协议层验证通过
- 已注册 `~/.nonoclaw/settings.json` mcpServers.ontology

# 遗留三项已落地（2026-08-17 第二轮）
1. **审批闸门**：approvals 表 + AWAITING_APPROVAL 返回 approval_id；LLM 用会话内
   AskUserQuestion 问人 → approve_action(approval_id, approved) 决议。
   拒绝归档 / 二次决议拒绝 / 批准后仍跑 guards（批准≠免检）/ 版本漂移作废。
2. **声明式聚合规则**：aggregate DSL 两种形态——
   events 源 `{source: events, event, match, reduce: sum, field, compare, target_var}`；
   instance 源 `{source: instance, object, key, take, compare, target_var}`。
   删除了 rule_all_items_allocated / rule_invoice_le_shipped 硬编码分支。
   新增 computed 声明字段（ship_order 写 shipped_amount=amount）。
3. **编译+版本hash**：Registry.compile() 启动时 YAML→registry/<domain>.json（键排序规范化），
   SHA-256 短 hash；version_tag = "v@hash"；事件日志、审批记录均携带；漂移检测已测。

# 待办
- [ ] 接真实域（等用户提供素材：EigenFlux/ef-trading/公司业务/NonoClaw 自举）
- [ ] LLM 侧审批协议：AWAITING_APPROVAL 的 note 已引导 LLM 走 AskUserQuestion，
      端到端（agent 真实调用）未验证——服务器重启后可实测
- [x] _key_of 已泛化（2026-08-17 第三轮）：从本体 objects.key 声明读取；
      缺省取首个 required 属性；无则报错引导加声明。order-fulfillment.yaml
      四个对象均已显式声明 key。e2e 14/14 + hardening 13/13 复验通过。
