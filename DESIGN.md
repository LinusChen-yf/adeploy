# ADeploy - 通用部署工具设计文档

## 项目概述

ADeploy 是一个使用 Rust 开发的通用部署工具，支持跨平台、跨语言的应用程序部署。通过 gRPC 通信和 Rhai 脚本配置，提供灵活、安全的部署解决方案。程序由并且由Tokio 驱动，支持异步部署。

## 核心特性

1. **跨平台支持**: 支持 Windows、Linux、macOS 等主流操作系统
2. **语言无关**: 可部署任何语言编写的应用程序
3. **灵活配置**: 使用 Rhai 脚本语言进行配置，支持复杂的部署逻辑
4. **安全传输**: 基于 SSH 密钥的身份验证和加密传输
5. **统一工具**: 单一二进制文件同时支持客户端和服务端模式

## 系统架构

### 整体架构图

```
┌─────────────────┐         gRPC/TLS         ┌─────────────────┐
│   Client Side   │◄─────────────────────────►│   Server Side   │
├─────────────────┤                          ├─────────────────┤
│ • adeploy.rhai  │                          │ • config.rhai   │
│ • SSH Key       │                          │ • Deploy Scripts│
│ • File Scanner  │                          │ • Backup System │
│ • gRPC Client   │                          │ • gRPC Server   │
└─────────────────┘                          └─────────────────┘
```

### 核心组件

#### 1. 客户端组件
- **配置解析器**: 解析 `adeploy.rhai` 配置文件
- **文件扫描器**: 扫描并打包需要部署的文件
- **gRPC 客户端**: 与服务端通信
- **SSH 认证**: 处理 SSH 密钥认证

#### 2. 服务端组件
- **gRPC 服务器**: 接收部署请求
- **配置管理器**: 管理 `config.rhai` 配置文件
- **部署执行器**: 执行部署前后脚本
- **备份管理器**: 可选的备份功能
- **安全验证器**: SSH 密钥验证

## 配置文件设计

### 客户端配置 (adeploy.rhai)

```rhai
// Package configuration
let package = #{
    name: "my-web-app",
    version: "1.0.0",
    path: "./dist",
    exclude: ["*.log", "node_modules", ".git"],
    include_hidden: false
};

// Server configuration
let server = #{
    host: "192.168.50.11",
    port: 6060,
    ssh_key_path: "~/.ssh/id_rsa",
    timeout: 30
};

// Export configuration
package
```

### 服务端配置 (config.rhai)

```rhai
// Define packages and their deployment configurations
let packages = #{
    "my-web-app": #{
        deploy_path: "/var/www/my-web-app",
        pre_deploy_script: "./scripts/pre_deploy.sh",
        post_deploy_script: "./scripts/post_deploy.sh",
        backup_enabled: true,
        backup_path: "/var/backups/my-web-app",
        owner: "www-data",
        permissions: "755"
    },
    "api-service": #{
        deploy_path: "/opt/api-service",
        pre_deploy_script: "./scripts/stop_service.sh",
        post_deploy_script: "./scripts/start_service.sh",
        backup_enabled: false
    }
};

// Server settings
let server = #{
    port: 6060,
    max_file_size: 100 * 1024 * 1024, // 100MB
    allowed_ssh_keys: [
        "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAB...",
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI..."
    ]
};

packages
```

## gRPC 服务接口设计

### Protocol Buffers 定义

```protobuf
syntax = "proto3";

package adeploy;

// Deploy service definition
service DeployService {
    rpc Deploy(DeployRequest) returns (DeployResponse);
    rpc GetStatus(StatusRequest) returns (StatusResponse);
    rpc ListPackages(ListPackagesRequest) returns (ListPackagesResponse);
}

// Deploy request message
message DeployRequest {
    string package_name = 1;
    string version = 2;
    bytes file_data = 3;
    string ssh_signature = 4;
    string client_public_key = 5;
    map<string, string> metadata = 6;
}

// Deploy response message
message DeployResponse {
    bool success = 1;
    string message = 2;
    string deploy_id = 3;
    repeated string logs = 4;
}

// Status request message
message StatusRequest {
    string deploy_id = 1;
}

// Status response message
message StatusResponse {
    enum DeployStatus {
        PENDING = 0;
        RUNNING = 1;
        SUCCESS = 2;
        FAILED = 3;
    }
    DeployStatus status = 1;
    string message = 2;
    repeated string logs = 3;
}

// List packages request
message ListPackagesRequest {}

// List packages response
message ListPackagesResponse {
    repeated PackageInfo packages = 1;
}

// Package information
message PackageInfo {
    string name = 1;
    string deploy_path = 2;
    bool backup_enabled = 3;
    string last_deploy_time = 4;
    string version = 5;
}
```

## 安全机制设计

### SSH 密钥认证流程

1. **密钥生成**: 客户端生成 SSH 密钥对
2. **公钥注册**: 将公钥添加到服务端配置文件
3. **签名验证**: 客户端使用私钥对请求进行签名
4. **服务端验证**: 服务端使用公钥验证签名

### 传输安全

- 使用 TLS 1.3 加密 gRPC 通信
- 支持双向 TLS 认证
- 文件传输使用流式传输，支持大文件

## 部署流程设计

### 客户端流程

```
1. 读取 adeploy.rhai 配置
2. 扫描并打包文件
3. 生成 SSH 签名
4. 发送 gRPC 请求
5. 等待部署结果
6. 显示部署日志
```

### 服务端流程

```
1. 验证 SSH 签名
2. 检查包配置
3. 创建备份（如果启用）
4. 执行 pre_deploy 脚本
5. 解压并部署文件
6. 设置文件权限
7. 执行 post_deploy 脚本
8. 返回部署结果
```

## 命令行接口设计

### 客户端使用方式

```bash
# 基本部署
./adeploy 192.168.50.11

# 指定端口
./adeploy 192.168.50.11 --port 8080

# 指定配置文件
./adeploy 192.168.50.11 --config ./custom.rhai

# 查看状态
./adeploy 192.168.50.11 --status deploy-id-123

# 列出服务端包
./adeploy 192.168.50.11 --list
```

### 服务端使用方式

```bash
# 启动服务端
./adeploy server

# 指定端口
./adeploy server --port 8080

# 启用备份
./adeploy server --backup

# 指定配置文件
./adeploy server --config ./server-config.rhai

# 后台运行
./adeploy server --daemon
```

## 错误处理和日志

### 错误类型

- **配置错误**: 配置文件格式错误、缺失必要字段
- **网络错误**: 连接超时、网络不可达
- **认证错误**: SSH 密钥验证失败
- **部署错误**: 脚本执行失败、文件权限问题
- **系统错误**: 磁盘空间不足、内存不足

### 日志系统设计 (基于 log2)

#### 日志级别

- **ERROR**: 严重错误，导致操作失败
- **WARN**: 警告信息，操作可能受影响
- **INFO**: 一般信息，记录重要操作
- **DEBUG**: 调试信息，详细的执行过程

#### 日志配置

使用 log2 库进行日志管理，支持以下特性：

**客户端日志配置**:
```rust
// Initialize logger for client (console output only)
log2::stderr()
    .level(log::Level::Info)
    .start();
```

**服务端日志配置**:
```rust
// Initialize logger for server with file output
log2::open("./logs/server.log")
    .size_limit(10*1024*1024) // 10MB
    .rotate(5) // Keep 5 backup files
    .level(log::Level::Debug)
    .tee(true) // Output to both console and file
    .start();
```

#### 日志格式

标准日志格式包含以下信息：
- 时间戳 (ISO 8601 格式)
- 日志级别
- 模块名称
- 消息内容
- 可选的上下文信息 (deploy_id, package_name 等)

示例日志输出：
```
2024-01-15T10:30:45.123Z [INFO] adeploy::server - Starting deploy server on port 6060
2024-01-15T10:31:02.456Z [INFO] adeploy::deploy - Received deploy request for package 'my-web-app' (deploy_id: abc123)
2024-01-15T10:31:03.789Z [DEBUG] adeploy::auth - SSH key validation successful for client
2024-01-15T10:31:05.012Z [ERROR] adeploy::script - Pre-deploy script failed: exit code 1
```

#### 服务端日志文件管理

服务端在程序目录下创建 `logs/` 目录存储日志文件：

```
adeploy/
├── adeploy (binary)
├── config.rhai
├── logs/
│   ├── adeploy-server.log (当前日志)
│   ├── adeploy-server.log.1 (备份1)
│   ├── adeploy-server.log.2 (备份2)
│   └── ...
└── scripts/
```

#### 日志轮转策略

- **大小限制**: 单个日志文件最大 10MB
- **备份数量**: 保留最近 5 个备份文件
- **压缩**: 自动压缩旧的日志文件 (可选)
- **清理**: 定期清理超过保留期限的日志文件

#### 结构化日志

对于重要事件，使用结构化日志记录：

```rust
use log::{info, error};

// Deploy start event
info!("deploy_start"; 
    "deploy_id" => deploy_id,
    "package_name" => package_name,
    "client_ip" => client_ip,
    "file_size" => file_size
);

// Deploy completion event
info!("deploy_complete";
    "deploy_id" => deploy_id,
    "duration_ms" => duration.as_millis(),
    "status" => "success"
);

// Error event
error!("deploy_failed";
    "deploy_id" => deploy_id,
    "error_type" => "script_execution",
    "error_message" => error_msg
);
```

## 性能优化

### 文件传输优化

- 使用流式传输处理大文件
- 支持文件压缩（gzip）
- 增量传输（基于文件哈希）
- 并发传输多个小文件

### 内存管理

- 使用内存映射处理大文件
- 流式处理避免内存溢出
- 及时释放临时资源

## 扩展性设计

### 插件系统

- 支持自定义部署插件
- 插件接口标准化
- 动态加载插件

### 多服务端支持

- 支持部署到多个服务端
- 负载均衡和故障转移
- 集群管理功能

## 开发任务分解

### Phase 1: 核心框架 (2-3 周)

1. **项目初始化**
   - 创建 Cargo 项目结构
   - 配置依赖项 (tonic, rhai, clap, tokio, log2)
   - 设置 CI/CD 流程

2. **gRPC 服务定义**
   - 编写 .proto 文件
   - 生成 Rust 代码
   - 实现基础服务接口

3. **配置文件解析**
   - 实现 Rhai 配置解析器
   - 定义配置数据结构
   - 添加配置验证逻辑

### Phase 2: 核心功能 (3-4 周)

4. **文件处理模块**
   - 实现文件扫描和打包
   - 支持文件过滤和排除
   - 添加文件压缩功能

5. **SSH 认证模块**
   - 实现 SSH 密钥生成和管理
   - 添加签名和验证逻辑
   - 集成到 gRPC 服务中

6. **部署执行器**
   - 实现脚本执行功能
   - 添加进程管理和监控
   - 实现日志收集和输出

### Phase 3: 高级功能 (2-3 周)

7. **备份系统**
   - 实现文件备份功能
   - 支持增量备份
   - 添加备份清理策略

8. **错误处理和日志**
   - 完善错误处理机制
   - 集成 log2 库实现日志记录
   - 配置服务端日志文件存储
   - 实现结构化日志和日志轮转功能

9. **性能优化**
   - 优化文件传输性能
   - 实现并发处理
   - 添加内存管理优化

### Phase 4: 测试和文档 (1-2 周)

10. **单元测试**
    - 编写核心模块测试
    - 添加集成测试
    - 实现端到端测试

11. **文档编写**
    - 编写用户手册
    - 创建 API 文档
    - 添加示例和教程

12. **打包和发布**
    - 配置跨平台编译
    - 创建安装包
    - 设置发布流程

## 技术栈

### 核心依赖

- **tonic**: gRPC 框架
- **rhai**: 脚本语言引擎
- **clap**: 命令行参数解析
- **tokio**: 异步运行时
- **serde**: 序列化/反序列化
- **log2**: 日志记录和文件输出

### 辅助依赖

- **ssh2**: SSH 客户端功能
- **tar**: 文件打包
- **flate2**: 文件压缩
- **uuid**: 唯一标识符生成
- **chrono**: 时间处理

## 部署和运维

### 系统要求

- **操作系统**: Linux, macOS, Windows
- **内存**: 最小 64MB，推荐 256MB
- **磁盘**: 最小 10MB，根据部署文件大小调整
- **网络**: TCP 端口访问权限

### 监控和维护

- 部署日志监控
- 系统资源监控
- 错误告警机制
- 定期备份清理

## 总结

ADeploy 设计为一个功能完整、安全可靠的通用部署工具。通过模块化设计和清晰的接口定义，确保系统的可维护性和可扩展性。分阶段的开发计划有助于逐步实现所有功能，并在每个阶段进行充分的测试和验证。