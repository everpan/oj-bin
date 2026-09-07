# CI（plugin-matrix 的 windows 行）专用工具链，经 CMAKE_TOOLCHAIN_FILE 环境变量注入
# （cmake crate 会读取该环境变量）。当前工作区唯一的 cmake 工程是 rdkafka-sys 源码
# 编译的 librdkafka：其默认按 /MD（动态 CRT）编译，而 rustc 在 windows-msvc 上默认
# 静态 CRT（libcmt），混链会产生 LNK4098/LNK4217/LNK4286 警告并留下悬空的
# __imp__* 符号 → LNK1120。此处强制全树 /MT，与 rustc 及 V8 静态库保持一致。
set(CMAKE_POLICY_DEFAULT_CMP0091 NEW CACHE STRING "" FORCE)
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded" CACHE STRING "" FORCE)
