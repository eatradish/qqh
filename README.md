# qqh (悄悄话)

qqh 是一个基于 Rust 开发的极简个人碎碎念/消息记录系统。它支持 Web 端展示，并提供了一个便捷的命令行工具，让你能像提交代码一样快速发布想法。

## 🚀 快速上手
1. 准备配置

在程序根目录创建 config.toml:

```toml
title = "我的悄悄话"
url = "127.0.0.1:3000"
db_path = "data.redb"
page_content = 10       # 每页显示条数
split_length = 100      # 首页预览字符长度
push_password = "你的秘钥"
```

2. 启动服务端
```Bash
cargo run -- serve
```

3. 发布内容

你可以通过以下两种方式发布消息：

直接输入：

```
cargo run -- push "今天天气不错"
```

通过标准输入 (Stdin)：
```Bash
cat draft.txt | cargo run -- push
```

## 📖 路由说明

| 路径 | 方法 |
| :--- | :--- | 
| GET /	| 浏览所有条目 |
| POST / | 发布一条消息 |
| GET /{id}	| 获取某条的详情 |

## 🏗️ 编译与部署

### 编译 Release 版本

```bash
cargo build --release
```

### 运行

```bash
./target/release/qqh --config-path ./config.toml serve
```
