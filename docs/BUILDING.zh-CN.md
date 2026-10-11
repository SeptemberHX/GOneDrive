# 构建与开发

[English](BUILDING.md) | **简体中文**

> 普通用户请直接用 [Releases](https://github.com/SeptemberHX/GOneDrive/releases) 里的 deb 包，见 [README](../README.zh-CN.md)。本文面向自己编译、打包或参与开发的人。

## 构建

```bash
# 依赖（Ubuntu / Debian）
sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config build-essential

# Rust 工具链
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

cargo build --release
```

有了 client_id（见下一节）之后，用这条命令构建可分发的二进制：

```bash
GONEDRIVE_CLIENT_ID=<你的 id> cargo build --release
```

SQLite 与 TLS 无需系统开发包：SQLite 由 `rusqlite` 静态编译，TLS 走 rustls。

产物：

| 二进制 | 作用 |
|---|---|
| `target/release/gonedrived` | 后台同步守护进程（普通用户权限） |
| `target/release/gonedrive` | GTK4 前端（纯 D-Bus 客户端，不持有凭据） |
| `target/release/gonedrive-hsm` | 按需下载 helper（root 运行；不联网、不持有凭据） |

---

## 注册 Entra ID 应用（一次性，约 3 分钟）

**先说清楚：`client_id` 不是你的账户。** 登录时你仍然是在微软官方页面用自己的 Microsoft 账户输密码。`client_id` 只是"哪个程序在请求访问"的标识 —— 它不是机密，微软的 OAuth 也不存在免注册路径（ROPC 对个人账户和开了 MFA 的账户都是禁用的）。

**这是构建者做一次的事，不是每个用户都要做的事。** 和 abraunegg/onedrive、rclone、onedriver 的做法一致：注册一个应用，把 id 烤进二进制，之后所有用户开箱即用；需要独立 API 配额的人再自行覆盖。

### 优先级

| 来源 | 用途 |
|---|---|
| 环境变量 `GONEDRIVE_CLIENT_ID` | 覆盖一切，开发和临时测试用 |
| 配置文件里的 `client_id` | 用户级覆盖（比如共享注册被限流了，想用自己的配额） |
| 构建期烤入 | **推荐的分发方式**，见下 |

三者都没有时，守护进程会拒绝启动并打印完整指引，而不是稍后在微软那边报一个什么都没说明的 `AADSTS700016`。

### 注册步骤

前提：你需要一个 Entra 租户。**个人 outlook.com 账户默认没有** —— 有工作/学校的 Microsoft 365 账户就直接用它登录（注册出来的应用照样能登录个人 OneDrive）；没有的话在 [azure.microsoft.com/free](https://azure.microsoft.com/free) 注册免费账户，会自动创建一个 Default Directory。

1. 打开 [Entra ID 门户 → 应用注册 → 新注册](https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade)
2. **名称**随便填，比如 `GOnedrive`
3. **支持的账户类型**选 **“任何组织目录中的帐户和个人 Microsoft 帐户”**
   （少了"个人"这半句，outlook.com 账户登录会被拒）
4. **重定向 URI**：左侧下拉选 **“公共客户端/本机(移动和桌面)”**，右侧填 **`http://localhost`**
5. 创建后在概述页复制 **“应用程序(客户端) ID”**

> **`http://localhost` 必须一字不差，不要加路径、不要加端口。**
> 微软匹配 `localhost` 重定向 URI 时会忽略端口（这正是本客户端能用随机端口的原因），
> 但**路径是精确匹配的**。填成 `http://localhost/callback` 而客户端请求 `http://localhost:1234`，
> 会得到 `AADSTS50011: reply URL does not match`。
> 这也正是微软官方对"使用系统浏览器的桌面应用"的推荐值。
>
> 文档建议优先用 `127.0.0.1` 而非 `localhost`，但门户 UI 拒绝添加 `http` + `127.0.0.1` 的组合
> （需手改应用清单 JSON），且端口豁免规则是针对 `localhost` 写的。一次点击胜过一次改清单。

### 用起来

```bash
# 分发方式：烤进二进制，用户不必再管
GONEDRIVE_CLIENT_ID=<你的 id> cargo build --release

# 或临时试一下
export GONEDRIVE_CLIENT_ID=<你的 id>
```

之后 `gonedrived login` 打开浏览器，用**你自己的 Microsoft 账户**正常登录并同意授权即可。refresh token 存进系统密钥环，不落数据库文件。

---

## 安装方式

两条路径，**二选一**，不要同时使用。

**一、deb 包（推荐给日常使用）**

```bash
GONEDRIVE_CLIENT_ID=<你的应用 ID> ./build-deb.sh
sudo apt install ./gonedrive_*.deb
```

包内的二进制在 `/usr/bin`，helper 与用户服务由包自己注册，`apt upgrade` 即可升级。

**二、源码安装（推荐给开发）**

```bash
sudo ./install.sh     # 二进制 + 特权 helper 系统服务（唯一需要 root 的一半）
./install.sh --user   # 守护进程用户服务 + Nautilus 集成 + 图标
```

它装到 `/usr/local/bin` 与 `$HOME`，两者都排在包内文件之前。所以装了 deb 之后再跑
`install.sh`，升级包会"装上但跑不起来"——旧二进制继续运行，界面上看不出任何异常。
`install.sh` 因此会检测到已安装的 deb 并直接拒绝执行；确实需要并存时用
`GONEDRIVE_ALLOW_BESIDE_DEB=1` 覆盖。

从源码安装切换到 deb 包时，先删除源码安装的文件：

```bash
sudo rm /etc/systemd/system/gonedrive-hsm.service \
        /usr/local/bin/gonedrive /usr/local/bin/gonedrived /usr/local/bin/gonedrive-hsm
sudo systemctl daemon-reload
```

---

## 测试

```bash
cargo test --workspace
# GTK 测试需要显示环境，CI 中这样运行：
CI=true xvfb-run -a dbus-run-session -- cargo test --workspace
python3 -m unittest discover -s data/nautilus/tests
```

端到端集成测试用 `wiremock` 起一个假 Graph，驱动真实的引擎、数据库和文件系统，覆盖
lazy 首登、目录 Partial/Populated 边界、children/delta 并发顺序、增量、远端删除、
本地新增、双端冲突、单项延期、410 重同步、429 限流退避，以及“待下载不能被误判为
本地删除”等数据安全守卫。

单元测试覆盖冲突判定矩阵的每一格、delta 拓扑排序、路径推导、大小写冲突、分片边界（4 MiB 阈值、320 KiB 倍数），以及 OAuth loopback 回调的 state 校验和错误透传。

## 调试辅助

- `GONEDRIVE_PAGE=activity|folders|settings`：打开面板时预选页面。
- `GONEDRIVE_NO_TRAY=1`：不启动托盘图标。
- `GONEDRIVE_SNAPSHOT=/path.png`：离屏渲染当前窗口后退出。离屏渲染目前会丢失大部分文字标签，只适合检查布局，不能用来截图。
- 两个服务都可以用 `RUST_LOG=debug` 打开详细日志，见 [设计说明中的诊断日志一节](DESIGN.md#按需开启诊断日志)。

## 持续集成

`.github/workflows/ci.yml` 在每次推送和 PR 上运行 `cargo fmt --check`、`clippy -D warnings`、
完整测试（xvfb 下跑 GTK 测试）、Nautilus 扩展测试和 `cargo audit`，并把 deb 作为构建产物上传。

发布新版本：

1. 修改 `Cargo.toml` 里的 `version`，提交。
2. 打与之一致的 tag 并推送：`git tag v0.1.7 && git push origin v0.1.7`。
3. CI 校验 tag 与版本号一致后，自动创建 GitHub Release，附上 deb 与 `SHA256SUMS`。

要让发布的 deb 开箱即可登录，在仓库 Settings → Secrets and variables → Actions →
**Variables** 中添加 `GONEDRIVE_CLIENT_ID`（它是公开配置，不是机密，因此用 Variable 而不是 Secret）。
