# NonoClaw × Terminal-Bench 接入

在 Terminal-Bench 上评测 NonoClaw 的接入层。包含两部分：

1. **标准评测**：`nonoclaw_agent.py`（tb harness agent）+ `run_standard.sh`
2. **本地冒烟**：`run_local_smoke.py`（无需 Docker，宿主机 tmux 直接跑 3 个任务）

## 快速开始

### 本地冒烟（无需 Docker，先跑这个）

```bash
cd bench/terminal-bench
mkdir -p smoke_work && cd smoke_work
python3 ../run_local_smoke.py            # 跑全部 3 个任务
python3 ../run_local_smoke.py --task hello_world
```

输出示例：

```
✅ PASS  hello_world    turns=  3 in=  4971 out=   102    38.1s
✅ PASS  write_file     turns=  3 in=   154 out=   146    41.1s
✅ PASS  grep_count     turns=  2 in=  5362 out=    68    35.1s
=== 3/3 tasks passed ===
```

### 标准评测（需 Docker + tb CLI）

1. 安装 tb：`uv tool install terminal-bench --python 3.12`（或任意 ≥3.12 解释器）
2. 获取数据集：`bash scripts/fetch_dataset.sh`
   （下载 terminal-bench-core 0.1.1 的 80 个任务到 tb 缓存，自动放宽
   agent/test 超时，并避开 `tb datasets download` 的 git clone 超时）
3. 构建静态 nonoclaw 二进制（任务容器是 Debian 12 / glibc 2.36，宿主动态
   二进制跑不了）：`cargo rustc --release -p nonoclaw -- -C target-feature=+crt-static`
   → 复制到 `bench/terminal-bench/bin/nonoclaw`
4. 运行：

```bash
cd bench/terminal-bench
./run_standard.sh -t hello-world        # 单任务
./run_standard.sh --n-tasks 20          # 前 20 个任务
./run_standard.sh -d terminal-bench-core==0.1.1   # 锁定数据集版本
```

## 已验证结果（2026-08-21，deepseek-v4-pro）

| 任务 | 结果 | 备注 |
|---|---|---|
| `hello-world` | ✅ 100% | 简单，~20s agent + 测试 |
| `csv-to-parquet` | ✅ 100% | 难任务，19 turns，agent 368s（超过默认 360s 上限 → 已放宽到 900s） |
| `count-dataset-tokens` | ✅ | 中 |
| `create-bucket` | ⚠️ build 失败 | localstack 大镜像拉取（网络问题，非代码） |

## 组件说明

| 文件 | 作用 |
|---|---|
| `nonoclaw_agent.py` | `BaseAgent` 实现，驱动 `nonoclaw -p --permission-mode bypassPermissions` |
| `run_local_smoke.py` | 宿主机冒烟：tmux 驱动 + timeout 兜底 + token 解析 + 任务 check |
| `run_standard.sh` | 标准 `tb runs create` 包装脚本 |

## 执行模型（两个组件一致）

```
timeout 900 nonoclaw -p --permission-mode bypassPermissions '<instruction>' \
    > /tmp/nonoclaw_tb_run.log 2>&1; echo __NC_DONE_<id>__
```

- **重定向**：nonoclaw 的运行日志量大，写文件避免撑满 tmux pane 的 pty 缓冲。
- **timeout**：兜底。已知 NonoClaw headless 在 tmux 环境任务完成后可能不退出
  （见下文「已知问题」），timeout 保证 shell 能返回。
- **完成检测**：poll pane 找独立行的 `__NC_DONE_<id>__`；若 log 先出现
  `[turns: ...]`（任务实际完成）而 marker 未出现，`pkill -TERM -f '^nonoclaw -p '`
  杀掉残留进程（精确匹配 headless 模式，不误伤 `--serve-http`/`--acp` 常驻进程）。
- **token**：从 log 的 `[turns: N, in: N, out: N, ...]` 解析，回报给 tb。

## 容器内安装 nonoclaw

Terminal-Bench 每个任务在独立容器里跑，agent 通过 tmux 在容器 shell 里执行
命令。容器镜像需要包含 `nonoclaw`。有两种方式：

**方式 A：修改任务镜像**（推荐，一劳永逸）
构建一个包含 nonoclaw 的基础镜像，通过 dataset config 的 `base_image` 指定
（见 tb 的 dataset 配置文档）。

**方式 B：agent 启动时安装**（快速原型）
在 `perform_task` 里先 `curl -fsSL <nonoclaw安装脚本> | sh`（或
`uv tool install`）再跑任务。注意每次任务都是新容器。

## 已知问题（NonoClaw headless 在 tmux 下的 shutdown hang）

**症状**：`nonoclaw -p` 完成任务、打印 `[turns: ...]` 摘要后，进程不退出
（卡在 tokio runtime shutdown，SIGTERM 可杀）。仅在 **tmux** 环境复现
（普通 subprocess、`script` pty 均正常退出）。

**影响**：tmux 驱动的评测（如 Terminal-Bench）会等满 timeout 或永久等待。

**规避**：本接入已用 `timeout` + summary 检测 + `pkill` 兜底，不影响结果
（任务文件已写入）。**根本修复**在 NonoClaw 侧（engine runtime shutdown），
见 memory/facts 中对应条目。
