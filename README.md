# Codex 额度面板

codex-proxy-rs 的独立管理页面插件，展示 OpenAI Codex OAuth 账号的被动额度、套餐和令牌中的套餐到期时间。额度条表示**剩余百分比**，用量越高，色条越短。

页面每 30 秒读取一次宿主已有数据，不主动请求上游。套餐到期时间来自宿主保存的 ID Token，可能与实时订阅信息不同；缺少该声明时显示“未知”。插件从不把原始令牌返回给浏览器。

## 安装

本仓库每次推送 `main` 后，GitHub Actions 会构建 Linux amd64 安装包并发布为最新正式 Release。在 codex-proxy-rs 插件管理中选择 GitHub 来源，填写 `BlackCatCmx/cpr-plugin-cx-panel`，留空标签即可选择最新正式版。

插件声明 `accounts` 和 `data` 两项权限：前者读取账号名称、套餐及 ID Token，后者只读已有额度。`accounts` 在宿主中属于较宽的账号权限，安装时请核对来源和权限摘要。

当前按 codex-proxy-rs v3.15.2 的 SDK 和打包工具构建。后续宿主升级时，若插件仍兼容可继续使用；若合同变化，再更新插件依赖和代码。
