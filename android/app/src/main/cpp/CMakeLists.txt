# Rebleach Android native 层：CMake 只做一件事——把 cargo-ndk 产出的
# librebleach_client.so 直接打包进 APK，不做任何 C/C++ 编译。
#
# 真实的 Rust 交叉编译由 M0 阶段单独执行：
#   cargo ndk --target aarch64-linux-android --android-platform 31 \
#            -p rebleach-client build --release --no-default-features --features android
# 产出的 .so 放到 client/target/aarch64-linux-android/release/librebleach_client.so
# 然后 gradle task copyRustLibs 把它拷到 app/src/main/jniLibs/arm64-v8a/
#
# 为什么不用 cmake externalNativeBuild 直接编译 Rust：
#   cargo-ndk 依赖 cargo build，而 CMake 的 externalNativeBuild 会等 .so 生成再 return，
#   用一个简单的 add_custom_command 更可靠、也方便分离 Android 构建和 Rust 构建的耗时。

cmake_minimum_required(VERSION 3.22.1)
project(rebleach_jni)

add_library(rebleach_client SHARED IMPORTED GLOBAL)

# 真实的 .so 由 gradle copyRustLibs 任务放到这里，
# 先留个占位（构建时如果不存在会报错，这是预期的——需要先跑 cargo ndk）
set_target_properties(rebleach_client PROPERTIES
    IMPORTED_LOCATION "${CMAKE_SOURCE_DIR}/../../../jniLibs/${ANDROID_ABI}/librebleach_client.so"
)
