# oj-jdbc-bridge

泛型 JDBC 插件的 Java sidecar 桥：stdin/stdout 走 NDJSON 帧协议（见
`docs/superpowers/specs/2026-11-20-jdbc-generic-plugin-design.md` §5.3）。

## 构建

```bash
./gradlew test        # 单元测试（H2 真绑）
./gradlew shadowJar   # 产物固定名 build/libs/oj-jdbc-bridge.jar
```

运行：`java -jar oj-jdbc-bridge.jar <driversDir>`（driversDir 缺省 `./drivers`，
目录下全部 `*.jar` 由 URLClassLoader 装载并设为 TCCL）。启动先打印一行
`{"hello":{"proto":1,"bridge":"0.1.0"}}`；EOF 后有序退出（0）。

## Gradle wrapper 校验

`gradle/wrapper/gradle-wrapper.jar` 已提交入库。重新生成后核对 sha256：

```
621008afc4f5d59357fcf06ffa644dd5aade5e091bdc363742710445f73cdaf8  gradle/wrapper/gradle-wrapper.jar
```

distributionUrl 钉死 `gradle-9.9.2-bin`（不使用 latest/动态版本）。
