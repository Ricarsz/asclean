# asclean

[![package](https://github.com/Ricarsz/asclean/actions/workflows/package.yml/badge.svg)](https://github.com/Ricarsz/asclean/actions/workflows/package.yml)

一个终端界面，用来查看、筛选和删除本机 AI 编码客户端留下的会话与工作区：

- **DeepSeek Harness**（`~/.dsh`）
- **Codex**（`~/.codex`，`sessions/` 下的 rollout 文件与新版 `state_<n>.sqlite`
  里的会话题都算，含 `CODEX_HOME` 形式的独立实例）
- **Opencode**（`~/.local/share/opencode/opencode.db`）

会话按「工作区 → 会话」的树形排在一个视图里：工作区标题行显示会话数、总大小和最近更新，
默认展开，`Enter` 或 `←`/`→` 可以折叠；会话行缩进显示更新时间、客户端、标题和大小。
支持多选删除、整组删除工作区、按客户端切换、按关键字过滤，删除前要确认，也可以只演练。

经过 billion-context 代理的 DSH 会话、接入 magic-context 的 Opencode 会话都会标出附带数据，
删除时一并清理。

## 安装

Arch Linux 用 AUR：

```sh
paru -S asclean
```

从仓库里的 `PKGBUILD` 本地构建并安装：

```sh
git clone https://github.com/Ricarsz/asclean.git asclean
cd asclean
paru -Bi .
# 或者：makepkg -si
```

只想要二进制的话，直接 `cargo build --release`，产物在 `target/release/asclean`。
需要 Rust 1.88 或更新（使用了 let-chains）。运行时优先用内置的纯 Rust zstd 解码；
系统里有 `zstd` 命令时会作为兜底。

## 使用

```sh
asclean                 # 打开 TUI
asclean list            # 列出会话（--workspace <spec> 只看一个工作区）
asclean workspaces      # 列出工作区（含还没有会话的登记项）
asclean delete <id>     # 删除指定会话（可加 --dry-run）
asclean delete-workspace <spec>  # 删除整个工作区（可加 --dry-run）
asclean gc              # 清理残留：目录已删、登记/缓存/bili 还留着的记录（--dry-run 只统计）
asclean doctor          # 检查扫描位置、数据库与依赖
asclean paths           # 只打印扫描路径
asclean config --init   # 写入默认配置
```

`delete` 的 ID 形如 `dsh:session-<uuid>`、`codex:<uuid>`、`opencode:ses_<id>`；
不带前缀时会在所有客户端里按 ID 匹配，匹配到多个会拒绝执行。

`delete-workspace` 与 `list --workspace` 的 `<spec>` 可以是工作区标识、完整路径、
标题或路径末段（例如 `Projects` 或 `/home/you/Projects`），也可以带 `dsh:`、`codex:`、
`opencode:` 前缀限定客户端；匹配到多个会列出候选并拒绝执行。

### 键位

| 按键 | 作用 |
| --- | --- |
| `↑` `↓` / `k` `j` | 在树上移动（工作区标题行与会话行） |
| `PgUp` `PgDn` | 翻 10 行 |
| `Home` `End` | 跳到首尾 |
| `Enter` | 折叠 / 展开当前工作区 |
| `←` / `h` | 折叠当前工作区（会话行会跳回它的标题行） |
| `→` / `l` | 展开当前工作区 |
| `空格` | 会话：勾选并下移；工作区：整组勾选 / 取消 |
| `a` | 全选可见会话 / 取消全选 |
| `d` | 删除（有勾选删勾选；工作区行删整个工作区） |
| `X` | 清理残留（会话目录已不在，登记 / 渲染缓存 / billion-context 里还留着的记录） |
| `Tab` / `Shift+Tab` | 切换客户端视图 |
| `/` | 按标题、ID、目录、工作区过滤 |
| `J` / `K` | 滚动右侧详情 |
| `r` | 立即重新扫描 |
| `t` | 切换主题 |
| `?` | 显示 / 隐藏帮助 |
| `q` / `Esc` | 退出（先清工作区过滤，再清关键字过滤） |

鼠标：左栏滚轮移动光标、点击选中，再点一次已选中的工作区行折叠 / 展开；
右栏滚轮翻详情。`mouse = false` 可以整体关掉。

## 工作区

会话归属哪个工作区，按各客户端自己的记录来识别：

| 客户端 | 识别方式 |
| --- | --- |
| DeepSeek Harness | 读 `~/.dsh*/storages/workspace.json` 的登记表，用会话 ID 反查；退而用会话的 `cwd` 匹配工作区路径；再不行就按 `cwd` 现合成一个 |
| Codex | 没有工作区表，直接按会话的 `cwd` 归并 |
| Opencode | 读 `opencode.db` 的 `project` 表，按 `session_v2.project_id` 关联；没有会话的项目也会列出来 |

识别不到工作区的会话按客户端归到「未识别工作区」一组，只能逐个删除。

DSH 的子会话（会话记录里 `origin: subagent`）在列表里带 `子` 标记，详情面板显示父会话标识，
并跟随父会话的工作区，避免同一个工作区在树里裂成两行。

TUI 里工作区标题行默认展开，按 `Enter` 或 `←`/`→` 折叠、展开；在标题行按 `d` 删除整个工作区，
命令行对应 `asclean workspaces` 与 `asclean delete-workspace <spec>`。

删除工作区做两件事：删掉它名下的会话，再移除工作区登记（DeepSeek Harness 改
`workspace.json`，Opencode 删 `project` 行）。**项目目录本身不会被删除**；只要还有会话
没删干净（例如正被其他进程占用），登记就会保留，避免留下没有归属的会话。

## 会话存储

| 客户端 | 位置 | 删除方式 |
| --- | --- | --- |
| DeepSeek Harness | `~/.dsh*/sessions/<workspace>/session-<uuid>/` | 删除整个会话目录；`session.lock` 被占用时跳过 |
| Codex | `~/.codex*/sessions/YYYY/MM/DD/rollout-*.jsonl` | 删除 rollout 文件，并同步移除 `session_index.jsonl` 里的记录 |
| Opencode | `~/.local/share/opencode/opencode.db` | 在事务里删除 `session_v2` 行，消息等子表由外键级联删除 |
| billion-context | `~/.local/share/billion-context/sessions/index.db` | 随 DSH 会话一起删除：事务内清掉同名 `session_id` 的各表记录，并回收无人引用的 `content` |
| magic-context | `~/.local/share/cortexkit/magic-context/context.db` | 随 Opencode 会话一起删除：事务内删掉所有带 `session_id` 列的表里该会话的行 |

扫描位置可以用环境变量覆盖，多个路径用 `:` 分隔：

```sh
ASCLEAN_DSH_ROOT=~/.dsh-alt asclean list
ASCLEAN_CODEX_HOME=~/.codex-alt asclean doctor
ASCLEAN_OPENCODE_DIR=/path/to/opencode asclean list
ASCLEAN_BILI_DIR=/path/to/billion-context asclean doctor
ASCLEAN_MAGIC_DIR=/path/to/magic-context asclean doctor
```

若多个根目录的 `sessions/` 指向同一份存储（例如用符号链接共享），只会列一次，
并在 `doctor` 里提示已合并。

## billion-context 联动

DSH 会话只要经过 billion-context 代理，就会在 `sessions/index.db` 里留下同名记录
（`session_id` 与 DSH 会话目录同名）。asclean 打开时会扫这份库：

- 会话行的客户端标记后带 `•`，大小列与头部统计都包含它的附带数据；
- 详情面板显示附带数据大小、消息数、最后更新和数据库路径；
- 删除 DSH 会话时在同一个事务里清掉 `messages`、`blocks`、`refs`、`checkpoints`
  等表里该会话的行，再回收没人引用的 `content`；
- 清理失败不会影响 DSH 会话本身的删除，只在状态栏给出提示。

`asclean doctor` 会检查数据库、`-wal`/`-shm`/锁文件，并统计对不上 DSH 的残留记录。
数据根可用 `ASCLEAN_BILI_DIR` 覆盖，指向目录或 `index.db` 都行。

## magic-context 联动

Opencode 会话装过 magic-context 插件后，`context.db` 里会留下同名记录（`session_id`
就是 opencode 的 `ses_...`）。asclean 打开时会扫这份库：

- 会话行的客户端标记后带 `•`，详情面板显示隔间数、最后活动时间与数据库路径；
- 工作区详情汇总该工作区里的 magic-context 隔间数；
- 删除 Opencode 会话时，在同一个事务里删掉所有带 `session_id` 列的表里的对应行
  （插件版本升级新增的表也能覆盖），不碰 `memories`、`workspaces` 等跨会话数据；
- 清理失败不会影响 Opencode 会话本身的删除，只在状态栏给出提示。

插件本身只在收到 opencode 的 `session.deleted` 事件时清理；asclean 直接改库不产生
这个事件，所以在删除流程里补做一次。

`asclean doctor` 会检查数据库、`-wal`/`-shm`、关联与残留记录、隔间总数。
数据根可用 `ASCLEAN_MAGIC_DIR` 覆盖，指向目录或 `context.db` 都行。

## 配置

路径：`$XDG_CONFIG_HOME/asclean/config.toml`，默认 `~/.config/asclean/config.toml`。
改名前的 `~/.config/sesstui/config.toml` 只在读不到新文件时兜底读一次，写入永远走新路径。

```toml
theme = "ocean"        # ocean | forest | sunset
mouse = true           # 滚轮滚动、点击选择
rescan_secs = 30       # 自动重扫间隔，最小 5 秒
confirm_delete = true  # 删除前弹确认框
```

## 残留清理

DSH 自己删会话时只删目录，工作区登记表、`storages/session_projcache/` 里的渲染缓存
和 billion-context 的 `index.db` 记录都可能留下。asclean 的删除流程会同步清掉这些，
之前漏掉的用 `X`（TUI）或 `asclean gc`（CLI）补：

- 只处理 DSH 形状的 ID（`<uuid>` 或 `session-<uuid>`），`dsh-pet` 这类别的客户端记录不动；
- 判断「目录还在不在」看磁盘上所有 DSH 根，不看本次扫描范围，避免环境变量收窄时误删；
- 一个 DSH 会话目录都找不到时直接放弃，宁可少清；
- `--dry-run` 只统计条数与可释放字节。

旧版本 DSH 的会话目录名是裸 UUID、新版本带 `session-` 前缀，两种写法按同一会话处理。

## 删除安全

- 删除前会把目标路径规范化，并确认它仍在对应的存储根目录内，避免误删。
- DeepSeek Harness 会话被其他进程加锁时直接跳过，不会强删。
- Opencode 的删除在单个事务里完成，失败会整体回滚。
- 删除工作区只清理会话存储与登记，项目目录保持原样；有会话删不掉时保留登记。
- billion-context / magic-context 的数据在删除对应会话的同一流程里清理；清理失败会保留会话本身的删除结果并提示。
- `asclean delete --dry-run` 与 `asclean delete-workspace --dry-run` 只打印目标，不落盘。

## 限制

- 扫描器按格式解析各客户端的会话文件；客户端改格式后需要同步更新。
- 正在写入的会话可能读到不完整的数据，按 `r` 重扫即可刷新。
- Opencode 数据库被长时间占用时删除会报锁错误；关闭对应进程后重试。
- 目前只在 Linux 上验证过。

## 许可

MIT，见 [LICENSE](LICENSE)。
